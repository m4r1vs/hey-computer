//! The conductor: multi-lane coordinator that owns the ACP agent process and
//! runs one interactive lane alongside a bounded FIFO background task lane.
//!
//! All concurrent turns share one warm `acp::Connection` through cloneable
//! `acp::Client` handles and per-`sessionId` subscriptions, while lifecycle
//! transitions (start, restart, idle stop, follow-up tracking, queueing, and
//! shutdown) stay serialized inside `Conductor::run`.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::acp::{
    self, CallError, Client as AcpClient, Connection, SessionEvent, SessionSubscription,
};
use crate::attachments::{self, LoadedAttachment};
use crate::config::{Config, MAX_RESTART_DELAY_SECS, ResolvedProfile, secs};
use crate::context::{self, ContextProvider, SystemContext};
use crate::environment::{self, Paths};
use crate::policy::{LanePolicy, PermissionDecision};
use crate::present::{Presentation, Presenter, Urgency};
use crate::protocol::{AgentState, FailReason, InputSource, Reply, Status};
use crate::routing::{self, HeuristicResult, LaneChoice, RouteDecision, TierChoice};
use crate::store::{self, Lane, NewTurn, Store, TurnStatus};

/// initialize and session/new answer in about a second; this only catches an
/// agent that hangs on start.
const START_TIMEOUT: Duration = Duration::from_secs(60);
const CONFIG_TIMEOUT: Duration = Duration::from_secs(10);
/// agy answers session/cancel within about 10 ms; an agent that has not
/// finished the turn after this long is treated as wedged and restarted.
const CANCEL_GRACE: Duration = Duration::from_secs(3);
/// Restarts back off up to this.
const MAX_RESTART_DELAY: Duration = Duration::from_secs(MAX_RESTART_DELAY_SECS as u64);
/// An agent that lived this long before dying starts the back-off afresh.
const HEALTHY_RUN: Duration = Duration::from_secs(60);

/// Where a request's result goes.
pub enum Sink {
    /// The daemon shows it.
    Present,
    /// A waiting client gets it, and nothing is shown.
    Client(oneshot::Sender<Reply>),
}

/// Parameters submitted to [`Handle::ask`].
pub struct AskParams {
    pub text: String,
    pub new: bool,
    pub profile: Option<String>,
    pub background: bool,
    pub suppress_token: Option<String>,
    pub attachments: Vec<LoadedAttachment>,
    pub source: InputSource,
}

/// What became of a request.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Answer {
        text: String,
        denied: Vec<String>,
        suppressed: bool,
    },
    Failed {
        reason: FailReason,
        message: String,
    },
}

impl Outcome {
    fn failed(reason: FailReason, message: impl Into<String>) -> Self {
        Self::Failed {
            reason,
            message: message.into(),
        }
    }

    pub fn into_reply(self, id: u64) -> Reply {
        match self {
            Self::Answer {
                text,
                denied,
                suppressed,
            } => Reply::Answer {
                id,
                text,
                denied,
                suppressed,
            },
            Self::Failed { reason, message } => Reply::Failed {
                id,
                reason,
                message,
            },
        }
    }
}

/// Cancellation of one request, set from outside the conductor.
#[derive(Default)]
pub struct CancelToken {
    state: Mutex<TokenState>,
    notify: Notify,
}

#[derive(Default)]
struct TokenState {
    reason: Option<FailReason>,
    finished: bool,
}

impl CancelToken {
    /// Cancel unless already cancelled or finished; true if this did it.
    pub fn cancel(&self, reason: FailReason) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.finished || state.reason.is_some() {
            return false;
        }
        state.reason = Some(reason);
        drop(state);
        self.notify.notify_waiters();
        true
    }

    pub fn reason(&self) -> Option<FailReason> {
        self.state.lock().ok().and_then(|s| s.reason)
    }

    fn finish(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.finished = true;
        }
    }

    pub async fn cancelled(&self) -> FailReason {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(reason) = self.reason() {
                return reason;
            }
            notified.await;
        }
    }
}

pub struct Ask {
    pub id: u64,
    pub text: String,
    pub new: bool,
    pub profile: Option<String>,
    pub background: bool,
    pub suppress_token: Option<String>,
    pub attachments: Vec<LoadedAttachment>,
    pub source: InputSource,
    pub auto_routed_fast: bool,
    pub escalated_once: bool,
    pub sink: Sink,
    pub token: Arc<CancelToken>,
}

enum Message {
    Ask(Ask),
    Wake,
    Shutdown,
}

/// What the socket handlers can see and do without waiting for the conductor.
struct Shared {
    next_id: u64,
    /// The newest interactive request, which is the only interactive one that
    /// may still run.
    fg_latest: Option<(u64, Arc<CancelToken>)>,
    /// Active or queued background tasks indexed by task id.
    bg_tasks: HashMap<u64, Arc<CancelToken>>,
    bg_running: usize,
    bg_queued: usize,
    agent: AgentState,
    /// Currently running interactive request `(id, started, profile)`.
    running: Option<(u64, Instant, Option<String>)>,
    follow_up_until: Option<Instant>,
    handled: u64,
}

#[derive(Clone)]
pub struct Handle {
    config: Arc<Config>,
    shared: Arc<Mutex<Shared>>,
    messages: mpsc::UnboundedSender<Message>,
    /// Set at shutdown; interrupts whatever the conductor is waiting on.
    stop: Arc<CancelToken>,
    store: Option<Store>,
}

impl Handle {
    /// Validate a requested profile name against the daemon's configuration
    /// before queuing a request (so an unknown profile never supersedes an
    /// active foreground turn).
    pub fn validate_profile(&self, profile: Option<&str>) -> std::result::Result<(), String> {
        self.config.resolve_profile(profile).map(|_| ())
    }

    /// Queue an interactive or background request. Returns its monotonic id,
    /// or an error if the background queue is full.
    pub fn ask(&self, params: AskParams, sink: Sink) -> std::result::Result<u64, String> {
        let token = Arc::new(CancelToken::default());
        let lane = if params.background {
            Lane::Background
        } else {
            Lane::Interactive
        };
        let id = {
            let mut shared = self
                .shared
                .lock()
                .expect("the shared state is never poisoned");
            if params.background {
                if shared.bg_queued >= self.config.tasks.max_queued {
                    return Err(format!(
                        "too many background tasks queued (limit {})",
                        self.config.tasks.max_queued
                    ));
                }
                shared.next_id += 1;
                let id = shared.next_id;
                shared.bg_tasks.insert(id, Arc::clone(&token));
                shared.bg_queued += 1;
                id
            } else {
                shared.next_id += 1;
                let id = shared.next_id;
                if let Some((_, previous)) = shared.fg_latest.replace((id, Arc::clone(&token))) {
                    previous.cancel(FailReason::Superseded);
                }
                id
            }
        };
        let stored_profile = params
            .profile
            .as_deref()
            .or(if params.background {
                self.config.tasks.profile.as_deref()
            } else {
                None
            })
            .filter(|p| *p != "default")
            .unwrap_or("")
            .to_string();
        if let Some(store) = &self.store {
            store.create_turn(NewTurn {
                id,
                lane,
                status: TurnStatus::Queued,
                profile: stored_profile,
                created_at_ms: store::unix_timestamp_ms(),
                request: params.text.clone(),
                attachments: params
                    .attachments
                    .iter()
                    .map(LoadedAttachment::meta)
                    .collect(),
            });
        }
        let ask = Ask {
            id,
            text: params.text,
            new: params.new,
            profile: params.profile,
            background: params.background,
            suppress_token: params.suppress_token,
            attachments: params.attachments,
            source: params.source,
            auto_routed_fast: false,
            escalated_once: false,
            sink,
            token,
        };
        if let Err(mpsc::error::SendError(Message::Ask(ask))) =
            self.messages.send(Message::Ask(ask))
        {
            if let Ok(mut shared) = self.shared.lock()
                && ask.background
            {
                shared.bg_tasks.remove(&ask.id);
                shared.bg_queued = shared.bg_queued.saturating_sub(1);
            }
            if let Some(store) = &self.store {
                store.finish_failed(
                    id,
                    FailReason::AgentDown,
                    "the daemon is shutting down".to_string(),
                    store::unix_timestamp_ms(),
                );
            }
            deliver(
                None,
                ask,
                Outcome::failed(FailReason::AgentDown, "the daemon is shutting down"),
                None,
            );
        }
        Ok(id)
    }

    /// Cancel the active interactive request (`only == None`), or a specific
    /// interactive or background request (`only == Some(id)`).
    pub fn cancel(&self, only: Option<u64>, reason: FailReason) -> Option<u64> {
        let cancelled = {
            let shared = self.shared.lock().ok()?;
            match only {
                None => {
                    let (id, token) = shared.fg_latest.as_ref()?;
                    token.cancel(reason).then_some(*id)
                }
                Some(target) => {
                    if let Some((id, token)) = shared.fg_latest.as_ref()
                        && *id == target
                    {
                        token.cancel(reason).then_some(*id)
                    } else if let Some(token) = shared.bg_tasks.get(&target) {
                        token.cancel(reason).then_some(target)
                    } else {
                        None
                    }
                }
            }
        };
        if cancelled.is_some() {
            let _ = self.messages.send(Message::Wake);
        }
        cancelled
    }

    pub fn status(&self) -> Status {
        let shared = self
            .shared
            .lock()
            .expect("the shared state is never poisoned");
        let now = Instant::now();
        let waiting = shared
            .fg_latest
            .as_ref()
            .filter(|(_, token)| {
                token.reason().is_none() && !token.state.lock().map(|s| s.finished).unwrap_or(true)
            })
            .map(|(id, _)| *id);
        let (running, running_secs, running_profile) = match &shared.running {
            Some((id, since, profile)) => (
                Some(*id),
                Some(since.elapsed().as_secs_f64()),
                profile.clone(),
            ),
            None => (waiting, None, None),
        };
        Status {
            agent: shared.agent,
            running,
            running_secs,
            running_profile,
            follow_up_secs: shared
                .follow_up_until
                .filter(|until| *until > now)
                .map(|until| (until - now).as_secs_f64()),
            handled: shared.handled,
            background_running: shared.bg_running,
            background_queued: shared.bg_queued,
        }
    }

    /// Stop the conductor and the agent; waits for neither.
    pub fn shutdown(&self) {
        self.stop.cancel(FailReason::AgentDown);
        let _ = self.messages.send(Message::Shutdown);
    }
}

/// Start the conductor task.
pub fn spawn(config: Arc<Config>, presenter: Arc<dyn Presenter>) -> (Handle, JoinHandle<()>) {
    spawn_with_context(config, presenter, Arc::new(SystemContext))
}

/// Start the conductor task with a custom ambient context provider (for tests).
pub fn spawn_with_context(
    config: Arc<Config>,
    presenter: Arc<dyn Presenter>,
    context: Arc<dyn ContextProvider>,
) -> (Handle, JoinHandle<()>) {
    spawn_with_store(config, presenter, context, None)
}

/// Start the conductor task wired to a persistent SQLite [`Store`].
pub fn spawn_with_store(
    config: Arc<Config>,
    presenter: Arc<dyn Presenter>,
    context: Arc<dyn ContextProvider>,
    store: Option<(Store, u64)>,
) -> (Handle, JoinHandle<()>) {
    let (store, initial_last_id) = match store {
        Some((s, id)) => (Some(s), id),
        None => (None, 0),
    };
    let shared = Arc::new(Mutex::new(Shared {
        next_id: initial_last_id,
        fg_latest: None,
        bg_tasks: HashMap::new(),
        bg_running: 0,
        bg_queued: 0,
        agent: AgentState::Down,
        running: None,
        follow_up_until: None,
        handled: 0,
    }));
    let (messages, inbox) = mpsc::unbounded_channel();
    let (done_tx, done_rx) = mpsc::unbounded_channel();
    let stop = Arc::new(CancelToken::default());
    let conductor = Conductor {
        policy: Arc::new(LanePolicy::standard(&config.permissions.deny)),
        config: Arc::clone(&config),
        presenter,
        context,
        store: store.clone(),
        shared: Arc::clone(&shared),
        inbox,
        done_tx,
        done_rx,
        stop: Arc::clone(&stop),
        live: None,
        paths: None,
        conversation: None,
        fg_active: None,
        fg_pending: None,
        bg_active: HashSet::new(),
        bg_queue: VecDeque::new(),
        failures: 0,
        restart_at: None,
        last_used: Instant::now(),
        last_death_detail: None,
    };
    let task = tokio::spawn(conductor.run());
    (
        Handle {
            config,
            shared,
            messages,
            stop,
            store,
        },
        task,
    )
}

/// A pre-opened session configured for a specific `(model, thought_level)`.
/// MCP server allowlists are enforced per turn by `LanePolicy`.
struct SpareSession {
    session: String,
    model: String,
    thought_level: String,
}

/// A running agent process shared by concurrent lanes.
struct Live {
    conn: Connection,
    exited: Option<oneshot::Receiver<()>>,
    started: Instant,
    /// A session made ahead of time for the default interactive profile.
    spare: Option<SpareSession>,
    /// Initial `(model, thought_level)` reported by the agent's first `session/new`.
    agent_defaults: (String, String),
    /// Set when one session wedged on cancel while sibling lanes are still active;
    /// stop and restart the agent once active turns drain.
    draining: bool,
    /// Set on `auth_required`; stop the agent (without auto-restart) once idle.
    auth_failed: bool,
}

struct Conversation {
    session: String,
    /// First turn id in this conversation (for threading in history.db).
    conversation_id: u64,
    profile_name: Option<String>,
    model: String,
    thought_level: String,
    /// When the last turn ended; follow-ups are measured from here.
    last: Instant,
    turns: u32,
}

struct TurnFinished {
    ask: Ask,
    session: String,
    used_profile: Option<String>,
    started: Instant,
    end: TurnEnd,
}

struct Conductor {
    config: Arc<Config>,
    presenter: Arc<dyn Presenter>,
    context: Arc<dyn ContextProvider>,
    store: Option<Store>,
    policy: Arc<LanePolicy>,
    shared: Arc<Mutex<Shared>>,
    inbox: mpsc::UnboundedReceiver<Message>,
    done_tx: mpsc::UnboundedSender<TurnFinished>,
    done_rx: mpsc::UnboundedReceiver<TurnFinished>,
    stop: Arc<CancelToken>,
    live: Option<Live>,
    paths: Option<Paths>,
    conversation: Option<Conversation>,
    fg_active: Option<u64>,
    fg_pending: Option<Ask>,
    bg_active: HashSet<u64>,
    bg_queue: VecDeque<Ask>,
    /// Agent deaths in a row, for the restart back-off.
    failures: u32,
    restart_at: Option<Instant>,
    last_used: Instant,
    /// Last reaped exit detail from `agent_died`, shared across any turns that
    /// failed on the same agent crash.
    last_death_detail: Option<String>,
}

/// How one turn ended.
struct TurnEnd {
    outcome: Outcome,
    /// False when the agent stdout closed mid-turn.
    alive: bool,
    /// True when `session/cancel` timed out and the session is wedged.
    wedged: bool,
    /// The session should not be continued (for example, the agent refused
    /// the prompt, timed out, or hit a per-turn budget).
    broken: bool,
    /// The agent rejected the turn for lack of a login and must be restarted
    /// on the next request to re-read the keyring.
    auth_required: bool,
}

enum Step {
    Responded(Result<Result<Value, CallError>, oneshot::error::RecvError>),
    Event(Option<SessionEvent>),
    Quiet,
    TurnTimeout,
    Cancelled(FailReason),
}

#[derive(Clone, Copy)]
struct Budgets {
    inactivity: Duration,
    max_turn: Option<Duration>,
    max_tool_calls: Option<usize>,
    max_repeat_tool_calls: Option<usize>,
}

struct TurnContext<'a> {
    policy: &'a LanePolicy,
    budgets: Budgets,
    progress: Option<&'a Arc<dyn Presenter>>,
    id: u64,
}

enum TurnAbort {
    Quiet,
    TurnTimeout(Duration),
    ToolLimit(usize),
    RepeatToolLoop { count: usize, tool: String },
    Cancelled(FailReason),
}

/// Counts distinct tool calls in a single turn without double-counting
/// repeated `tool_call` updates or the matching `session/request_permission`,
/// and detects consecutive identical tool-call retry loops.
#[derive(Default)]
struct ToolTracker {
    seen: HashSet<String>,
    count: usize,
    last_id: Option<String>,
    last_sig: Option<(String, String)>,
    last_had_args: bool,
    prev_sig: Option<(String, String)>,
    prev_streak: usize,
    streak: usize,
}

impl ToolTracker {
    fn record_update(&mut self, call: &Value) -> bool {
        let id = acp::tool_call_id(call);
        let (sig, has_args) = tool_signature(call);
        match id {
            Some(id) => {
                if self.seen.insert(id.to_string()) {
                    self.push_new(Some(id), sig, has_args);
                    true
                } else {
                    self.maybe_upgrade(id, sig, has_args);
                    false
                }
            }
            None => {
                self.push_new(None, sig, has_args);
                true
            }
        }
    }

    fn record_permission(&mut self, call: Option<&Value>) -> bool {
        let Some(call) = call else {
            return false;
        };
        let Some(id) = acp::tool_call_id(call) else {
            return false;
        };
        let (sig, has_args) = tool_signature(call);
        if self.seen.insert(id.to_string()) {
            self.push_new(Some(id), sig, has_args);
            true
        } else {
            self.maybe_upgrade(id, sig, has_args);
            false
        }
    }

    fn push_new(&mut self, id: Option<&str>, sig: (String, String), has_args: bool) {
        self.count += 1;
        self.prev_sig = self.last_sig.take();
        self.prev_streak = self.streak;
        if self.prev_sig.as_ref() == Some(&sig) {
            self.streak = self.prev_streak + 1;
        } else {
            self.streak = 1;
        }
        self.last_id = id.map(String::from);
        self.last_sig = Some(sig);
        self.last_had_args = has_args;
    }

    fn maybe_upgrade(&mut self, id: &str, sig: (String, String), has_args: bool) {
        if self.last_id.as_deref() == Some(id) && !self.last_had_args && has_args {
            if self.prev_sig.as_ref() == Some(&sig) {
                self.streak = self.prev_streak + 1;
            } else {
                self.streak = 1;
            }
            self.last_sig = Some(sig);
            self.last_had_args = true;
        }
    }

    fn count(&self) -> usize {
        self.count
    }

    fn repeat_streak(&self) -> usize {
        self.streak
    }

    fn last_tool_name(&self) -> String {
        self.last_sig
            .as_ref()
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| "tool".to_string())
    }
}

fn tool_signature(call: &Value) -> ((String, String), bool) {
    let raw_name = call
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let name = if raw_name == Some("call_mcp_tool") {
        match (acp::mcp_server_name(call), acp::mcp_tool_name(call)) {
            (Some(s), Some(t)) => format!("mcp:{s}/{t}"),
            (Some(s), None) => format!("mcp:{s}"),
            _ => "call_mcp_tool".to_string(),
        }
    } else if let Some(n) = raw_name {
        n.to_string()
    } else {
        call.get("kind")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("other")
            .to_string()
    };

    let input = ["rawInput", "args"]
        .into_iter()
        .find_map(|key| call.get(key).filter(|v| !is_empty_json(v)));
    let (args, has_args) = match input {
        Some(v) => (canonical_json(v), true),
        None => {
            if let Some(cmd) = acp::tool_command(call).filter(|c| !c.trim().is_empty()) {
                (canonical_json(&Value::String(cmd)), true)
            } else if let Some(title) = call
                .get("title")
                .and_then(Value::as_str)
                .filter(|t| !t.trim().is_empty())
            {
                (canonical_json(&Value::String(title.to_string())), false)
            } else {
                ("{}".to_string(), false)
            }
        }
    };
    ((name, args), has_args)
}

fn is_empty_json(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Object(map) => map.is_empty(),
        Value::Array(arr) => arr.is_empty(),
        Value::String(s) => s.is_empty(),
        _ => false,
    }
}

/// Deterministic JSON serialization with object keys sorted lexicographically.
fn canonical_json(value: &Value) -> String {
    fn write_canon(v: &Value, out: &mut String) {
        match v {
            Value::Object(map) => {
                let sorted: BTreeMap<&str, &Value> =
                    map.iter().map(|(k, v)| (k.as_str(), v)).collect();
                out.push('{');
                for (i, (k, val)) in sorted.into_iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    if let Ok(ks) = serde_json::to_string(k) {
                        out.push_str(&ks);
                    }
                    out.push(':');
                    write_canon(val, out);
                }
                out.push('}');
            }
            Value::Array(arr) => {
                out.push('[');
                for (i, val) in arr.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_canon(val, out);
                }
                out.push(']');
            }
            primitive => {
                if let Ok(s) = serde_json::to_string(primitive) {
                    out.push_str(&s);
                }
            }
        }
    }
    let mut out = String::new();
    write_canon(value, &mut out);
    out
}

/// Check whether a completed answer matches `suppress_token` (e.g.
/// `HEARTBEAT_OK`). Never suppresses when commands were refused.
pub fn is_suppressed(text: &str, token: &str, denied: &[String]) -> bool {
    if !denied.is_empty() {
        return false;
    }
    let target = token.trim();
    if target.is_empty() {
        return false;
    }
    let mut candidate = text.trim();
    if let Some(inner) = candidate
        .strip_prefix("```")
        .and_then(|rest| rest.strip_suffix("```"))
    {
        candidate = match inner.find('\n') {
            Some(nl) => inner[nl + 1..].trim(),
            None => inner.trim(),
        };
    } else if let Some(inner) = candidate
        .strip_prefix("**")
        .and_then(|rest| rest.strip_suffix("**"))
    {
        candidate = inner.trim();
    } else if let Some(inner) = candidate
        .strip_prefix('`')
        .and_then(|rest| rest.strip_suffix('`'))
    {
        candidate = inner.trim();
    }
    candidate = candidate.trim_end_matches(['.', '!']).trim();
    candidate == target
}

impl Conductor {
    async fn run(mut self) {
        let stop = Arc::clone(&self.stop);
        if self.config.agent.warm_start {
            // A start that hangs must not hold up a shutdown; dropping it
            // kills the half-started agent.
            tokio::select! {
                () = self.start_warm() => {}
                _ = stop.cancelled() => {}
            }
        }
        loop {
            let restart_at = self.restart_at;
            let idle_at = self.idle_deadline();
            if stop.reason().is_some() {
                break;
            }
            tokio::select! {
                // Keep a deterministic biased order: completions, then inbox,
                // then agent exit, then timers.
                biased;
                finished = self.done_rx.recv() => {
                    if let Some(finished) = finished {
                        self.handle_finished(finished).await;
                        self.dispatch_ready().await;
                    }
                }
                message = self.inbox.recv() => match message {
                    Some(Message::Ask(ask)) => {
                        self.enqueue(ask);
                        self.dispatch_ready().await;
                    }
                    Some(Message::Wake) => {
                        self.evict_cancelled_queued();
                        self.dispatch_ready().await;
                    }
                    Some(Message::Shutdown) | None => break,
                },
                () = wait_exited(&mut self.live) => {
                    if let Some(live) = self.live.take() {
                        let detail = self.agent_died(live).await;
                        self.last_death_detail = Some(detail);
                    }
                }
                () = sleep_until(restart_at) => {
                    self.restart_at = None;
                    tokio::select! {
                        () = self.start_warm() => {}
                        _ = stop.cancelled() => {}
                    }
                    self.dispatch_ready().await;
                }
                () = sleep_until(idle_at) => {
                    tracing::info!("stopping the idle agent");
                    self.stop_agent().await;
                }
            }
        }

        // Let active turns complete their cancel_turn and report outcomes.
        let shutdown_deadline = tokio::time::Instant::now() + CANCEL_GRACE;
        while self.has_active_turns() {
            tokio::select! {
                finished = self.done_rx.recv() => match finished {
                    Some(finished) => self.handle_finished(finished).await,
                    None => break,
                },
                () = tokio::time::sleep_until(shutdown_deadline) => break,
            }
        }

        // Drain anything still waiting in queues or the inbox.
        if let Some(ask) = self.fg_pending.take() {
            let profile = self.effective_display_profile(&ask);
            self.finish_request(ask, cancelled(FailReason::AgentDown), profile, false);
        }
        while let Some(ask) = self.bg_queue.pop_front() {
            let profile = self.effective_display_profile(&ask);
            self.finish_request(ask, cancelled(FailReason::AgentDown), profile, true);
        }
        while let Ok(message) = self.inbox.try_recv() {
            if let Message::Ask(ask) = message {
                let profile = self.effective_display_profile(&ask);
                let was_queued_bg = ask.background;
                self.finish_request(
                    ask,
                    cancelled(FailReason::AgentDown),
                    profile,
                    was_queued_bg,
                );
            }
        }
        self.stop_agent().await;
    }

    fn has_active_turns(&self) -> bool {
        self.fg_active.is_some() || !self.bg_active.is_empty()
    }

    fn enqueue(&mut self, ask: Ask) {
        if ask.background {
            self.bg_queue.push_back(ask);
        } else {
            if let Some(old) = self.fg_pending.replace(ask) {
                let reason = old.token.reason().unwrap_or(FailReason::Superseded);
                let profile = self.effective_display_profile(&old);
                self.finish_request(old, cancelled(reason), profile, false);
            }
        }
    }

    fn evict_cancelled_queued(&mut self) {
        if let Some(pending) = &self.fg_pending
            && let Some(reason) = pending.token.reason()
        {
            let ask = self.fg_pending.take().expect("checked");
            let profile = self.effective_display_profile(&ask);
            self.finish_request(ask, cancelled(reason), profile, false);
        }
        let mut remaining = VecDeque::with_capacity(self.bg_queue.len());
        while let Some(ask) = self.bg_queue.pop_front() {
            if let Some(reason) = ask.token.reason() {
                let profile = self.effective_display_profile(&ask);
                self.finish_request(ask, cancelled(reason), profile, true);
            } else {
                remaining.push_back(ask);
            }
        }
        self.bg_queue = remaining;
    }

    async fn dispatch_ready(&mut self) {
        self.evict_cancelled_queued();
        if self.stop.reason().is_some() {
            return;
        }
        // Finish draining a wedged or unauthenticated agent once all sibling
        // turns have completed.
        if let Some(live) = &self.live
            && (live.draining || live.auth_failed)
        {
            if self.has_active_turns() {
                return;
            }
            let auth_failed = live.auth_failed;
            self.stop_agent().await;
            if !auth_failed {
                self.schedule_restart_if_warm();
            }
        }

        loop {
            self.evict_cancelled_queued();
            if self.stop.reason().is_some() {
                return;
            }
            let can_fg = self.fg_active.is_none() && self.fg_pending.is_some();
            let can_bg = self.bg_active.len() < self.config.tasks.max_concurrent
                && !self.bg_queue.is_empty();
            if !can_fg && !can_bg {
                return;
            }

            if can_fg {
                let ask = self.fg_pending.take().expect("checked");
                self.launch_ask(ask).await;
            } else if can_bg {
                let ask = self.bg_queue.pop_front().expect("checked");
                if let Ok(mut shared) = self.shared.lock() {
                    shared.bg_queued = shared.bg_queued.saturating_sub(1);
                }
                self.launch_ask(ask).await;
            }

            // If a turn aborted early or drained the agent, re-evaluate loop condition.
            if self
                .live
                .as_ref()
                .is_some_and(|l| l.draining || l.auth_failed)
            {
                return;
            }
        }
    }

    async fn launch_ask(&mut self, mut ask: Ask) {
        if let Some(reason) = ask.token.reason() {
            let display_profile = self.effective_display_profile(&ask);
            self.finish_request(ask, cancelled(reason), display_profile, false);
            return;
        }

        // Three-tier smart routing (runs only when enabled and caller did not
        // pass explicit --profile or --background).
        if self.config.routing.enabled
            && ask.profile.is_none()
            && !ask.background
            && !ask.escalated_once
        {
            if let Some(reason) = self.apply_routing(&mut ask).await {
                let display_profile = self.effective_display_profile(&ask);
                self.finish_request(ask, cancelled(reason), display_profile, false);
                return;
            }
            if ask.background {
                // Promoted to background lane: hand off to bg_queue and let
                // dispatch_ready launch it without blocking interactive asks.
                let preview = attachments::progress_label(&ask.text, &ask.attachments);
                self.presenter.present(Presentation::Notice {
                    summary: "Working in the background".to_string(),
                    body: preview,
                    urgency: Urgency::Low,
                });
                self.bg_queue.push_back(ask);
                return;
            }
        }

        let display_profile = self.effective_display_profile(&ask);
        if let Some(reason) = ask.token.reason() {
            self.finish_request(ask, cancelled(reason), display_profile, false);
            return;
        }

        if !ask.background {
            if let Ok(mut shared) = self.shared.lock() {
                shared.running = Some((ask.id, Instant::now(), display_profile.clone()));
            }
            if matches!(ask.sink, Sink::Present) {
                let thinking_label = display_profile
                    .as_deref()
                    .and_then(|name| self.config.profiles.get(name))
                    .and_then(|p| p.thinking_label.as_deref())
                    .map(|s| s.trim().to_string());
                self.presenter.present(Presentation::Progress {
                    id: ask.id,
                    request: attachments::progress_label(&ask.text, &ask.attachments),
                    profile: display_profile.clone(),
                    thinking_label,
                });
            }
        }

        let started = Instant::now();
        let attach_summary = if ask.attachments.is_empty() {
            String::new()
        } else {
            format!(", {}", attachments::summary_for_log(&ask.attachments))
        };
        tracing::info!(
            "{} {}: {} characters{}{}",
            if ask.background { "task" } else { "request" },
            ask.id,
            ask.text.chars().count(),
            display_profile
                .as_deref()
                .map(|p| format!(", profile {p}"))
                .unwrap_or_default(),
            attach_summary,
        );
        tracing::debug!("request {}: {:?}", ask.id, ask.text);

        let token = Arc::clone(&ask.token);
        let stop = Arc::clone(&self.stop);
        let interrupts = Interrupts {
            request: &token,
            stop: &stop,
        };

        let mut retried_auth = false;
        let (session, resolved, conv_id) = loop {
            let was_live = self.live.is_some();
            if !was_live {
                self.restart_at = None;
                // Only a shutdown interrupts cold start; a superseding request
                // needs the same agent process.
                let start = tokio::select! {
                    result = self.start_agent() => result,
                    reason = stop.cancelled() => {
                        self.set_agent(AgentState::Down);
                        self.finish_request(ask, cancelled(reason), display_profile, false);
                        return;
                    }
                };
                if let Err(e) = start {
                    self.schedule_restart_if_warm();
                    self.finish_request(
                        ask,
                        Outcome::failed(
                            FailReason::AgentDown,
                            format!("Cannot start the assistant: {e:#}"),
                        ),
                        display_profile,
                        false,
                    );
                    return;
                }
                if let Some(reason) = token.reason() {
                    self.finish_request(ask, cancelled(reason), display_profile, false);
                    return;
                }
            }

            let session_result = if ask.background {
                let effective_req = ask
                    .profile
                    .clone()
                    .or_else(|| self.config.tasks.profile.clone());
                tokio::select! {
                    res = self.new_background_session(effective_req.as_deref()) => res.map(|(s, r)| (s, r, None)),
                    reason = interrupts.wait() => {
                        self.finish_request(ask, cancelled(reason), display_profile, false);
                        return;
                    }
                }
            } else {
                tokio::select! {
                    res = self.session_for(ask.id, ask.new, ask.profile.as_deref()) => res.map(|(s, r)| {
                        let cid = self.conversation.as_ref().map(|c| c.conversation_id);
                        (s, r, cid)
                    }),
                    reason = interrupts.wait() => {
                        self.finish_request(ask, cancelled(reason), display_profile, false);
                        return;
                    }
                }
            };

            match session_result {
                Ok(triple) => break triple,
                Err(outcome) => {
                    if was_live
                        && !retried_auth
                        && self.live.is_none()
                        && matches!(
                            outcome,
                            Outcome::Failed {
                                reason: FailReason::AgentError,
                                ..
                            }
                        )
                    {
                        retried_auth = true;
                        continue;
                    }
                    self.finish_request(ask, outcome, display_profile, false);
                    return;
                }
            }
        };

        let Some(live) = &self.live else {
            self.finish_request(
                ask,
                Outcome::failed(FailReason::AgentDown, "the assistant exited"),
                resolved.name,
                false,
            );
            return;
        };

        let used_profile = resolved.name.clone();
        if let Some(store) = &self.store {
            let lane = if ask.background {
                Lane::Background
            } else {
                Lane::Interactive
            };
            store.mark_running(
                ask.id,
                lane,
                session.clone(),
                conv_id,
                used_profile.clone().unwrap_or_default(),
                store::unix_timestamp_ms(),
            );
        }

        let client = live.conn.client();
        let subscription = client.subscribe(&session);

        if ask.background {
            self.bg_active.insert(ask.id);
            if let Ok(mut shared) = self.shared.lock() {
                shared.bg_running = self.bg_active.len();
            }
        } else {
            self.fg_active = Some(ask.id);
            if let Ok(mut shared) = self.shared.lock() {
                shared.running = Some((ask.id, started, used_profile.clone()));
            }
        }

        let budgets = Budgets {
            inactivity: resolved.inactivity,
            max_turn: resolved.max_turn,
            max_tool_calls: resolved.max_tool_calls,
            max_repeat_tool_calls: resolved.max_repeat_tool_calls,
        };
        let config = Arc::clone(&self.config);
        let context_provider = Arc::clone(&self.context);
        let policy = Arc::new(
            self.policy
                .with_mcp_servers(Some(resolved.mcp_servers.clone())),
        );
        let presenter = (!ask.background && matches!(ask.sink, Sink::Present))
            .then(|| Arc::clone(&self.presenter));
        let done_tx = self.done_tx.clone();
        let stop = Arc::clone(&self.stop);

        tokio::spawn(async move {
            let token = Arc::clone(&ask.token);
            let interrupts = Interrupts {
                request: &token,
                stop: &stop,
            };
            let header = if config.context.is_enabled() {
                tokio::select! {
                    header = context_provider.gather(&config.context) => header,
                    reason = interrupts.wait() => {
                        let _ = done_tx.send(TurnFinished {
                            ask,
                            session,
                            used_profile,
                            started,
                            end: TurnEnd {
                                outcome: cancelled(reason),
                                alive: true,
                                wedged: false,
                                broken: false,
                                auth_required: false,
                            },
                        });
                        return;
                    }
                }
            } else {
                None
            };
            if let Some(header) = &header {
                tracing::debug!("request {}: ambient context: {:?}", ask.id, header);
            }
            let effective_text = if ask.text.trim().is_empty() && !ask.attachments.is_empty() {
                attachments::default_prompt(&ask.attachments)
            } else {
                &ask.text
            };
            let prompt = context::prepend(header.as_deref(), effective_text);
            let blocks = attachments::build_prompt_blocks(&prompt, &ask.attachments);
            let ctx = TurnContext {
                policy: &policy,
                budgets,
                progress: presenter.as_ref(),
                id: ask.id,
            };
            let end = turn(&client, subscription, &session, blocks, interrupts, &ctx).await;
            let _ = done_tx.send(TurnFinished {
                ask,
                session,
                used_profile,
                started,
                end,
            });
        });
    }

    async fn handle_finished(&mut self, finished: TurnFinished) {
        let TurnFinished {
            ask,
            session,
            used_profile,
            started,
            mut end,
        } = finished;

        if ask.background {
            self.bg_active.remove(&ask.id);
            if let Ok(mut shared) = self.shared.lock() {
                shared.bg_running = self.bg_active.len();
            }
        } else if self.fg_active == Some(ask.id) {
            self.fg_active = None;
        }

        // Apply --suppress-token check before updating follow-up window or persistence.
        if let Some(token) = ask.suppress_token.as_deref()
            && let Outcome::Answer {
                text,
                denied,
                suppressed,
            } = &mut end.outcome
            && is_suppressed(text, token, denied)
        {
            *suppressed = true;
            tracing::info!(
                "{} {}: suppressed (matched token)",
                if ask.background { "task" } else { "request" },
                ask.id
            );
        }

        if end.alive {
            if end.auth_required {
                if !self.has_active_turns() {
                    self.stop_agent().await;
                } else if let Some(live) = &mut self.live {
                    live.auth_failed = true;
                }
            } else if end.wedged {
                if let Some(live) = &self.live {
                    drop(
                        live.conn
                            .start_call("session/close", json!({"sessionId": session})),
                    );
                }
                if !ask.background
                    && self
                        .conversation
                        .as_ref()
                        .is_some_and(|c| c.session == session)
                {
                    self.conversation = None;
                    self.set_follow_up(None);
                }
                if !self.has_active_turns() {
                    self.stop_agent().await;
                    self.schedule_restart_if_warm();
                } else if let Some(live) = &mut self.live {
                    live.draining = true;
                }
            } else if ask.background {
                if let Some(live) = &self.live {
                    drop(
                        live.conn
                            .start_call("session/close", json!({"sessionId": session})),
                    );
                }
            } else if end.broken {
                if let Some(old) = self.conversation.take()
                    && let Some(live) = &self.live
                {
                    drop(
                        live.conn
                            .start_call("session/close", json!({"sessionId": old.session})),
                    );
                }
                self.set_follow_up(None);
            } else if let Some(conversation) = &mut self.conversation
                && conversation.session == session
            {
                let suppressed = matches!(
                    end.outcome,
                    Outcome::Answer {
                        suppressed: true,
                        ..
                    }
                );
                if !suppressed {
                    conversation.last = Instant::now();
                    conversation.turns += 1;
                    let until = secs(self.config.conversation.follow_up_secs)
                        .and_then(|window| conversation.last.checked_add(window));
                    self.set_follow_up(until);
                }
            }
        } else {
            let detail = match self.live.take() {
                Some(live) => {
                    let detail = self.agent_died(live).await;
                    self.last_death_detail = Some(detail.clone());
                    detail
                }
                None => self.last_death_detail.clone().unwrap_or_default(),
            };
            if let Outcome::Failed {
                reason: FailReason::AgentDown,
                message,
            } = &end.outcome
                && !detail.is_empty()
            {
                end.outcome =
                    Outcome::failed(FailReason::AgentDown, format!("{message} ({detail})"));
            }
        }

        if matches!(end.outcome, Outcome::Answer { .. }) {
            self.failures = 0;
        }

        // Tier 2 failure escalation: if an auto-routed fast turn fails with a
        // retryable error (empty answer, agent error, or tool/repeat limit),
        // retry once transparently on `deep_profile`.
        if self.config.routing.enabled
            && self.config.routing.escalate_on_failure
            && ask.auto_routed_fast
            && !ask.escalated_once
            && end.alive
            && !end.auth_required
            && !end.wedged
            && ask.token.reason().is_none()
            && self.config.routing.deep_profile != self.config.routing.fast_profile
            && !ask.background
            && self.fg_pending.is_none()
            && matches!(
                end.outcome,
                Outcome::Failed {
                    reason: FailReason::Empty | FailReason::AgentError | FailReason::LimitReached,
                    ..
                }
            )
        {
            tracing::info!(
                "request {}: escalating failed fast turn to {} profile",
                ask.id,
                self.config.routing.deep_profile
            );
            let mut retry = ask;
            retry.escalated_once = true;
            retry.auto_routed_fast = false;
            retry.profile = Some(self.config.routing.deep_profile.clone());
            retry.new = true;
            self.fg_pending = Some(retry);
            return;
        }

        self.last_used = Instant::now();
        log_outcome(ask.id, &end.outcome, started);
        self.finish_request(ask, end.outcome, used_profile, false);
    }

    /// Apply Tier 1 heuristics and (for non-voice unsure new turns) Tier 3
    /// restricted LLM triage. Returns `Some(reason)` if cancelled during triage.
    async fn apply_routing(&mut self, ask: &mut Ask) -> Option<FailReason> {
        let window = secs(self.config.conversation.follow_up_secs);
        let is_followup = !ask.new
            && self
                .conversation
                .as_ref()
                .is_some_and(|c| window.is_some_and(|w| c.last.elapsed() < w));
        let can_background = matches!(ask.sink, Sink::Present);
        let heuristic = routing::classify_heuristic(
            &ask.text,
            !ask.attachments.is_empty(),
            is_followup,
            can_background,
            self.config.routing.auto_background,
        );

        if is_followup {
            if let HeuristicResult::Decided(RouteDecision {
                profile: TierChoice::Deep,
                ..
            }) = heuristic
            {
                tracing::info!(
                    "request {}: follow-up escalated to {} profile",
                    ask.id,
                    self.config.routing.deep_profile
                );
                ask.profile = Some(self.config.routing.deep_profile.clone());
            }
            return None;
        }

        let decision = match heuristic {
            HeuristicResult::Decided(d) => d,
            HeuristicResult::Unsure => {
                if ask.source != InputSource::Voice && self.config.routing.triage_non_voice {
                    match self.run_triage(ask, can_background).await {
                        Ok(d) => d,
                        Err(reason) => return Some(reason),
                    }
                } else {
                    RouteDecision {
                        profile: TierChoice::Fast,
                        lane: LaneChoice::Interactive,
                    }
                }
            }
        };

        let chosen_profile = match decision.profile {
            TierChoice::Fast => self.config.routing.fast_profile.clone(),
            TierChoice::Deep => self.config.routing.deep_profile.clone(),
        };
        tracing::info!(
            "request {}: routed to {} ({})",
            ask.id,
            chosen_profile,
            if decision.lane == LaneChoice::Background && can_background {
                "background"
            } else {
                "interactive"
            }
        );
        ask.profile = Some(chosen_profile);
        ask.auto_routed_fast = decision.profile == TierChoice::Fast;

        if decision.lane == LaneChoice::Background && can_background {
            let promoted = if let Ok(mut shared) = self.shared.lock() {
                if shared.bg_queued < self.config.tasks.max_queued {
                    if shared
                        .fg_latest
                        .as_ref()
                        .is_some_and(|(id, _)| *id == ask.id)
                    {
                        shared.fg_latest = None;
                    }
                    shared.bg_tasks.insert(ask.id, Arc::clone(&ask.token));
                    shared.bg_queued += 1;
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if promoted {
                ask.background = true;
            }
        }
        None
    }

    /// Tier 3 bounded, tool-disabled LLM triage on `fast_profile` for non-voice
    /// requests where heuristics returned `Unsure`. Never called for `Voice`.
    async fn run_triage(
        &mut self,
        ask: &Ask,
        can_background: bool,
    ) -> std::result::Result<RouteDecision, FailReason> {
        let fallback = RouteDecision {
            profile: TierChoice::Fast,
            lane: LaneChoice::Interactive,
        };
        if let Some(reason) = ask.token.reason() {
            return Err(reason);
        }
        if let Ok(mut shared) = self.shared.lock() {
            shared.running = Some((ask.id, Instant::now(), None));
        }
        if self.live.is_none() {
            self.restart_at = None;
            let stop = Arc::clone(&self.stop);
            let started = tokio::select! {
                res = self.start_agent() => res,
                reason = stop.cancelled() => return Err(reason),
            };
            if started.is_err() {
                self.schedule_restart_if_warm();
                return Ok(fallback);
            }
        }
        if let Some(reason) = ask.token.reason() {
            return Err(reason);
        }
        let Ok(fast_resolved) = self
            .config
            .resolve_profile(Some(&self.config.routing.fast_profile))
        else {
            return Ok(fallback);
        };
        let session = match self.new_session(&fast_resolved, false).await {
            Ok(s) => s,
            Err(_) => return Ok(fallback),
        };
        let Some(live) = &self.live else {
            return Ok(fallback);
        };
        let client = live.conn.client();
        let sub = client.subscribe(&session);
        let timeout = Duration::from_secs_f64(self.config.routing.triage_timeout_secs.max(0.2));
        let restricted = LanePolicy::restricted();
        let budgets = Budgets {
            inactivity: timeout,
            max_turn: Some(timeout),
            max_tool_calls: Some(0),
            max_repeat_tool_calls: None,
        };
        let ctx = TurnContext {
            policy: &restricted,
            budgets,
            progress: None,
            id: ask.id,
        };
        let stop = Arc::clone(&self.stop);
        let interrupts = Interrupts {
            request: &ask.token,
            stop: &stop,
        };
        let prompt = routing::build_triage_prompt(
            &ask.text,
            can_background && self.config.routing.auto_background,
        );
        let blocks = vec![json!({"type": "text", "text": prompt})];
        let end = turn(&client, sub, &session, blocks, interrupts, &ctx).await;

        if end.alive {
            if let Some(live) = &self.live {
                drop(
                    live.conn
                        .start_call("session/close", json!({"sessionId": session})),
                );
            }
        } else if let Some(live) = self.live.take() {
            let detail = self.agent_died(live).await;
            self.last_death_detail = Some(detail);
        }

        if let Some(reason) = ask.token.reason() {
            return Err(reason);
        }
        if let Outcome::Answer { text, .. } = end.outcome
            && let Some(mut decision) = routing::parse_triage_response(&text)
        {
            if !can_background || !self.config.routing.auto_background {
                decision.lane = LaneChoice::Interactive;
            }
            return Ok(decision);
        }
        Ok(fallback)
    }

    fn finish_request(
        &mut self,
        ask: Ask,
        outcome: Outcome,
        profile: Option<String>,
        was_queued_bg: bool,
    ) {
        ask.token.finish();
        if let Ok(mut shared) = self.shared.lock() {
            if ask.background {
                shared.bg_tasks.remove(&ask.id);
                if was_queued_bg {
                    shared.bg_queued = shared.bg_queued.saturating_sub(1);
                }
            } else if shared
                .running
                .as_ref()
                .is_some_and(|(id, _, _)| *id == ask.id)
            {
                shared.running = None;
            }
            shared.handled += 1;
        }
        if let Some(store) = &self.store {
            let now_ms = store::unix_timestamp_ms();
            match &outcome {
                Outcome::Answer {
                    suppressed: true, ..
                } => {
                    store.finish_suppressed(
                        ask.id,
                        ask.suppress_token.clone().unwrap_or_default(),
                        now_ms,
                    );
                }
                Outcome::Answer { text, denied, .. } => {
                    store.finish_completed(ask.id, text.clone(), denied.clone(), now_ms);
                }
                Outcome::Failed {
                    reason: reason @ (FailReason::Cancelled | FailReason::Superseded),
                    message,
                } => {
                    store.finish_cancelled(ask.id, *reason, message.clone(), now_ms);
                }
                Outcome::Failed { reason, message } => {
                    store.finish_failed(ask.id, *reason, message.clone(), now_ms);
                }
            }
        }
        deliver(Some(&self.presenter), ask, outcome, profile);
    }

    fn effective_display_profile(&self, ask: &Ask) -> Option<String> {
        let raw = ask.profile.as_deref().or(if ask.background {
            self.config.tasks.profile.as_deref()
        } else {
            None
        });
        match raw {
            Some("default") => None,
            Some(name) => Some(name.to_string()),
            None if !ask.background => {
                let window = secs(self.config.conversation.follow_up_secs);
                if !ask.new
                    && let Some(conv) = &self.conversation
                    && window.is_some_and(|w| conv.last.elapsed() < w)
                {
                    conv.profile_name.clone()
                } else {
                    None
                }
            }
            None => None,
        }
    }

    fn idle_deadline(&self) -> Option<Instant> {
        let idle = secs(self.config.agent.idle_stop_secs)?;
        self.live.as_ref()?;
        if self.has_active_turns() || self.fg_pending.is_some() || !self.bg_queue.is_empty() {
            return None;
        }
        self.last_used.checked_add(idle)
    }

    fn set_agent(&self, state: AgentState) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.agent = state;
        }
    }

    /// Start the agent ahead of any request, with a default session ready.
    async fn start_warm(&mut self) {
        if self.live.is_some() {
            return;
        }
        if let Err(e) = self.start_agent().await {
            tracing::warn!("cannot start the agent: {e:#}");
            self.schedule_restart();
            return;
        }
        let default_profile = self
            .config
            .resolve_profile(None)
            .expect("base [agent] profile always resolves");
        match self
            .new_session(&default_profile, self.policy.allow_mcp)
            .await
        {
            Ok(session) => {
                if let Some(live) = &mut self.live {
                    live.spare = Some(SpareSession {
                        session,
                        model: default_profile.model,
                        thought_level: default_profile.thought_level,
                    });
                }
            }
            Err(Outcome::Failed { message, .. }) => {
                if self.live.is_some() {
                    tracing::warn!("the agent is up but cannot open a session: {message}");
                } else {
                    tracing::warn!("warm start could not open a session: {message}");
                }
            }
            Err(Outcome::Answer { .. }) => {}
        }
    }

    async fn start_agent(&mut self) -> Result<()> {
        self.set_agent(AgentState::Starting);
        let result = self.spawn_agent().await;
        self.set_agent(if result.is_ok() {
            AgentState::Ready
        } else {
            AgentState::Down
        });
        result
    }

    async fn spawn_agent(&mut self) -> Result<()> {
        let started = Instant::now();
        let paths =
            environment::prepare(&self.config).context("preparing the agent's directory")?;
        // The agent is not running, so nothing it has open is deleted.
        environment::prune_history(&paths.agent_dir, self.config.agent.history_days);
        environment::prune_artifacts(&paths.artifacts, self.config.agent.history_days);
        let argv = paths.agent_command(&self.config);
        let (conn, exited) = Connection::spawn(&argv, &paths.workspace, &paths.state_dir)?;
        let init = conn
            .call(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": {"readTextFile": false, "writeTextFile": false},
                        "terminal": false,
                    },
                    "clientInfo": {"name": "hey-computer", "version": env!("CARGO_PKG_VERSION")},
                }),
                START_TIMEOUT,
            )
            .await;
        let result = match init {
            Ok(result) => result,
            Err(e) => {
                let ended = conn.reap().await;
                return Err(anyhow!(
                    "the agent did not initialize ({e}); it ended with {ended}"
                ));
            }
        };
        let version = result.get("protocolVersion").and_then(Value::as_u64);
        if version != Some(1) {
            tracing::warn!("the agent speaks protocol version {version:?}, expected 1");
        }
        let agent = result
            .pointer("/agentInfo/name")
            .and_then(Value::as_str)
            .unwrap_or("agent");
        let agent_version = result
            .pointer("/agentInfo/version")
            .and_then(Value::as_str)
            .unwrap_or("?");
        tracing::info!(
            "{agent} {agent_version} started in {:.2} s",
            started.elapsed().as_secs_f64()
        );
        self.last_death_detail = None;
        self.paths = Some(paths);
        self.live = Some(Live {
            conn,
            exited: Some(exited),
            started: Instant::now(),
            spare: None,
            agent_defaults: (String::new(), String::new()),
            draining: false,
            auth_failed: false,
        });
        Ok(())
    }

    async fn stop_agent(&mut self) {
        self.conversation = None;
        self.set_follow_up(None);
        if let Some(live) = self.live.take() {
            live.conn.shutdown().await;
        }
        self.set_agent(AgentState::Down);
    }

    fn schedule_restart(&mut self) {
        let base = self.config.agent.restart_delay_secs.max(0.05);
        let factor = 2f64.powi(self.failures.min(10) as i32);
        let delay = Duration::from_secs_f64((base * factor).min(MAX_RESTART_DELAY.as_secs_f64()));
        self.failures += 1;
        self.restart_at = Some(Instant::now() + delay);
        tracing::info!("restarting the agent in {:.1} s", delay.as_secs_f64());
    }

    fn schedule_restart_if_warm(&mut self) {
        if self.config.agent.warm_start {
            self.schedule_restart();
        }
    }

    /// The agent is gone: say why, forget its session, and restart it if it
    /// is meant to stay up.
    async fn agent_died(&mut self, live: Live) -> String {
        let lived = live.started.elapsed();
        let ended = live.conn.reap().await;
        tracing::warn!("the agent exited: {}", ended.status);
        tracing::debug!("the agent's last stderr: {}", ended.tail);
        self.conversation = None;
        self.set_follow_up(None);
        self.set_agent(AgentState::Down);
        if lived >= HEALTHY_RUN {
            self.failures = 0;
        }
        if self.config.agent.warm_start {
            self.schedule_restart();
        }
        ended.to_string()
    }

    fn set_follow_up(&self, until: Option<Instant>) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.follow_up_until = until;
        }
    }

    async fn new_background_session(
        &mut self,
        requested: Option<&str>,
    ) -> Result<(String, ResolvedProfile), Outcome> {
        let resolved = self
            .config
            .resolve_profile(requested)
            .map_err(|e| Outcome::failed(FailReason::AgentError, e))?;
        let session = self.new_session(&resolved, self.policy.allow_mcp).await?;
        Ok((session, resolved))
    }

    /// The session this interactive request continues (with any mid-conversation
    /// profile switch applied), or a new one.
    async fn session_for(
        &mut self,
        turn_id: u64,
        new: bool,
        requested: Option<&str>,
    ) -> Result<(String, ResolvedProfile), Outcome> {
        let window = secs(self.config.conversation.follow_up_secs);
        if !new
            && let Some(conversation) = &self.conversation
            && window.is_some_and(|w| conversation.last.elapsed() < w)
        {
            if requested.is_none() {
                let resolved = self
                    .config
                    .resolve_profile(conversation.profile_name.as_deref())
                    .map_err(|e| Outcome::failed(FailReason::AgentError, e))?;
                tracing::info!(
                    "continuing the conversation (turn {})",
                    conversation.turns + 1
                );
                return Ok((conversation.session.clone(), resolved));
            }

            let resolved = self
                .config
                .resolve_profile(requested)
                .map_err(|e| Outcome::failed(FailReason::AgentError, e))?;
            // `session/new` always passes `mcpServers: []` (`config/mcp_config.json`
            // is process-wide) and `LanePolicy` enforces `resolved.mcp_servers` per
            // turn, so switching profiles across different MCP allowlists stays in
            // place and preserves conversation history.
            if resolved.model == conversation.model
                && resolved.thought_level == conversation.thought_level
            {
                let session = conversation.session.clone();
                let turns = conversation.turns + 1;
                if let Some(conv) = &mut self.conversation {
                    conv.profile_name = resolved.name.clone();
                }
                tracing::info!("continuing the conversation (turn {turns})");
                return Ok((session, resolved));
            }

            let session = conversation.session.clone();
            let prev_model = conversation.model.clone();
            let prev_thought = conversation.thought_level.clone();
            let turns = conversation.turns + 1;
            match self
                .apply_profile_switch(&session, &prev_model, &prev_thought, &resolved)
                .await?
            {
                true => {
                    if let Some(conv) = &mut self.conversation {
                        conv.profile_name = resolved.name.clone();
                        conv.model = resolved.model.clone();
                        conv.thought_level = resolved.thought_level.clone();
                    }
                    tracing::info!("switched profile in place on the conversation (turn {turns})");
                    return Ok((session, resolved));
                }
                false => {
                    tracing::warn!(
                        "mid-conversation profile switch refused; starting a new session"
                    );
                }
            }
        }

        let resolved = self
            .config
            .resolve_profile(requested)
            .map_err(|e| Outcome::failed(FailReason::AgentError, e))?;

        if let Some(old) = self.conversation.take()
            && let Some(live) = &self.live
        {
            drop(
                live.conn
                    .start_call("session/close", json!({"sessionId": old.session})),
            );
        }
        self.set_follow_up(None);

        let spare_matches = self
            .live
            .as_ref()
            .and_then(|live| live.spare.as_ref())
            .is_some_and(|spare| {
                spare.model == resolved.model && spare.thought_level == resolved.thought_level
            });
        let session = if spare_matches {
            self.live
                .as_mut()
                .and_then(|live| live.spare.take())
                .expect("checked above")
                .session
        } else {
            self.new_session(&resolved, self.policy.allow_mcp).await?
        };
        tracing::info!("new conversation");
        self.conversation = Some(Conversation {
            session: session.clone(),
            conversation_id: turn_id,
            profile_name: resolved.name.clone(),
            model: resolved.model.clone(),
            thought_level: resolved.thought_level.clone(),
            last: Instant::now(),
            turns: 0,
        });
        Ok((session, resolved))
    }

    async fn apply_profile_switch(
        &mut self,
        session: &str,
        prev_model: &str,
        prev_thought: &str,
        resolved: &ResolvedProfile,
    ) -> Result<bool, Outcome> {
        let Some(live) = &self.live else {
            return Err(Outcome::failed(
                FailReason::AgentDown,
                "the assistant is not running",
            ));
        };
        let mut updates: Vec<(&str, String)> = Vec::new();
        if resolved.model != prev_model {
            let target = if resolved.model.is_empty() {
                live.agent_defaults.0.clone()
            } else {
                resolved.model.clone()
            };
            if target.is_empty() {
                return Ok(false);
            }
            updates.push(("model", target));
        }
        if resolved.thought_level != prev_thought {
            let target = if resolved.thought_level.is_empty() {
                live.agent_defaults.1.clone()
            } else {
                resolved.thought_level.clone()
            };
            if target.is_empty() {
                return Ok(false);
            }
            updates.push(("thought_level", target));
        }
        for (key, value) in updates {
            let Some(live) = &self.live else {
                return Err(Outcome::failed(
                    FailReason::AgentDown,
                    "the assistant is not running",
                ));
            };
            let res = live
                .conn
                .call(
                    "session/set_config_option",
                    json!({"sessionId": session, "configId": key, "value": value}),
                    CONFIG_TIMEOUT,
                )
                .await;
            match res {
                Ok(_) => {}
                Err(CallError::Rpc(e)) => {
                    if is_auth_required(&e) {
                        return Err(self
                            .call_failed("session/set_config_option", CallError::Rpc(e))
                            .await);
                    }
                    tracing::warn!("cannot switch {key} to {value} mid-session: {e}");
                    return Ok(false);
                }
                Err(other) => {
                    return Err(self.call_failed("session/set_config_option", other).await);
                }
            }
        }
        Ok(true)
    }

    async fn new_session(
        &mut self,
        resolved: &ResolvedProfile,
        _allow_mcp: bool,
    ) -> Result<String, Outcome> {
        let started = Instant::now();
        let (Some(live), Some(paths)) = (&self.live, &self.paths) else {
            return Err(Outcome::failed(
                FailReason::AgentDown,
                "the assistant is not running",
            ));
        };
        // MCP servers are written to `<agent_dir>/config/mcp_config.json` in
        // `environment::prepare()` so `agy acp` 1.5.13 enables lazy
        // `call_mcp_tool` loading instead of eagerly declaring and truncating
        // large tool schemas. Per-profile `mcp_servers` allowlists and
        // restricted-lane blocking are enforced by `LanePolicy` on every
        // `call_mcp_tool` / `mcp_*` permission request.
        let params = json!({"cwd": paths.workspace, "mcpServers": []});
        let result = live.conn.call("session/new", params, START_TIMEOUT).await;
        let result = match result {
            Ok(result) => result,
            Err(e) => return Err(self.call_failed("session/new", e).await),
        };
        let Some(session) = result.get("sessionId").and_then(Value::as_str) else {
            return Err(Outcome::failed(
                FailReason::AgentError,
                "the assistant opened a session without an id",
            ));
        };
        let session = session.to_string();

        if let Some(live) = &mut self.live
            && live.agent_defaults.0.is_empty()
            && live.agent_defaults.1.is_empty()
        {
            live.agent_defaults = (
                config_option(&result, "model").unwrap_or_default(),
                config_option(&result, "thought_level").unwrap_or_default(),
            );
        }

        let mut options = vec![];
        if !resolved.model.is_empty() {
            options.push(("model", resolved.model.clone()));
        }
        if !resolved.thought_level.is_empty() {
            options.push(("thought_level", resolved.thought_level.clone()));
        }
        if let Some(mode) = config_option(&result, "mode")
            && mode != "default"
        {
            options.push(("mode", "default".to_string()));
        }
        let live = self.live.as_ref().expect("checked above");
        for (key, value) in options {
            let set = live
                .conn
                .call(
                    "session/set_config_option",
                    json!({"sessionId": session, "configId": key, "value": value}),
                    CONFIG_TIMEOUT,
                )
                .await;
            if let Err(e) = set {
                tracing::warn!("cannot set {key} to {value}: {e}");
            }
        }
        tracing::info!("session ready in {:.2} s", started.elapsed().as_secs_f64());
        Ok(session)
    }

    async fn call_failed(&mut self, method: &str, error: CallError) -> Outcome {
        match error {
            CallError::Rpc(e) => {
                if is_auth_required(&e) {
                    tracing::warn!(
                        "{method} failed: {e}; stopping the agent so the next request starts fresh"
                    );
                    if !self.has_active_turns() {
                        self.stop_agent().await;
                    } else if let Some(live) = &mut self.live {
                        live.auth_failed = true;
                    }
                } else {
                    tracing::warn!("{method} failed: {e}");
                }
                Outcome::failed(FailReason::AgentError, agent_error_message(&e))
            }
            CallError::Closed => {
                let detail = match self.live.take() {
                    Some(live) => {
                        let d = self.agent_died(live).await;
                        self.last_death_detail = Some(d.clone());
                        d
                    }
                    None => self.last_death_detail.clone().unwrap_or_default(),
                };
                Outcome::failed(
                    FailReason::AgentDown,
                    format!("The assistant exited ({detail})"),
                )
            }
            CallError::Timeout(after) => {
                tracing::warn!("{method} got no answer within {after:?}; restarting the agent");
                if !self.has_active_turns() {
                    self.stop_agent().await;
                    self.schedule_restart_if_warm();
                } else if let Some(live) = &mut self.live {
                    live.draining = true;
                }
                Outcome::failed(
                    FailReason::AgentDown,
                    format!(
                        "The assistant did not answer {method} within {} s",
                        after.as_secs()
                    ),
                )
            }
        }
    }
}

/// Send the outcome where it belongs.
fn deliver(
    presenter: Option<&Arc<dyn Presenter>>,
    ask: Ask,
    outcome: Outcome,
    profile: Option<String>,
) {
    match ask.sink {
        Sink::Client(reply) => {
            let _ = reply.send(outcome.into_reply(ask.id));
        }
        Sink::Present => {
            let Some(presenter) = presenter else {
                return;
            };
            match outcome {
                Outcome::Answer {
                    suppressed: true, ..
                } => {
                    if !ask.background {
                        presenter.present(Presentation::DismissProgress { id: ask.id });
                    }
                }
                Outcome::Answer { text, denied, .. } => {
                    if ask.background {
                        presenter.present(Presentation::TaskComplete {
                            id: ask.id,
                            request: ask.text,
                            text,
                            denied,
                            profile,
                        });
                    } else {
                        presenter.present(Presentation::Answer {
                            id: ask.id,
                            text,
                            denied,
                            profile,
                        });
                    }
                }
                Outcome::Failed { reason, message } => {
                    presenter.present(Presentation::Failure {
                        id: ask.id,
                        reason,
                        message,
                        profile,
                        background: ask.background,
                    });
                }
            }
        }
    }
}

fn is_auth_required(error: &acp::RpcError) -> bool {
    error.code == AUTH_REQUIRED || error.message.contains("Authentication required")
}

fn agent_error_message(error: &acp::RpcError) -> String {
    if is_auth_required(error) {
        format!(
            "The assistant is not signed in ({error}). Sign in once with an interactive `agy` \
             session, or run `agy acp --login`, then ask again."
        )
    } else {
        error.to_string()
    }
}

const AUTH_REQUIRED: i64 = -32000;

fn cancelled(reason: FailReason) -> Outcome {
    Outcome::failed(reason, cancel_message(reason))
}

fn cancel_message(reason: FailReason) -> &'static str {
    match reason {
        FailReason::Superseded => "A newer request replaced this one.",
        FailReason::AgentDown => "The daemon is shutting down.",
        _ => "The request was cancelled.",
    }
}

#[derive(Clone, Copy)]
struct Interrupts<'a> {
    request: &'a CancelToken,
    stop: &'a CancelToken,
}

impl Interrupts<'_> {
    async fn wait(self) -> FailReason {
        tokio::select! {
            reason = self.request.cancelled() => reason,
            reason = self.stop.cancelled() => reason,
        }
    }
}

fn log_outcome(id: u64, outcome: &Outcome, started: Instant) {
    let elapsed = started.elapsed().as_secs_f64();
    match outcome {
        Outcome::Answer {
            text,
            denied,
            suppressed,
        } => {
            tracing::info!(
                "request {id}: {} in {elapsed:.2} s, {} characters, {} refused",
                if *suppressed {
                    "suppressed"
                } else {
                    "answered"
                },
                text.chars().count(),
                denied.len()
            );
            tracing::debug!("answer {id}: {text:?}");
        }
        Outcome::Failed { reason, message } => {
            tracing::info!("request {id}: {reason:?} after {elapsed:.2} s");
            tracing::debug!("request {id}: {message}");
        }
    }
}

fn config_option(session: &Value, id: &str) -> Option<String> {
    session
        .get("configOptions")?
        .as_array()?
        .iter()
        .find(|o| o.get("id").and_then(Value::as_str) == Some(id))?
        .get("currentValue")?
        .as_str()
        .map(String::from)
}

async fn wait_exited(live: &mut Option<Live>) {
    match live {
        Some(live) => match &mut live.exited {
            Some(rx) => {
                let _ = rx.await;
            }
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

/// One prompt, from `session/prompt` to its stop reason, isolated to one
/// session via its [`SessionSubscription`].
async fn turn(
    client: &AcpClient,
    mut sub: SessionSubscription,
    session: &str,
    prompt_blocks: Vec<Value>,
    interrupts: Interrupts<'_>,
    ctx: &TurnContext<'_>,
) -> TurnEnd {
    let policy = ctx.policy;
    let budgets = ctx.budgets;
    let id = ctx.id;
    let started = Instant::now();
    let mut response = client.start_call(
        "session/prompt",
        json!({"sessionId": session, "prompt": prompt_blocks}),
    );
    let mut answer = String::new();
    let mut denied: Vec<String> = Vec::new();
    let mut tools: Vec<String> = Vec::new();
    let mut tracker = ToolTracker::default();
    let mut last_progress: Option<String> = None;
    let mut first_text: Option<Duration> = None;
    let mut deadline = Instant::now() + budgets.inactivity;
    let turn_deadline = budgets.max_turn.and_then(|d| started.checked_add(d));
    let turn_sleep = sleep_until(turn_deadline);
    tokio::pin!(turn_sleep);
    let at = || started.elapsed().as_secs_f64();
    let emit_progress = |last: &mut Option<String>, status: Option<String>| {
        let (Some(presenter), Some(status)) = (ctx.progress, status) else {
            return;
        };
        if last.as_deref() == Some(status.as_str()) {
            return;
        }
        *last = Some(status.clone());
        presenter.present(Presentation::ProgressUpdate { id, status });
    };

    let ended = loop {
        // Events first: read_loop queues every update before it hands over the
        // response that follows them.
        let step = tokio::select! {
            biased;
            event = sub.recv() => Step::Event(event),
            result = &mut response => Step::Responded(result),
            () = tokio::time::sleep_until(deadline.into()) => Step::Quiet,
            () = &mut turn_sleep => Step::TurnTimeout,
            reason = interrupts.wait() => Step::Cancelled(reason),
        };
        match step {
            Step::Responded(result) => break Ok(result),
            Step::Event(None) => {
                return TurnEnd {
                    outcome: Outcome::failed(
                        FailReason::AgentDown,
                        "The assistant exited in the middle of the request",
                    ),
                    alive: false,
                    wedged: false,
                    broken: true,
                    auth_required: false,
                };
            }
            Step::Event(Some(SessionEvent::Update(update))) => {
                if let Some(limit) = budgets.max_turn
                    && turn_deadline.is_some_and(|d| Instant::now() >= d)
                {
                    break Err(TurnAbort::TurnTimeout(limit));
                }
                deadline = Instant::now() + budgets.inactivity;
                if let Some(chunk) = acp::chunk_text(&update) {
                    first_text.get_or_insert_with(|| started.elapsed());
                    answer.push_str(chunk);
                } else {
                    match update.get("sessionUpdate").and_then(Value::as_str) {
                        Some("tool_call") => {
                            if tracker.record_update(&update) {
                                let kind = update
                                    .get("kind")
                                    .and_then(Value::as_str)
                                    .unwrap_or("other")
                                    .to_string();
                                tracing::info!("request {id}: +{:.2} s tool call ({kind})", at());
                                let title =
                                    update.get("title").and_then(Value::as_str).unwrap_or("");
                                tracing::debug!("request {id}: tool call {title}");
                                tools.push(kind);
                                if let Some(limit) = budgets.max_tool_calls
                                    && tracker.count() > limit
                                {
                                    break Err(TurnAbort::ToolLimit(limit));
                                }
                            }
                            if let Some(limit) = budgets.max_repeat_tool_calls
                                && tracker.repeat_streak() >= limit
                            {
                                break Err(TurnAbort::RepeatToolLoop {
                                    count: tracker.repeat_streak(),
                                    tool: tracker.last_tool_name(),
                                });
                            }
                            if !policy.will_deny_update(&update) {
                                emit_progress(&mut last_progress, acp::tool_progress(&update));
                            }
                        }
                        Some("tool_call_update") => {
                            emit_progress(&mut last_progress, acp::tool_progress(&update));
                        }
                        _ => {}
                    }
                }
            }
            Step::Event(Some(SessionEvent::Permission {
                id: request,
                params,
            })) => {
                if let Some(limit) = budgets.max_turn
                    && turn_deadline.is_some_and(|d| Instant::now() >= d)
                {
                    client.respond(request, json!({"outcome": {"outcome": "cancelled"}}));
                    break Err(TurnAbort::TurnTimeout(limit));
                }
                deadline = Instant::now() + budgets.inactivity;
                let _ = tracker.record_permission(params.get("toolCall"));
                if let Some(limit) = budgets.max_tool_calls
                    && tracker.count() > limit
                {
                    client.respond(request, json!({"outcome": {"outcome": "cancelled"}}));
                    break Err(TurnAbort::ToolLimit(limit));
                }
                if let Some(limit) = budgets.max_repeat_tool_calls
                    && tracker.repeat_streak() >= limit
                {
                    client.respond(request, json!({"outcome": {"outcome": "cancelled"}}));
                    break Err(TurnAbort::RepeatToolLoop {
                        count: tracker.repeat_streak(),
                        tool: tracker.last_tool_name(),
                    });
                }
                let kind = params
                    .pointer("/toolCall/kind")
                    .and_then(Value::as_str)
                    .unwrap_or("other");
                let outcome = match policy.evaluate_permission(&params) {
                    PermissionDecision::Deny { tag, detail } => {
                        tracing::info!("request {id}: +{:.2} s refused a {tag} call", at());
                        denied.push(detail);
                        acp::permission_outcome(&params, &["reject_once", "reject_always"])
                    }
                    PermissionDecision::Allow => {
                        tracing::info!("request {id}: +{:.2} s allowed a {kind} call", at());
                        if let Some(command) = acp::permission_command(&params) {
                            tracing::debug!("request {id}: allowed {command:?}");
                        }
                        emit_progress(
                            &mut last_progress,
                            params.get("toolCall").and_then(acp::tool_progress),
                        );
                        acp::permission_outcome(&params, &["allow_once", "allow_always"])
                    }
                };
                client.respond(request, outcome);
            }
            Step::Quiet => break Err(TurnAbort::Quiet),
            Step::TurnTimeout => {
                let limit = budgets.max_turn.unwrap_or(Duration::ZERO);
                break Err(TurnAbort::TurnTimeout(limit));
            }
            Step::Cancelled(reason) => break Err(TurnAbort::Cancelled(reason)),
        }
    };

    let result = match ended {
        Ok(result) => result,
        Err(abort) => {
            let cancel_state = cancel_turn(client, &mut sub, session, &mut response).await;
            let (alive, wedged) = match cancel_state {
                CancelState::Stopped => (true, false),
                CancelState::Wedged => (true, true),
                CancelState::AgentDead => (false, false),
            };
            let (reason, message, broken) = match abort {
                TurnAbort::Quiet => (
                    FailReason::Timeout,
                    format!(
                        "The assistant sent nothing for {} s and was stopped.",
                        budgets.inactivity.as_secs_f64().round()
                    ),
                    true,
                ),
                TurnAbort::TurnTimeout(limit) => {
                    tracing::warn!(
                        "request {id}: +{:.2} s exceeded max_turn_secs ({:.0} s)",
                        at(),
                        limit.as_secs_f64()
                    );
                    (
                        FailReason::Timeout,
                        format!(
                            "The assistant ran for more than {} s and was stopped.",
                            limit.as_secs_f64().round()
                        ),
                        true,
                    )
                }
                TurnAbort::ToolLimit(limit) => {
                    tracing::warn!(
                        "request {id}: +{:.2} s exceeded max_tool_calls ({limit})",
                        at()
                    );
                    let noun = if limit == 1 {
                        "tool call"
                    } else {
                        "tool calls"
                    };
                    (
                        FailReason::LimitReached,
                        format!(
                            "The assistant exceeded the limit of {limit} {noun} and was stopped."
                        ),
                        true,
                    )
                }
                TurnAbort::RepeatToolLoop { count, tool } => {
                    tracing::warn!(
                        "request {id}: +{:.2} s aborted repeat tool loop ({count} consecutive calls to {tool})",
                        at()
                    );
                    (
                        FailReason::LimitReached,
                        format!(
                            "The assistant repeated the same tool call {count} times ({tool}) and was stopped."
                        ),
                        true,
                    )
                }
                TurnAbort::Cancelled(reason) => (reason, cancel_message(reason).to_string(), false),
            };
            return TurnEnd {
                outcome: Outcome::failed(reason, message),
                alive,
                wedged,
                broken: broken || wedged,
                auth_required: false,
            };
        }
    };

    if let Some(first) = first_text {
        tracing::info!(
            "request {id}: first text after {:.2} s, done after {:.2} s, tools: {}",
            first.as_secs_f64(),
            at(),
            if tools.is_empty() {
                "none".to_string()
            } else {
                tools.join(", ")
            }
        );
    }

    let stop = match result {
        Ok(Ok(result)) => result
            .get("stopReason")
            .and_then(Value::as_str)
            .unwrap_or("end_turn")
            .to_string(),
        Ok(Err(CallError::Rpc(e))) => {
            tracing::warn!("request {id}: the agent refused the prompt: {e}");
            let auth_required = is_auth_required(&e);
            return TurnEnd {
                outcome: Outcome::failed(FailReason::AgentError, agent_error_message(&e)),
                alive: true,
                wedged: false,
                broken: true,
                auth_required,
            };
        }
        Ok(Err(_)) | Err(_) => {
            return TurnEnd {
                outcome: Outcome::failed(
                    FailReason::AgentDown,
                    "The assistant exited in the middle of the request",
                ),
                alive: false,
                wedged: false,
                broken: true,
                auth_required: false,
            };
        }
    };

    let answer = answer.trim().to_string();
    let outcome = match stop.as_str() {
        "cancelled" => cancelled(FailReason::Cancelled),
        _ if !answer.is_empty() => Outcome::Answer {
            text: answer,
            denied,
            suppressed: false,
        },
        _ if !denied.is_empty() => {
            let verb = if denied.len() == 1 { "is" } else { "are" };
            Outcome::failed(
                FailReason::Denied,
                format!(
                    "The assistant wanted to run `{}`, which {verb} on the deny list and cannot be run.",
                    denied.join("`, `")
                ),
            )
        }
        "refusal" => Outcome::failed(FailReason::AgentError, "The model refused to answer."),
        _ => Outcome::failed(
            FailReason::Empty,
            "The assistant finished without an answer.",
        ),
    };
    TurnEnd {
        outcome,
        alive: true,
        wedged: false,
        broken: false,
        auth_required: false,
    }
}

enum CancelState {
    Stopped,
    Wedged,
    AgentDead,
}

/// Ask the agent to stop one session's turn and wait briefly for it to do so.
async fn cancel_turn(
    client: &AcpClient,
    sub: &mut SessionSubscription,
    session: &str,
    response: &mut oneshot::Receiver<Result<Value, CallError>>,
) -> CancelState {
    client.notify("session/cancel", json!({"sessionId": session}));
    let deadline = tokio::time::Instant::now() + CANCEL_GRACE;
    loop {
        tokio::select! {
            biased;
            event = sub.recv() => match event {
                None => return CancelState::AgentDead,
                Some(SessionEvent::Permission { id, .. }) => {
                    client.respond(id, json!({"outcome": {"outcome": "cancelled"}}));
                }
                Some(SessionEvent::Update(_)) => {}
            },
            result = &mut *response => {
                return match result {
                    Ok(Ok(_)) | Ok(Err(CallError::Rpc(_))) => CancelState::Stopped,
                    _ => CancelState::AgentDead,
                };
            }
            () = tokio::time::sleep_until(deadline) => {
                tracing::warn!("the agent did not stop the turn within {CANCEL_GRACE:?}");
                return CancelState::Wedged;
            }
        }
    }
}

/// Run one request without a daemon: start the agent, ask, stop it. For
/// `debug prompt`, which is how the agent's behaviour is measured.
pub async fn ask_once(
    config: Config,
    text: String,
    profile: Option<String>,
    attachment_specs: Vec<crate::protocol::Attachment>,
) -> Result<Outcome> {
    if let Err(msg) = config.resolve_profile(profile.as_deref()) {
        anyhow::bail!("{msg}");
    }
    let loaded = attachments::load_all(&attachment_specs)?;
    let mut config = config;
    config.agent.warm_start = false;
    let presenter: Arc<dyn Presenter> = crate::present::Recorder::new();
    let (handle, task) =
        spawn_with_store(Arc::new(config), presenter, Arc::new(SystemContext), None);
    let (tx, rx) = oneshot::channel();
    let id = handle
        .ask(
            AskParams {
                text,
                new: true,
                profile,
                background: false,
                suppress_token: None,
                attachments: loaded,
                source: InputSource::Cli,
            },
            Sink::Client(tx),
        )
        .map_err(|e| anyhow!(e))?;
    let reply = rx.await.context("the conductor stopped")?;
    handle.shutdown();
    let _ = task.await;
    Ok(match reply {
        Reply::Answer {
            text,
            denied,
            suppressed,
            ..
        } => Outcome::Answer {
            text,
            denied,
            suppressed,
        },
        Reply::Failed {
            reason, message, ..
        } => Outcome::Failed { reason, message },
        other => Outcome::failed(
            FailReason::AgentError,
            format!("unexpected reply to request {id}: {other:?}"),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_tracker_counts_distinct_calls_without_double_counting() {
        let mut tracker = ToolTracker::default();
        let c1 = json!({"toolCallId": "call_1", "name": "read_file", "rawInput": {"Path": "/a"}});
        let c2 = json!({"toolCallId": "call_2", "name": "read_file", "rawInput": {"Path": "/b"}});
        let c_anon = json!({"name": "read_file", "rawInput": {"Path": "/c"}});
        assert!(tracker.record_update(&c1));
        assert!(!tracker.record_update(&c1));
        assert!(!tracker.record_permission(Some(&c1)));
        assert_eq!(tracker.count(), 1);
        assert_eq!(tracker.repeat_streak(), 1);

        assert!(tracker.record_permission(Some(&c2)));
        assert!(!tracker.record_update(&c2));
        assert_eq!(tracker.count(), 2);
        assert_eq!(tracker.repeat_streak(), 1);

        assert!(tracker.record_update(&c_anon));
        assert!(!tracker.record_permission(None));
        assert_eq!(tracker.count(), 3);
    }

    #[test]
    fn tool_tracker_detects_consecutive_repeats_across_key_order_and_upgraded_args() {
        let mut tracker = ToolTracker::default();
        // Call 1: tool_call arrives empty, permission upgrades with populated rawInput.
        tracker
            .record_update(&json!({"toolCallId": "c1", "name": "run_command", "kind": "execute"}));
        tracker.record_permission(Some(&json!({
            "toolCallId": "c1",
            "name": "run_command",
            "kind": "execute",
            "rawInput": {"b": 2, "a": 1}
        })));
        assert_eq!(tracker.repeat_streak(), 1);

        // Call 2: same args in different JSON key order.
        tracker
            .record_update(&json!({"toolCallId": "c2", "name": "run_command", "kind": "execute"}));
        tracker.record_permission(Some(&json!({
            "toolCallId": "c2",
            "name": "run_command",
            "kind": "execute",
            "rawInput": {"a": 1, "b": 2}
        })));
        assert_eq!(tracker.repeat_streak(), 2);

        // Call 3: identical again -> streak reaches 3.
        tracker.record_update(&json!({
            "toolCallId": "c3",
            "name": "run_command",
            "kind": "execute",
            "rawInput": {"a": 1, "b": 2}
        }));
        assert_eq!(tracker.repeat_streak(), 3);
        assert_eq!(tracker.last_tool_name(), "run_command");

        // Different call resets streak to 1.
        tracker.record_update(&json!({
            "toolCallId": "c4",
            "name": "run_command",
            "kind": "execute",
            "rawInput": {"a": 9, "b": 2}
        }));
        assert_eq!(tracker.repeat_streak(), 1);
    }

    #[test]
    fn suppress_token_matching_handles_wrappers_and_rejects_alerts_or_denials() {
        for matched in [
            "HEARTBEAT_OK",
            "  HEARTBEAT_OK\n",
            "HEARTBEAT_OK.",
            "HEARTBEAT_OK!",
            "`HEARTBEAT_OK`",
            "**HEARTBEAT_OK**",
            "```\nHEARTBEAT_OK\n```",
            "```text\nHEARTBEAT_OK.\n```",
        ] {
            assert!(is_suppressed(matched, "HEARTBEAT_OK", &[]), "{matched:?}");
        }
        for unmatched in [
            "heartbeat_ok",
            "HEARTBEAT_OK: disk 90% full",
            "not HEARTBEAT_OK",
            "",
        ] {
            assert!(
                !is_suppressed(unmatched, "HEARTBEAT_OK", &[]),
                "{unmatched:?}"
            );
        }
        // Denied commands must never be suppressed silently.
        assert!(!is_suppressed(
            "HEARTBEAT_OK",
            "HEARTBEAT_OK",
            &["sudo id".to_string()]
        ));
    }
}
