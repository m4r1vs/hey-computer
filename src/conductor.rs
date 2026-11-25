//! The conductor: multi-lane coordinator that owns the ACP agent process and
//! runs one interactive lane alongside a bounded FIFO background task lane.
//!
//! All concurrent turns share one warm `acp::Connection` through cloneable
//! `acp::Client` handles and per-`sessionId` subscriptions, while lifecycle
//! transitions (start, restart, idle stop, follow-up tracking, queueing, and
//! shutdown) stay serialized inside `Conductor::run`.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
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
use crate::protocol::{
    AgentState, FailReason, InputSource, Reply, SpeakAction, Status, WidgetSpec,
};
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
/// How often an idle warm conductor checks whether configured memory files changed.
const MEMORY_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Upper bound on internal `<agent_dir>/.../mcp/` schema `read_file` calls
/// exempted from `max_tool_calls` within a single turn.
const MAX_EXEMPT_SCHEMA_READS: usize = 64;
/// Maximum turns loaded from `history.db` when seeding a closed `reply_to` thread.
const MAX_SEEDED_HISTORY_TURNS: usize = 12;
/// Maximum characters injected inside `<untrusted_conversation_history>`.
const MAX_SEEDED_HISTORY_CHARS: usize = 24_000;

/// Where a request's result goes.
pub enum Sink {
    /// The daemon shows it.
    Present,
    /// A waiting client gets it, and nothing is shown.
    Client(oneshot::Sender<Reply>),
    /// A streaming waiting client gets incremental `Progress` / `Chunk` /
    /// `Widget` / `Reset` events followed by the terminal `Answer` / `Failed`.
    Stream(mpsc::UnboundedSender<Reply>),
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
    pub reply_to: Option<u64>,
    pub distill: bool,
    pub schedule_id: Option<u64>,
    pub schedule_has_script: bool,
}

/// What became of a request.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Answer {
        text: String,
        denied: Vec<String>,
        suppressed: bool,
        widgets: Vec<WidgetSpec>,
        conversation_id: Option<u64>,
        profile: Option<String>,
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
                widgets,
                conversation_id,
                profile,
            } => Reply::Answer {
                id,
                text,
                denied,
                suppressed,
                widgets,
                conversation_id,
                profile,
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
    pub reply_to: Option<u64>,
    pub reply_to_root_conv_id: Option<u64>,
    pub auto_routed_fast: bool,
    pub auto_backgrounded: bool,
    pub pre_progress_shown: bool,
    pub speech_started: bool,
    pub captured_ambient: Option<Option<String>>,
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
    /// Most recent auto-backgrounded task id, so a bare `cancel` while the
    /// interactive lane is idle can cancel a just-promoted background task.
    last_auto_bg: Option<u64>,
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
    presenter: Arc<dyn Presenter>,
    state_dir: Option<PathBuf>,
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
    /// or an error if the background queue is full or `reply_to` does not exist.
    pub fn ask(&self, params: AskParams, sink: Sink) -> std::result::Result<u64, String> {
        let mut effective_profile = params.profile;
        let mut reply_to_root_conv_id = None;
        if let Some(target_id) = params.reply_to {
            if self.store.is_some()
                && let Some(sd) = &self.state_dir
            {
                let target = store::get_turn(sd, target_id)
                    .map_err(|e| format!("reading turn #{target_id}: {e:#}"))?
                    .ok_or_else(|| format!("turn #{target_id} not found in history"))?;
                reply_to_root_conv_id = Some(target.conversation_id.unwrap_or(target.id));
                if effective_profile.is_none()
                    && !target.profile.is_empty()
                    && self.config.resolve_profile(Some(&target.profile)).is_ok()
                {
                    effective_profile = Some(target.profile);
                }
            } else {
                reply_to_root_conv_id = Some(target_id);
            }
        }

        let token = Arc::new(CancelToken::default());
        let mut effective_background = params.background;
        let mut auto_backgrounded = false;
        let id = {
            let mut shared = self
                .shared
                .lock()
                .expect("the shared state is never poisoned");
            // Tier 1 auto-backgrounding fast-path: if heuristics unambiguously
            // classify a new non-waiting request onto the background lane and
            // there is room in the background queue, place it directly onto
            // `bg_tasks` without superseding an active interactive request.
            if !effective_background
                && self.config.routing.enabled
                && effective_profile.is_none()
                && matches!(sink, Sink::Present)
                && shared.bg_queued < self.config.tasks.max_queued
            {
                let now = Instant::now();
                let is_followup = params.reply_to.is_some()
                    || (!params.new && shared.follow_up_until.is_some_and(|u| u > now));
                if !is_followup
                    && let HeuristicResult::Decided(RouteDecision {
                        lane: LaneChoice::Background,
                        profile: tier,
                    }) = routing::classify_heuristic(
                        &params.text,
                        !params.attachments.is_empty(),
                        false,
                        true,
                        self.config.routing.auto_background,
                    )
                {
                    effective_background = true;
                    auto_backgrounded = true;
                    effective_profile = Some(match tier {
                        TierChoice::Fast => self.config.routing.fast_profile.clone(),
                        TierChoice::Deep => self.config.routing.deep_profile.clone(),
                    });
                }
            }
            if effective_background {
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
                if auto_backgrounded {
                    shared.last_auto_bg = Some(id);
                }
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
        if !effective_background {
            // Barge-in: new interactive requests silence any ongoing TTS playback.
            self.presenter.stop_speech(None);
        }
        let lane = if effective_background {
            Lane::Background
        } else {
            Lane::Interactive
        };
        let stored_profile = effective_profile
            .as_deref()
            .or(if effective_background {
                self.config.tasks.profile.as_deref()
            } else {
                None
            })
            .filter(|p| *p != "default")
            .unwrap_or("")
            .to_string();
        let origin = if params.distill {
            store::TurnOrigin::Distill
        } else if params.schedule_id.is_some() && params.schedule_has_script {
            store::TurnOrigin::ScheduleScript
        } else if params.schedule_id.is_some() {
            store::TurnOrigin::Schedule
        } else {
            store::TurnOrigin::from_input_source(params.source)
        };
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
                origin,
                schedule_id: params.schedule_id,
            });
        }
        let ask = Ask {
            id,
            text: params.text,
            new: params.new,
            profile: effective_profile,
            background: effective_background,
            suppress_token: params.suppress_token,
            attachments: params.attachments,
            source: params.source,
            reply_to: params.reply_to,
            reply_to_root_conv_id,
            auto_routed_fast: false,
            auto_backgrounded,
            pre_progress_shown: false,
            speech_started: false,
            captured_ambient: None,
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
                if shared.last_auto_bg == Some(ask.id) {
                    shared.last_auto_bg = None;
                }
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

    /// Cancel the active interactive request (`only == None`, falling back to
    /// the most recent auto-backgrounded task when the interactive lane is
    /// idle), a specific interactive or background request (`only == Some(id)`),
    /// or active post-turn TTS playback.
    pub fn cancel(&self, only: Option<u64>, reason: FailReason) -> Option<u64> {
        let speech_stopped = self.presenter.stop_speech(only);
        let cancelled = self.shared.lock().ok().and_then(|shared| match only {
            None => {
                let fg_cancelled = shared
                    .fg_latest
                    .as_ref()
                    .and_then(|(id, token)| token.cancel(reason).then_some(*id));
                fg_cancelled.or_else(|| {
                    let id = shared.last_auto_bg?;
                    let token = shared.bg_tasks.get(&id)?;
                    token.cancel(reason).then_some(id)
                })
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
        });
        if cancelled.is_some() {
            let _ = self.messages.send(Message::Wake);
        }
        cancelled.or(speech_stopped)
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
        let (speak, speaking) = self.presenter.speak_status();
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
            speak,
            speaking,
            running_schedules: Vec::new(),
        }
    }

    /// Start on-demand document/selection TTS playback via the presenter.
    pub fn read_aloud(
        &self,
        text: String,
    ) -> std::result::Result<(u64, oneshot::Receiver<()>), String> {
        self.presenter.read_aloud(text)
    }

    /// Immediately stop active or queued TTS speech (optionally scoped to `only`).
    pub fn stop_speech(&self, only: Option<u64>) -> Option<u64> {
        self.presenter.stop_speech(only)
    }

    pub fn control_speak(&self, action: SpeakAction) -> (bool, bool) {
        let (current_enabled, current_speaking) = self.presenter.speak_status();
        let next = match action {
            SpeakAction::Status => return (current_enabled, current_speaking),
            SpeakAction::On => true,
            SpeakAction::Off => false,
            SpeakAction::Toggle => !current_enabled,
        };
        self.presenter.set_speak_enabled(next);
        if let Some(state_dir) = &self.state_dir
            && let Err(e) = store::save_speak_state(state_dir, next)
        {
            tracing::warn!(
                "cannot persist speak state in {}: {e:#}",
                state_dir.display()
            );
        }
        if !next {
            self.presenter.stop_speech(None);
        }
        self.presenter.speak_status()
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
    let state_dir = if !config.state_dir.as_os_str().is_empty() || store.is_some() {
        config.state_dir().ok()
    } else {
        None
    };
    let initial_speak = match &state_dir {
        Some(sd) => store::load_speak_state(sd, config.present.speak.enabled),
        None => config.present.speak.enabled,
    };
    presenter.set_speak_enabled(initial_speak);

    let shared = Arc::new(Mutex::new(Shared {
        next_id: initial_last_id,
        fg_latest: None,
        last_auto_bg: None,
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
    let mut known_mcp_servers: Vec<String> = config.mcp.keys().cloned().collect();
    if !known_mcp_servers
        .iter()
        .any(|s| s == crate::mcp::INTERNAL_UI_MCP_SERVER)
    {
        known_mcp_servers.push(crate::mcp::INTERNAL_UI_MCP_SERVER.to_string());
    }
    let conductor = Conductor {
        policy: Arc::new(
            LanePolicy::standard(&config.permissions.deny)
                .with_known_mcp_servers(known_mcp_servers),
        ),
        config: Arc::clone(&config),
        presenter: Arc::clone(&presenter),
        context,
        store: store.clone(),
        state_dir: state_dir.clone(),
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
        next_memory_check: None,
        last_used: Instant::now(),
        last_death_detail: None,
    };
    let task = tokio::spawn(conductor.run());
    (
        Handle {
            config,
            presenter,
            state_dir,
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
    /// Most recent completed turn id in this live session (for Path A `reply_to`).
    last_turn_id: Option<u64>,
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
    conv_id: Option<u64>,
    used_profile: Option<String>,
    started: Instant,
    end: TurnEnd,
}

struct Conductor {
    config: Arc<Config>,
    presenter: Arc<dyn Presenter>,
    context: Arc<dyn ContextProvider>,
    store: Option<Store>,
    state_dir: Option<PathBuf>,
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
    next_memory_check: Option<Instant>,
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
    speech: Option<&'a Arc<dyn Presenter>>,
    voice_stop: Option<&'a Arc<dyn Presenter>>,
    stream_tx: Option<&'a mpsc::UnboundedSender<Reply>>,
    agent_dir: Option<&'a Path>,
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
/// exempts internal `<agent_dir>/.../mcp/` lazy schema `read_file` calls from
/// `max_tool_calls`, and detects consecutive identical tool-call retry loops
/// without falsely tripping on parallel argument-less `tool_call` stubs.
#[derive(Default)]
struct ToolTracker {
    agent_dir: Option<PathBuf>,
    seen: HashSet<String>,
    calls: Vec<TrackedCall>,
    exempt_count: usize,
}

struct TrackedCall {
    id: Option<String>,
    sig: (String, String),
    has_args: bool,
    confirmed: bool,
    is_read: bool,
    exempt_schema_read: bool,
    pending_path: bool,
}

impl ToolTracker {
    fn new(agent_dir: Option<PathBuf>) -> Self {
        Self {
            agent_dir,
            ..Self::default()
        }
    }

    fn classify_read(&mut self, call: &Value) -> (bool, bool, bool) {
        let is_read = is_read_tool(call);
        if !is_read || self.agent_dir.is_none() {
            return (is_read, false, false);
        }
        match extract_read_path(call) {
            Some(path) => {
                if self.exempt_count < MAX_EXEMPT_SCHEMA_READS
                    && is_internal_mcp_schema_path(&path, self.agent_dir.as_deref())
                {
                    self.exempt_count += 1;
                    (true, true, false)
                } else {
                    (true, false, false)
                }
            }
            None => {
                let pending = self.exempt_count < MAX_EXEMPT_SCHEMA_READS;
                (true, false, pending)
            }
        }
    }

    fn record_update(&mut self, call: &Value) -> bool {
        let id = acp::tool_call_id(call);
        let (sig, has_args) = tool_signature(call);
        match id {
            Some(id) => {
                if self.seen.insert(id.to_string()) {
                    let (is_read, exempt_schema_read, pending_path) = self.classify_read(call);
                    self.calls.push(TrackedCall {
                        id: Some(id.to_string()),
                        sig,
                        has_args,
                        confirmed: has_args,
                        is_read,
                        exempt_schema_read,
                        pending_path,
                    });
                    true
                } else {
                    let resolved_path = extract_read_path(call);
                    self.upgrade(id, sig, has_args, has_args, resolved_path.as_deref(), false);
                    false
                }
            }
            None => {
                let (is_read, exempt_schema_read, _) = self.classify_read(call);
                self.calls.push(TrackedCall {
                    id: None,
                    sig,
                    has_args,
                    confirmed: has_args,
                    is_read,
                    exempt_schema_read,
                    pending_path: false,
                });
                true
            }
        }
    }

    fn record_followup_update(&mut self, call: &Value) {
        let Some(id) = acp::tool_call_id(call) else {
            return;
        };
        if !self.seen.contains(id) {
            return;
        }
        let (sig, has_args) = tool_signature(call);
        let terminal = matches!(
            call.get("status").and_then(Value::as_str),
            Some("completed" | "failed" | "cancelled")
        );
        let resolved_path = extract_read_path(call);
        self.upgrade(
            id,
            sig,
            has_args,
            has_args || terminal,
            resolved_path.as_deref(),
            terminal,
        );
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
            let (is_read, exempt_schema_read, _) = self.classify_read(call);
            self.calls.push(TrackedCall {
                id: Some(id.to_string()),
                sig,
                has_args,
                confirmed: true,
                is_read,
                exempt_schema_read,
                pending_path: false,
            });
            true
        } else {
            let resolved_path = extract_read_path(call);
            self.upgrade(id, sig, has_args, true, resolved_path.as_deref(), true);
            false
        }
    }

    fn upgrade(
        &mut self,
        id: &str,
        sig: (String, String),
        has_args: bool,
        confirm: bool,
        resolved_path: Option<&str>,
        finalize_pending: bool,
    ) {
        let agent_dir = self.agent_dir.clone();
        if let Some(entry) = self
            .calls
            .iter_mut()
            .rev()
            .find(|c| c.id.as_deref() == Some(id))
        {
            if has_args || (!entry.has_args && sig.1 != "{}") {
                entry.sig = sig;
                entry.has_args = entry.has_args || has_args;
            }
            if confirm {
                entry.confirmed = true;
            }
            if entry.is_read && !entry.exempt_schema_read {
                if let Some(path) = resolved_path {
                    entry.pending_path = false;
                    if self.exempt_count < MAX_EXEMPT_SCHEMA_READS
                        && is_internal_mcp_schema_path(path, agent_dir.as_deref())
                    {
                        entry.exempt_schema_read = true;
                        self.exempt_count += 1;
                    }
                } else if finalize_pending {
                    entry.pending_path = false;
                }
            }
        }
    }

    #[cfg(test)]
    fn count(&self) -> usize {
        self.calls.len()
    }

    fn budget_count(&self) -> usize {
        self.calls
            .iter()
            .filter(|c| !c.exempt_schema_read && !c.pending_path)
            .count()
    }

    fn is_exempt_call(&self, call: &Value) -> bool {
        if let Some(id) = acp::tool_call_id(call) {
            self.calls
                .iter()
                .rev()
                .find(|c| c.id.as_deref() == Some(id))
                .is_some_and(|c| c.exempt_schema_read)
        } else {
            self.calls.last().is_some_and(|c| c.exempt_schema_read)
        }
    }

    fn repeat_streak(&self) -> usize {
        let mut confirmed = self.calls.iter().rev().filter(|c| c.confirmed);
        let Some(last) = confirmed.next() else {
            return 0;
        };
        1 + confirmed.take_while(|c| c.sig == last.sig).count()
    }

    fn last_tool_name(&self) -> String {
        self.calls
            .iter()
            .rev()
            .find(|c| c.confirmed)
            .or_else(|| self.calls.last())
            .map(|c| c.sig.0.clone())
            .unwrap_or_else(|| "tool".to_string())
    }
}

fn is_read_tool(call: &Value) -> bool {
    call.get("name").and_then(Value::as_str) == Some("read_file")
        || call.get("kind").and_then(Value::as_str) == Some("read")
}

fn extract_read_path(call: &Value) -> Option<String> {
    for key in ["rawInput", "args"] {
        if let Some(input) = call.get(key)
            && let Some(path) = input
                .get("Path")
                .or_else(|| input.get("path"))
                .or_else(|| input.get("file_path"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        {
            return Some(path.to_string());
        }
    }
    call.get("locations")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|loc| loc.get("path"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn is_internal_mcp_schema_path(path_str: &str, agent_dir: Option<&Path>) -> bool {
    let Some(agent_dir) = agent_dir else {
        return false;
    };
    let path = Path::new(path_str);
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    let Ok(rel) = path.strip_prefix(agent_dir) else {
        return false;
    };
    if rel.starts_with("workspace") {
        return false;
    }
    rel.components().any(|c| {
        let s = c.as_os_str();
        s == "mcp" || s == crate::mcp::INTERNAL_UI_MCP_SERVER
    })
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
            let memory_at = self.memory_check_deadline();
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
                    Some(Message::Ask(mut ask)) => {
                        if ask.auto_backgrounded && ask.token.reason().is_none() {
                            if matches!(ask.sink, Sink::Present) {
                                let preview = attachments::progress_label(&ask.text, &ask.attachments);
                                self.presenter.present(Presentation::Notice {
                                    summary: "Working in the background".to_string(),
                                    body: preview,
                                    urgency: Urgency::Low,
                                    replaces_id: None,
                                });
                            }
                            if ask.captured_ambient.is_none() && self.config.context.is_enabled() {
                                ask.captured_ambient =
                                    Some(self.context.gather(&self.config.context).await);
                            }
                        }
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
                () = sleep_until(memory_at) => {
                    self.next_memory_check = None;
                    self.maybe_reload_changed_memory().await;
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
                    replaces_id: ask.pre_progress_shown.then_some(ask.id),
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
            if matches!(ask.sink, Sink::Present)
                && (!ask.pre_progress_shown || display_profile.is_some())
            {
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
        let (session, resolved, conv_id, seeded_history) = loop {
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
                let bg_cid = match ask.reply_to {
                    Some(target_id) => ask.reply_to_root_conv_id.unwrap_or(target_id),
                    None => ask.id,
                };
                let seeded = match ask.reply_to {
                    Some(target_id) => self.load_seeded_history(target_id).await,
                    None => None,
                };
                tokio::select! {
                    res = self.new_background_session(effective_req.as_deref()) => {
                        res.map(|(s, r)| (s, r, Some(bg_cid), seeded))
                    }
                    reason = interrupts.wait() => {
                        self.finish_request(ask, cancelled(reason), display_profile, false);
                        return;
                    }
                }
            } else if let Some(target_id) = ask.reply_to {
                tokio::select! {
                    res = self.session_for_reply_to(
                        ask.id,
                        target_id,
                        ask.reply_to_root_conv_id,
                        ask.new,
                        ask.profile.as_deref(),
                    ) => res,
                    reason = interrupts.wait() => {
                        self.finish_request(ask, cancelled(reason), display_profile, false);
                        return;
                    }
                }
            } else {
                tokio::select! {
                    res = self.session_for(ask.id, ask.new, ask.profile.as_deref()) => res.map(|(s, r)| {
                        let cid = self
                            .conversation
                            .as_ref()
                            .map(|c| c.conversation_id)
                            .or(Some(ask.id));
                        (s, r, cid, None)
                    }),
                    reason = interrupts.wait() => {
                        self.finish_request(ask, cancelled(reason), display_profile, false);
                        return;
                    }
                }
            };

            match session_result {
                Ok(quad) => break quad,
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
            let shown = self.display_profile_name(resolved.name.as_deref());
            self.finish_request(
                ask,
                Outcome::failed(FailReason::AgentDown, "the assistant exited"),
                shown,
                false,
            );
            return;
        };

        let used_profile = resolved.name.clone();
        let shown_profile = self.display_profile_name(used_profile.as_deref());
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
                shared.running = Some((ask.id, started, shown_profile));
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
        let emit_speech = !ask.background
            && ask.suppress_token.is_none()
            && matches!(ask.sink, Sink::Present)
            && (ask.source == InputSource::Voice || !self.config.present.speak.voice_only);
        ask.speech_started = emit_speech;
        let speech_presenter = emit_speech.then(|| Arc::clone(&self.presenter));
        let voice_stop_presenter =
            (emit_speech || ask.source == InputSource::Voice).then(|| Arc::clone(&self.presenter));
        let stream_tx = match &ask.sink {
            Sink::Stream(tx) => Some(tx.clone()),
            Sink::Present | Sink::Client(_) => None,
        };
        let agent_dir = self.paths.as_ref().map(|p| p.agent_dir.clone());
        let done_tx = self.done_tx.clone();
        let stop = Arc::clone(&self.stop);

        tokio::spawn(async move {
            let token = Arc::clone(&ask.token);
            let interrupts = Interrupts {
                request: &token,
                stop: &stop,
            };
            let ambient = match ask.captured_ambient.take() {
                Some(snapshot) => snapshot,
                None if config.context.is_enabled() => {
                    let bg_ctx = crate::config::Context {
                        time: config.context.time,
                        desktop: false,
                    };
                    let effective_ctx = if ask.background {
                        &bg_ctx
                    } else {
                        &config.context
                    };
                    if effective_ctx.is_enabled() {
                        tokio::select! {
                            header = context_provider.gather(effective_ctx) => header,
                            reason = interrupts.wait() => {
                                let _ = done_tx.send(TurnFinished {
                                    ask,
                                    session,
                                    conv_id,
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
                    }
                }
                None => None,
            };
            let budget_header = if config.context.is_enabled() || used_profile.is_some() {
                context::format_turn_budget(
                    used_profile.as_deref(),
                    budgets.max_tool_calls,
                    budgets.max_turn,
                )
            } else {
                None
            };
            let modality_header = context::format_modality(ask.source);
            let mut header_parts: Vec<&str> = Vec::new();
            if let Some(b) = budget_header.as_deref() {
                header_parts.push(b);
            }
            if let Some(m) = modality_header {
                header_parts.push(m);
            }
            if let Some(a) = ambient.as_deref() {
                header_parts.push(a);
            }
            if let Some(h) = seeded_history.as_deref() {
                header_parts.push(h);
            }
            let header = (!header_parts.is_empty()).then(|| header_parts.join("\n\n"));
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
                speech: speech_presenter.as_ref(),
                voice_stop: voice_stop_presenter.as_ref(),
                stream_tx: stream_tx.as_ref(),
                agent_dir: agent_dir.as_deref(),
                id: ask.id,
            };
            let end = turn(&client, subscription, &session, blocks, interrupts, &ctx).await;
            let _ = done_tx.send(TurnFinished {
                ask,
                session,
                conv_id,
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
            conv_id,
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

        if let Outcome::Answer {
            conversation_id,
            profile,
            ..
        } = &mut end.outcome
        {
            *conversation_id = conv_id.or(Some(ask.id));
            *profile = used_profile.clone();
        }

        // Apply --suppress-token check before updating follow-up window or persistence.
        if let Some(token) = ask.suppress_token.as_deref()
            && let Outcome::Answer {
                text,
                denied,
                suppressed,
                widgets,
                ..
            } = &mut end.outcome
            && widgets.is_empty()
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
                if matches!(
                    end.outcome,
                    Outcome::Answer {
                        suppressed: false,
                        ..
                    }
                ) {
                    conversation.last = Instant::now();
                    conversation.turns += 1;
                    conversation.last_turn_id = Some(ask.id);
                    let until = secs(self.config.conversation.follow_up_secs)
                        .and_then(|window| conversation.last.checked_add(window));
                    self.set_follow_up(until);
                } else if conversation.turns == 0 && self.fg_pending.is_none() {
                    if let Some(old) = self.conversation.take()
                        && let Some(live) = &self.live
                    {
                        drop(
                            live.conn
                                .start_call("session/close", json!({"sessionId": old.session})),
                        );
                    }
                    self.set_follow_up(None);
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
            self.presenter.stop_speech(Some(ask.id));
            if let Sink::Stream(ref tx) = ask.sink {
                let _ = tx.send(Reply::Reset { id: ask.id });
            }
            let mut retry = ask;
            retry.speech_started = false;
            retry.escalated_once = true;
            retry.auto_routed_fast = false;
            retry.profile = Some(self.config.routing.deep_profile.clone());
            if retry.reply_to.is_none() {
                retry.new = true;
            }
            self.fg_pending = Some(retry);
            return;
        }

        self.last_used = Instant::now();
        log_outcome(ask.id, &end.outcome, started);
        let display_profile = self.display_profile_name(used_profile.as_deref());
        self.finish_request(ask, end.outcome, display_profile, false);
    }

    /// Apply Tier 1 heuristics and (for non-voice unsure new turns) Tier 3
    /// restricted LLM triage. Returns `Some(reason)` if cancelled during triage.
    async fn apply_routing(&mut self, ask: &mut Ask) -> Option<FailReason> {
        let is_followup = ask.reply_to.is_some() || (!ask.new && self.is_in_follow_up_window());
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
                    if matches!(ask.sink, Sink::Present) && !ask.pre_progress_shown {
                        self.presenter.present(Presentation::Progress {
                            id: ask.id,
                            request: attachments::progress_label(&ask.text, &ask.attachments),
                            profile: None,
                            thinking_label: None,
                        });
                        ask.pre_progress_shown = true;
                    }
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
                    if shared
                        .running
                        .as_ref()
                        .is_some_and(|(id, _, _)| *id == ask.id)
                    {
                        shared.running = None;
                    }
                    shared.bg_tasks.insert(ask.id, Arc::clone(&ask.token));
                    shared.bg_queued += 1;
                    shared.last_auto_bg = Some(ask.id);
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if promoted {
                ask.background = true;
                ask.auto_backgrounded = true;
                if ask.captured_ambient.is_none() && self.config.context.is_enabled() {
                    ask.captured_ambient = Some(self.context.gather(&self.config.context).await);
                }
                if let Some(store) = &self.store {
                    let stored_prof = ask
                        .profile
                        .as_deref()
                        .filter(|p| *p != "default")
                        .unwrap_or("")
                        .to_string();
                    store.promote_to_background(ask.id, stored_prof);
                }
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
        let result = self.run_triage_inner(ask, can_background).await;
        if let Ok(mut shared) = self.shared.lock()
            && shared
                .running
                .as_ref()
                .is_some_and(|(id, _, _)| *id == ask.id)
        {
            shared.running = None;
        }
        result
    }

    async fn run_triage_inner(
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
            speech: None,
            voice_stop: None,
            stream_tx: None,
            agent_dir: None,
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
                if !self.has_active_turns() {
                    self.stop_agent().await;
                    self.schedule_restart_if_warm();
                } else if let Some(live) = &mut self.live {
                    live.draining = true;
                }
            } else if let Some(live) = &self.live {
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
            if shared.last_auto_bg == Some(ask.id) {
                shared.last_auto_bg = None;
            }
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
                Outcome::Answer {
                    text,
                    denied,
                    widgets,
                    ..
                } => {
                    store.finish_completed_with_widgets(
                        ask.id,
                        text.clone(),
                        denied.clone(),
                        widgets.clone(),
                        now_ms,
                    );
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

    fn display_profile_name(&self, name: Option<&str>) -> Option<String> {
        let name = name?.trim();
        if name.is_empty() || name == "default" {
            return None;
        }
        // When smart routing is enabled, suppress routine `(fast)` badge noise
        // unless the user configured a custom `thinking_label` for it.
        if self.config.routing.enabled
            && name == self.config.routing.fast_profile
            && self
                .config
                .profiles
                .get(name)
                .and_then(|p| p.thinking_label.as_deref())
                .is_none()
        {
            return None;
        }
        Some(name.to_string())
    }

    fn effective_display_profile(&self, ask: &Ask) -> Option<String> {
        let raw = ask.profile.as_deref().or(if ask.background {
            self.config.tasks.profile.as_deref()
        } else {
            None
        });
        match raw {
            Some(name) => self.display_profile_name(Some(name)),
            None if !ask.background => {
                if !ask.new && self.is_in_follow_up_window() {
                    let conv_prof = self
                        .conversation
                        .as_ref()
                        .and_then(|c| c.profile_name.as_deref());
                    self.display_profile_name(conv_prof)
                } else {
                    None
                }
            }
            None => None,
        }
    }

    fn is_in_follow_up_window(&self) -> bool {
        let window = secs(self.config.conversation.follow_up_secs);
        self.conversation
            .as_ref()
            .is_some_and(|c| c.turns > 0 && window.is_some_and(|w| c.last.elapsed() < w))
    }

    fn memory_check_deadline(&mut self) -> Option<Instant> {
        self.live.as_ref()?;
        if self.has_active_turns() || self.fg_pending.is_some() || !self.bg_queue.is_empty() {
            self.next_memory_check = None;
            return None;
        }
        if self.config.instructions.memory_file.as_os_str().is_empty()
            && self.config.instructions.memory_files.is_empty()
            && self.config.user_skills_dir().is_none()
        {
            return None;
        }
        if let Some(conv) = &self.conversation
            && conv.turns > 0
            && let Some(window) = secs(self.config.conversation.follow_up_secs)
            && let Some(until) = conv.last.checked_add(window)
            && until > Instant::now()
        {
            let after_followup = until + Duration::from_millis(50);
            self.next_memory_check = Some(after_followup);
            return Some(after_followup);
        }
        Some(
            *self
                .next_memory_check
                .get_or_insert_with(|| Instant::now() + MEMORY_POLL_INTERVAL),
        )
    }

    async fn maybe_reload_changed_memory(&mut self) {
        if self.live.is_none()
            || self.has_active_turns()
            || self.fg_pending.is_some()
            || !self.bg_queue.is_empty()
            || self.is_in_follow_up_window()
        {
            return;
        }
        let Some(prev_fp) = self.paths.as_ref().map(|p| p.memory_fingerprint) else {
            return;
        };
        if environment::compute_memory_fingerprint(&self.config) == prev_fp {
            return;
        }
        tracing::info!("memory file changed while idle; refreshing agent instructions");
        self.stop_agent().await;
        if self.config.agent.warm_start && self.stop.reason().is_none() {
            let stop = Arc::clone(&self.stop);
            tokio::select! {
                () = self.start_warm() => {}
                _ = stop.cancelled() => {}
            }
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
            last_turn_id: None,
            profile_name: resolved.name.clone(),
            model: resolved.model.clone(),
            thought_level: resolved.thought_level.clone(),
            last: Instant::now(),
            turns: 0,
        });
        Ok((session, resolved))
    }

    async fn load_seeded_history(&self, target_id: u64) -> Option<String> {
        let state_dir = self.state_dir.clone()?;
        if let Some(store) = &self.store {
            store.flush().await;
        }
        let turns = tokio::task::spawn_blocking(move || {
            store::conversation_turns(&state_dir, target_id, false, MAX_SEEDED_HISTORY_TURNS)
        })
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
        context::format_conversation_history(&turns, MAX_SEEDED_HISTORY_CHARS)
    }

    /// Resume a conversation explicitly targeted by `reply_to`:
    /// - Path A: reuse the live ACP session in place when it matches `root_conv_id`
    ///   and its latest completed turn is `target_id` (even past `follow_up_secs`).
    /// - Path B: otherwise load `history.db` turns up to `target_id`, open a fresh
    ///   ACP session linked to `root_conv_id`, and seed `<untrusted_conversation_history>`.
    async fn session_for_reply_to(
        &mut self,
        _turn_id: u64,
        target_id: u64,
        root_hint: Option<u64>,
        new: bool,
        requested: Option<&str>,
    ) -> Result<(String, ResolvedProfile, Option<u64>, Option<String>), Outcome> {
        let root_conv_id = root_hint
            .or_else(|| {
                self.conversation
                    .as_ref()
                    .filter(|c| c.last_turn_id == Some(target_id))
                    .map(|c| c.conversation_id)
            })
            .unwrap_or(target_id);

        if !new
            && let Some(conversation) = &self.conversation
            && conversation.conversation_id == root_conv_id
            && conversation.last_turn_id == Some(target_id)
        {
            if requested.is_none() {
                let resolved = self
                    .config
                    .resolve_profile(conversation.profile_name.as_deref())
                    .map_err(|e| Outcome::failed(FailReason::AgentError, e))?;
                tracing::info!(
                    "resuming live session for reply_to #{} (turn {})",
                    target_id,
                    conversation.turns + 1
                );
                return Ok((
                    conversation.session.clone(),
                    resolved,
                    Some(root_conv_id),
                    None,
                ));
            }

            let resolved = self
                .config
                .resolve_profile(requested)
                .map_err(|e| Outcome::failed(FailReason::AgentError, e))?;
            if resolved.model == conversation.model
                && resolved.thought_level == conversation.thought_level
            {
                let session = conversation.session.clone();
                let turns = conversation.turns + 1;
                if let Some(conv) = &mut self.conversation {
                    conv.profile_name = resolved.name.clone();
                }
                tracing::info!("resuming live session for reply_to #{target_id} (turn {turns})");
                return Ok((session, resolved, Some(root_conv_id), None));
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
                    tracing::info!(
                        "switched profile in place on reply_to #{target_id} (turn {turns})"
                    );
                    return Ok((session, resolved, Some(root_conv_id), None));
                }
                false => {
                    tracing::warn!(
                        "mid-conversation profile switch refused on reply_to #{target_id}; seeding new session from history"
                    );
                }
            }
        }

        let seeded = self.load_seeded_history(target_id).await;
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
        tracing::info!(
            "opened DB-seeded session for reply_to #{target_id} (root conversation #{root_conv_id})"
        );
        self.conversation = Some(Conversation {
            session: session.clone(),
            conversation_id: root_conv_id,
            last_turn_id: None,
            profile_name: resolved.name.clone(),
            model: resolved.model.clone(),
            thought_level: resolved.thought_level.clone(),
            last: Instant::now(),
            turns: 0,
        });
        Ok((session, resolved, Some(root_conv_id), seeded))
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
        Sink::Stream(tx) => {
            let _ = tx.send(outcome.into_reply(ask.id));
        }
        Sink::Present => {
            let Some(presenter) = presenter else {
                return;
            };
            match outcome {
                Outcome::Answer {
                    suppressed: true, ..
                } => {
                    if ask.speech_started {
                        presenter.stop_speech(Some(ask.id));
                    }
                    if !ask.background {
                        presenter.present(Presentation::DismissProgress { id: ask.id });
                    }
                }
                Outcome::Answer {
                    text,
                    denied,
                    widgets,
                    ..
                } => {
                    if ask.speech_started {
                        presenter.present(Presentation::SpeechFinish {
                            id: ask.id,
                            full_text: text.clone(),
                            has_widgets: !widgets.is_empty(),
                        });
                    }
                    if ask.background {
                        presenter.present(Presentation::TaskComplete {
                            id: ask.id,
                            request: ask.text,
                            text,
                            denied,
                            profile,
                            widgets,
                            icons: Vec::new(),
                        });
                    } else {
                        presenter.present(Presentation::Answer {
                            id: ask.id,
                            text,
                            denied,
                            profile,
                            widgets,
                            icons: Vec::new(),
                        });
                    }
                }
                Outcome::Failed { reason, message } => {
                    if ask.speech_started {
                        presenter.stop_speech(Some(ask.id));
                    }
                    presenter.present(Presentation::Failure {
                        id: ask.id,
                        reason,
                        message,
                        profile,
                        background: ask.background,
                        icons: Vec::new(),
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
            widgets,
            ..
        } => {
            tracing::info!(
                "request {id}: {} in {elapsed:.2} s, {} characters, {} widgets, {} refused",
                if *suppressed {
                    "suppressed"
                } else {
                    "answered"
                },
                text.chars().count(),
                widgets.len(),
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

fn parse_mcp_ui_widget(
    tool_name: &str,
    args: &Value,
    fallback_idx: usize,
) -> std::result::Result<WidgetSpec, String> {
    match tool_name {
        "show_widget" => crate::mcp::parse_show_widget_args(args, fallback_idx),
        "propose_widget_template" => crate::mcp::parse_propose_template_args(args, fallback_idx),
        other => Err(format!("unsupported mcp-ui tool `{other}`")),
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
    if let Some(sp) = ctx.speech {
        sp.present(Presentation::SpeechStart { id });
    }
    let mut response = client.start_call(
        "session/prompt",
        json!({"sessionId": session, "prompt": prompt_blocks}),
    );
    let mut answer = String::new();
    let mut denied: Vec<String> = Vec::new();
    let mut tools: Vec<String> = Vec::new();
    let mut ui_calls: HashMap<String, (String, bool)> = HashMap::new();
    let mut pending_widgets: HashMap<String, WidgetSpec> = HashMap::new();
    let mut committed_widgets: HashSet<String> = HashSet::new();
    let mut widgets: Vec<WidgetSpec> = Vec::new();
    let mut widget_seq: usize = 0;
    let mut tracker = ToolTracker::new(ctx.agent_dir.map(Path::to_path_buf));
    let mut last_progress: Option<String> = None;
    let mut first_text: Option<Duration> = None;
    let mut deadline = Instant::now() + budgets.inactivity;
    let turn_deadline = budgets.max_turn.and_then(|d| started.checked_add(d));
    let turn_sleep = sleep_until(turn_deadline);
    tokio::pin!(turn_sleep);
    let at = || started.elapsed().as_secs_f64();
    let emit_progress = |last: &mut Option<String>, status: Option<String>| {
        if ctx.progress.is_none() && ctx.stream_tx.is_none() {
            return;
        }
        let Some(status) = status else {
            return;
        };
        if last.as_deref() == Some(status.as_str()) {
            return;
        }
        *last = Some(status.clone());
        if let Some(presenter) = ctx.progress {
            presenter.present(Presentation::ProgressUpdate {
                id,
                status: status.clone(),
                icons: Vec::new(),
            });
        }
        if let Some(tx) = ctx.stream_tx {
            let _ = tx.send(Reply::Progress { id, status });
        }
    };
    let stage_widget = |call: &Value,
                        ui_calls: &mut HashMap<String, (String, bool)>,
                        pending_widgets: &mut HashMap<String, WidgetSpec>,
                        widget_seq: &mut usize| {
        if let Some(ui) = acp::extract_mcp_ui_call(call) {
            let is_lazy = call.get("name").and_then(Value::as_str) == Some("call_mcp_tool");
            if let Some(ref tc_id) = ui.tool_call_id {
                ui_calls.insert(tc_id.clone(), (ui.tool_name.clone(), is_lazy));
            }
            if let Some(ref args) = ui.arguments {
                *widget_seq += 1;
                if let Ok(spec) = parse_mcp_ui_widget(&ui.tool_name, args, *widget_seq)
                    && let Some(tc_id) = ui.tool_call_id
                {
                    pending_widgets.insert(tc_id, spec);
                }
            }
        } else if let Some(tc_id) = acp::tool_call_id(call)
            && let Some((tool_name, is_lazy)) = ui_calls.get(tc_id).cloned()
            && let Some(args) = acp::extract_mcp_ui_arguments(call, is_lazy)
        {
            *widget_seq += 1;
            if let Ok(spec) = parse_mcp_ui_widget(&tool_name, &args, *widget_seq) {
                pending_widgets.insert(tc_id.to_string(), spec);
            }
        }
    };
    let commit_widget = |tc_id: &str,
                         pending_widgets: &mut HashMap<String, WidgetSpec>,
                         committed_widgets: &mut HashSet<String>,
                         widgets: &mut Vec<WidgetSpec>| {
        if let Some(widget) = pending_widgets.remove(tc_id)
            && committed_widgets.insert(tc_id.to_string())
        {
            if let Some(vp) = ctx.voice_stop {
                vp.stop_speech(Some(id));
            }
            if let Some(tx) = ctx.stream_tx {
                let _ = tx.send(Reply::Widget {
                    id,
                    widget: widget.clone(),
                });
            }
            widgets.push(widget);
        }
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
                    if !chunk.is_empty() {
                        first_text.get_or_insert_with(|| started.elapsed());
                        answer.push_str(chunk);
                        if let Some(sp) = ctx.speech {
                            sp.present(Presentation::SpeechChunk {
                                id,
                                delta: chunk.to_string(),
                            });
                        }
                        if let Some(tx) = ctx.stream_tx {
                            let _ = tx.send(Reply::Chunk {
                                id,
                                text: chunk.to_string(),
                            });
                        }
                    }
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
                            }
                            if let Some(limit) = budgets.max_tool_calls
                                && tracker.budget_count() > limit
                            {
                                break Err(TurnAbort::ToolLimit(limit));
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
                                stage_widget(
                                    &update,
                                    &mut ui_calls,
                                    &mut pending_widgets,
                                    &mut widget_seq,
                                );
                                if update.get("status").and_then(Value::as_str) == Some("completed")
                                    && let Some(tc_id) = acp::tool_call_id(&update)
                                {
                                    commit_widget(
                                        tc_id,
                                        &mut pending_widgets,
                                        &mut committed_widgets,
                                        &mut widgets,
                                    );
                                }
                                if !tracker.is_exempt_call(&update) {
                                    emit_progress(&mut last_progress, acp::tool_progress(&update));
                                }
                            }
                        }
                        Some("tool_call_update") => {
                            tracker.record_followup_update(&update);
                            if let Some(limit) = budgets.max_tool_calls
                                && tracker.budget_count() > limit
                            {
                                break Err(TurnAbort::ToolLimit(limit));
                            }
                            if let Some(limit) = budgets.max_repeat_tool_calls
                                && tracker.repeat_streak() >= limit
                            {
                                break Err(TurnAbort::RepeatToolLoop {
                                    count: tracker.repeat_streak(),
                                    tool: tracker.last_tool_name(),
                                });
                            }
                            stage_widget(
                                &update,
                                &mut ui_calls,
                                &mut pending_widgets,
                                &mut widget_seq,
                            );
                            match update.get("status").and_then(Value::as_str) {
                                Some("completed") => {
                                    if let Some(tc_id) = acp::tool_call_id(&update) {
                                        commit_widget(
                                            tc_id,
                                            &mut pending_widgets,
                                            &mut committed_widgets,
                                            &mut widgets,
                                        );
                                    }
                                }
                                Some("failed" | "cancelled") => {
                                    if let Some(tc_id) = acp::tool_call_id(&update) {
                                        pending_widgets.remove(tc_id);
                                    }
                                }
                                _ => {}
                            }
                            if !tracker.is_exempt_call(&update) {
                                emit_progress(&mut last_progress, acp::tool_progress(&update));
                            }
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
                    && tracker.budget_count() > limit
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
                        if let Some(tc_id) = params.get("toolCall").and_then(acp::tool_call_id) {
                            pending_widgets.remove(tc_id);
                            ui_calls.remove(tc_id);
                        }
                        denied.push(detail);
                        acp::permission_outcome(&params, &["reject_once", "reject_always"])
                    }
                    PermissionDecision::Allow => {
                        tracing::info!("request {id}: +{:.2} s allowed a {kind} call", at());
                        if let Some(command) = acp::permission_command(&params) {
                            tracing::debug!("request {id}: allowed {command:?}");
                        }
                        if let Some(tc) = params.get("toolCall") {
                            stage_widget(tc, &mut ui_calls, &mut pending_widgets, &mut widget_seq);
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
        _ if !answer.is_empty() || !widgets.is_empty() => Outcome::Answer {
            text: answer,
            denied,
            suppressed: false,
            widgets,
            conversation_id: None,
            profile: None,
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
                reply_to: None,
                distill: false,
                schedule_id: None,
                schedule_has_script: false,
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
            widgets,
            conversation_id,
            profile,
            ..
        } => Outcome::Answer {
            text,
            denied,
            suppressed,
            widgets,
            conversation_id,
            profile,
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

        // Parallel batch of 3 argument-less `tool_call` stubs followed by 3
        // distinct `session/request_permission` payloads must NOT falsely
        // abort on stub #3 and must upgrade each call by its own toolCallId.
        let mut batch = ToolTracker::default();
        for id in ["p1", "p2", "p3"] {
            assert!(batch.record_update(
                &json!({"toolCallId": id, "name": "run_command", "kind": "execute"})
            ));
            assert_eq!(batch.repeat_streak(), 0);
        }
        assert_eq!(batch.count(), 3);
        for (id, cmd) in [("p1", "git status"), ("p2", "git diff"), ("p3", "git log")] {
            assert!(!batch.record_permission(Some(&json!({
                "toolCallId": id,
                "name": "run_command",
                "kind": "execute",
                "rawInput": {"CommandLine": cmd}
            }))));
            assert_eq!(batch.repeat_streak(), 1);
        }

        // Permissionless `read_file` stubs upgraded via `tool_call_update` also
        // track repeats accurately without tripping on distinct parallel reads.
        let mut reads = ToolTracker::default();
        for (id, path) in [("r1", "/a"), ("r2", "/a"), ("r3", "/a")] {
            reads.record_update(&json!({"toolCallId": id, "name": "read_file", "kind": "read"}));
            reads.record_followup_update(&json!({
                "toolCallId": id,
                "name": "read_file",
                "kind": "read",
                "rawInput": {"Path": path},
                "status": "completed"
            }));
        }
        assert_eq!(reads.repeat_streak(), 3);
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

    #[test]
    fn tool_tracker_exempts_internal_mcp_schema_reads_from_budget() {
        let agent_dir = PathBuf::from("/tmp/hc-state/agent");
        let mut tracker = ToolTracker::new(Some(agent_dir));

        // 1. Direct MCP schema read_file under <agent_dir>/.gemini/antigravity-cli/mcp/
        tracker.record_update(&json!({
            "toolCallId": "s1",
            "name": "read_file",
            "kind": "read",
            "rawInput": {"Path": "/tmp/hc-state/agent/.gemini/antigravity-cli/mcp/slack/conversations_history.json"}
        }));
        assert_eq!(tracker.count(), 1);
        assert_eq!(tracker.budget_count(), 0);

        // 2. Argument-less read_file stub upgraded via tool_call_update to an MCP schema path
        tracker.record_update(&json!({"toolCallId": "s2", "name": "read_file", "kind": "read"}));
        assert_eq!(tracker.budget_count(), 0);
        tracker.record_followup_update(&json!({
            "toolCallId": "s2",
            "name": "read_file",
            "kind": "read",
            "rawInput": {"Path": "/tmp/hc-state/agent/mcp/google-workspace/list_events.json"},
            "status": "completed"
        }));
        assert_eq!(tracker.budget_count(), 0);

        // 3. Argument-less read_file stub upgraded to a regular file DOES count!
        tracker.record_update(&json!({"toolCallId": "r1", "name": "read_file", "kind": "read"}));
        tracker.record_followup_update(&json!({
            "toolCallId": "r1",
            "name": "read_file",
            "kind": "read",
            "rawInput": {"Path": "/etc/hosts"},
            "status": "completed"
        }));
        assert_eq!(tracker.budget_count(), 1);

        // 4. Workspace `mcp/` files and `..` traversal are NEVER exempted.
        tracker.record_update(&json!({
            "toolCallId": "w1",
            "name": "read_file",
            "kind": "read",
            "rawInput": {"Path": "/tmp/hc-state/agent/workspace/mcp/fake.json"}
        }));
        tracker.record_update(&json!({
            "toolCallId": "w2",
            "name": "read_file",
            "kind": "read",
            "rawInput": {"Path": "/tmp/hc-state/agent/mcp/../../etc/passwd"}
        }));
        assert_eq!(tracker.budget_count(), 3);

        // 5. Internal `mcp-ui` schema reads are also exempted from budget.
        tracker.record_update(&json!({
            "toolCallId": "ui_s1",
            "name": "read_file",
            "kind": "read",
            "rawInput": {"Path": "/tmp/hc-state/agent/.gemini/antigravity-cli/mcp/mcp-ui/show_widget.json"}
        }));
        assert_eq!(tracker.budget_count(), 3);
    }

    struct MockSpeechPresenter {
        enabled: std::sync::atomic::AtomicBool,
        speaking: std::sync::atomic::AtomicBool,
        active_id: std::sync::atomic::AtomicU64,
        stops: Mutex<Vec<Option<u64>>>,
        events: Mutex<Vec<Presentation>>,
    }

    impl MockSpeechPresenter {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                enabled: std::sync::atomic::AtomicBool::new(false),
                speaking: std::sync::atomic::AtomicBool::new(false),
                active_id: std::sync::atomic::AtomicU64::new(0),
                stops: Mutex::new(Vec::new()),
                events: Mutex::new(Vec::new()),
            })
        }
    }

    impl Presenter for MockSpeechPresenter {
        fn present(&self, item: Presentation) {
            if let Presentation::SpeechStart { id } = &item
                && self.enabled.load(std::sync::atomic::Ordering::SeqCst)
            {
                self.speaking
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                self.active_id
                    .store(*id, std::sync::atomic::Ordering::SeqCst);
            }
            self.events.lock().unwrap().push(item);
        }

        fn stop_speech(&self, id: Option<u64>) -> Option<u64> {
            self.stops.lock().unwrap().push(id);
            let was_speaking = self
                .speaking
                .swap(false, std::sync::atomic::Ordering::SeqCst);
            if was_speaking {
                let cur = self.active_id.load(std::sync::atomic::Ordering::SeqCst);
                if id.is_none_or(|only| only == cur) {
                    return Some(cur);
                }
            }
            None
        }

        fn set_speak_enabled(&self, enabled: bool) {
            self.enabled
                .store(enabled, std::sync::atomic::Ordering::SeqCst);
        }

        fn speak_status(&self) -> (bool, bool) {
            (
                self.enabled.load(std::sync::atomic::Ordering::SeqCst),
                self.speaking.load(std::sync::atomic::Ordering::SeqCst),
            )
        }
    }

    #[tokio::test]
    async fn speak_control_persists_state_barges_in_and_cancels_post_turn_tts() {
        let dir = std::env::temp_dir().join(format!("hc-cond-speak-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut cfg = Config {
            state_dir: dir.clone(),
            ..Config::default()
        };
        cfg.agent.warm_start = false;
        cfg.present.speak.enabled = true;

        let mock_pres = MockSpeechPresenter::new();
        let presenter: Arc<dyn Presenter> = mock_pres.clone();
        let (handle, task) =
            spawn_with_store(Arc::new(cfg), presenter, Arc::new(SystemContext), None);

        // Initialized from config (no state file yet).
        assert_eq!(handle.control_speak(SpeakAction::Status), (true, false));
        assert!(handle.status().speak);

        // Toggle off -> persists `off` and stops speech.
        assert_eq!(handle.control_speak(SpeakAction::Toggle), (false, false));
        assert!(!store::load_speak_state(&dir, true));
        assert!(!handle.status().speak);

        // Turn back on -> simulate active post-turn TTS playback for turn #7 -> cancel stops it!
        assert_eq!(handle.control_speak(SpeakAction::On), (true, false));
        assert!(store::load_speak_state(&dir, false));
        mock_pres
            .speaking
            .store(true, std::sync::atomic::Ordering::SeqCst);
        mock_pres
            .active_id
            .store(7, std::sync::atomic::Ordering::SeqCst);
        assert!(handle.status().speaking);
        assert_eq!(handle.cancel(None, FailReason::Cancelled), Some(7));
        assert!(!handle.status().speaking);

        handle.shutdown();
        let _ = task.await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn reply_to_validates_target_and_records_accurate_turn_origins() {
        let dir = std::env::temp_dir().join(format!("hc-cond-origin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut cfg = Config {
            state_dir: dir.clone(),
            ..Config::default()
        };
        cfg.agent.warm_start = false;
        cfg.profiles.insert(
            "deep".to_string(),
            crate::config::Profile {
                thought_level: Some("high".to_string()),
                ..crate::config::Profile::default()
            },
        );
        let (store, last_id) = Store::open(&dir, &cfg.history).unwrap();
        store.create_turn(NewTurn {
            id: 1,
            lane: Lane::Interactive,
            status: TurnStatus::Queued,
            profile: "deep".to_string(),
            created_at_ms: 1000,
            request: "root question".to_string(),
            attachments: vec![],
            origin: store::TurnOrigin::Typed,
            schedule_id: None,
        });
        store.mark_running(
            1,
            Lane::Interactive,
            "s1".to_string(),
            Some(1),
            "deep".to_string(),
            1000,
        );
        store.finish_completed(1, "root answer".to_string(), vec![], 1100);
        store.flush().await;

        let presenter: Arc<dyn Presenter> = crate::present::Recorder::new();
        let (handle, task) = spawn_with_store(
            Arc::new(cfg),
            presenter,
            Arc::new(SystemContext),
            Some((store.clone(), last_id.max(1))),
        );

        // Missing reply_to target is rejected immediately.
        let err = handle
            .ask(
                AskParams {
                    text: "followup".to_string(),
                    new: false,
                    profile: None,
                    background: false,
                    suppress_token: None,
                    attachments: vec![],
                    source: InputSource::Typed,
                    reply_to: Some(999),
                    distill: false,
                    schedule_id: None,
                    schedule_has_script: false,
                },
                Sink::Present,
            )
            .unwrap_err();
        assert!(err.contains("999"), "{err}");

        // Valid reply_to inherits `deep` profile; scheduled & distill asks record exact TurnOrigin.
        let r_id = handle
            .ask(
                AskParams {
                    text: "followup".to_string(),
                    new: false,
                    profile: None,
                    background: true,
                    suppress_token: None,
                    attachments: vec![],
                    source: InputSource::Typed,
                    reply_to: Some(1),
                    distill: false,
                    schedule_id: None,
                    schedule_has_script: false,
                },
                Sink::Present,
            )
            .unwrap();
        let d_id = handle
            .ask(
                AskParams {
                    text: "distill review".to_string(),
                    new: true,
                    profile: None,
                    background: true,
                    suppress_token: None,
                    attachments: vec![],
                    source: InputSource::Cli,
                    reply_to: None,
                    distill: true,
                    schedule_id: None,
                    schedule_has_script: false,
                },
                Sink::Present,
            )
            .unwrap();
        let s_id = handle
            .ask(
                AskParams {
                    text: "collector summary".to_string(),
                    new: true,
                    profile: None,
                    background: true,
                    suppress_token: None,
                    attachments: vec![],
                    source: InputSource::Cli,
                    reply_to: None,
                    distill: false,
                    schedule_id: Some(42),
                    schedule_has_script: true,
                },
                Sink::Present,
            )
            .unwrap();
        store.flush().await;

        let r_turn = store::get_turn(&dir, r_id).unwrap().unwrap();
        assert_eq!(r_turn.profile, "deep");
        assert_eq!(r_turn.origin, store::TurnOrigin::Typed);

        let d_turn = store::get_turn(&dir, d_id).unwrap().unwrap();
        assert_eq!(d_turn.origin, store::TurnOrigin::Distill);

        let s_turn = store::get_turn(&dir, s_id).unwrap().unwrap();
        assert_eq!(s_turn.origin, store::TurnOrigin::ScheduleScript);
        assert_eq!(s_turn.schedule_id, Some(42));

        handle.shutdown();
        let _ = task.await;
        store.shutdown().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
