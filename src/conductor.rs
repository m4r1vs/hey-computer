//! The conductor: one task that owns the agent and runs one request at a time.
//!
//! Everything that can interrupt a request (a newer request, `cancel`, a
//! client that hangs up, the agent dying, the agent going quiet) meets in one
//! `select!` here, so there is a single place that decides what happens to the
//! request and to the agent.
//!
//! Requests reach the conductor through a [`Handle`]. The handle numbers them
//! and cancels the one before as it queues the next, so the conductor only
//! ever starts the newest; it answers older ones as superseded without running
//! them.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::acp::{self, CallError, Connection, Incoming};
use crate::config::{Config, MAX_RESTART_DELAY_SECS, secs};
use crate::context::{self, ContextProvider, SystemContext};
use crate::environment::{self, Paths};
use crate::policy::Policy;
use crate::present::{Presentation, Presenter};
use crate::protocol::{AgentState, FailReason, Reply, Status};
use crate::store::{self, LastAnswer};

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

/// What became of a request.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Answer { text: String, denied: Vec<String> },
    Failed { reason: FailReason, message: String },
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
            Self::Answer { text, denied } => Reply::Answer { id, text, denied },
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
    pub sink: Sink,
    pub token: Arc<CancelToken>,
}

enum Message {
    Ask(Ask),
    Shutdown,
}

/// What the socket handlers can see and do without waiting for the conductor.
struct Shared {
    next_id: u64,
    /// The newest request, which is the only one that may still run.
    latest: Option<(u64, Arc<CancelToken>)>,
    agent: AgentState,
    running: Option<(u64, Instant)>,
    follow_up_until: Option<Instant>,
    handled: u64,
}

#[derive(Clone)]
pub struct Handle {
    shared: Arc<Mutex<Shared>>,
    messages: mpsc::UnboundedSender<Message>,
    /// Set at shutdown; interrupts whatever the conductor is waiting on.
    stop: Arc<CancelToken>,
}

impl Handle {
    /// Queue a request, superseding the one before. Returns its number.
    pub fn ask(&self, text: String, new: bool, sink: Sink) -> u64 {
        let token = Arc::new(CancelToken::default());
        let id = {
            let mut shared = self
                .shared
                .lock()
                .expect("the shared state is never poisoned");
            shared.next_id += 1;
            let id = shared.next_id;
            if let Some((_, previous)) = shared.latest.replace((id, Arc::clone(&token))) {
                previous.cancel(FailReason::Superseded);
            }
            id
        };
        let ask = Ask {
            id,
            text,
            new,
            sink,
            token,
        };
        if let Err(mpsc::error::SendError(Message::Ask(ask))) =
            self.messages.send(Message::Ask(ask))
        {
            deliver(
                None,
                ask,
                Outcome::failed(FailReason::AgentDown, "the daemon is shutting down"),
            );
        }
        id
    }

    /// Cancel the newest request if it has not finished, or only request
    /// `only` if given. Returns the request that was cancelled.
    pub fn cancel(&self, only: Option<u64>, reason: FailReason) -> Option<u64> {
        let shared = self.shared.lock().ok()?;
        let (id, token) = shared.latest.as_ref()?;
        if only.is_some_and(|only| only != *id) {
            return None;
        }
        token.cancel(reason).then_some(*id)
    }

    pub fn status(&self) -> Status {
        let shared = self
            .shared
            .lock()
            .expect("the shared state is never poisoned");
        let now = Instant::now();
        let waiting = shared
            .latest
            .as_ref()
            .filter(|(_, token)| {
                token.reason().is_none() && !token.state.lock().map(|s| s.finished).unwrap_or(true)
            })
            .map(|(id, _)| *id);
        let (running, running_secs) = match shared.running {
            Some((id, since)) => (Some(id), Some(since.elapsed().as_secs_f64())),
            None => (waiting, None),
        };
        Status {
            agent: shared.agent,
            running,
            running_secs,
            follow_up_secs: shared
                .follow_up_until
                .filter(|until| *until > now)
                .map(|until| (until - now).as_secs_f64()),
            handled: shared.handled,
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
    spawn_inner(config, presenter, context, true)
}

fn spawn_inner(
    config: Arc<Config>,
    presenter: Arc<dyn Presenter>,
    context: Arc<dyn ContextProvider>,
    persist_last: bool,
) -> (Handle, JoinHandle<()>) {
    let shared = Arc::new(Mutex::new(Shared {
        next_id: 0,
        latest: None,
        agent: AgentState::Down,
        running: None,
        follow_up_until: None,
        handled: 0,
    }));
    let (messages, inbox) = mpsc::unbounded_channel();
    let stop = Arc::new(CancelToken::default());
    let conductor = Conductor {
        policy: Policy::new(&config.permissions.deny),
        config,
        presenter,
        context,
        persist_last,
        shared: Arc::clone(&shared),
        inbox,
        stop: Arc::clone(&stop),
        live: None,
        paths: None,
        conversation: None,
        failures: 0,
        restart_at: None,
        last_used: Instant::now(),
    };
    let task = tokio::spawn(conductor.run());
    (
        Handle {
            shared,
            messages,
            stop,
        },
        task,
    )
}

/// A running agent.
struct Live {
    conn: Connection,
    incoming: mpsc::UnboundedReceiver<Incoming>,
    started: Instant,
    /// A session made ahead of time: the first session/new of an agent
    /// process takes about a second, later ones about 20 ms.
    spare: Option<String>,
}

struct Conversation {
    session: String,
    /// When the last turn ended; follow-ups are measured from here.
    last: Instant,
    turns: u32,
}

struct Conductor {
    config: Arc<Config>,
    presenter: Arc<dyn Presenter>,
    context: Arc<dyn ContextProvider>,
    persist_last: bool,
    policy: Policy,
    shared: Arc<Mutex<Shared>>,
    inbox: mpsc::UnboundedReceiver<Message>,
    stop: Arc<CancelToken>,
    live: Option<Live>,
    paths: Option<Paths>,
    conversation: Option<Conversation>,
    /// Agent deaths in a row, for the restart back-off.
    failures: u32,
    restart_at: Option<Instant>,
    last_used: Instant,
}

/// How one turn ended.
struct TurnEnd {
    outcome: Outcome,
    /// False when the agent died or had to be killed.
    alive: bool,
    /// The session should not be continued (for example, the agent refused
    /// the prompt, timed out, or hit a per-turn budget).
    broken: bool,
    /// The agent rejected the turn for lack of a login and must be restarted
    /// on the next request to re-read the keyring.
    auth_required: bool,
}

enum Step {
    Responded(Result<Result<Value, CallError>, oneshot::error::RecvError>),
    Event(Option<Incoming>),
    Quiet,
    TurnTimeout,
    Cancelled(FailReason),
}

#[derive(Clone, Copy)]
struct Budgets {
    inactivity: Duration,
    max_turn: Option<Duration>,
    max_tool_calls: Option<usize>,
}

struct TurnContext<'a> {
    policy: &'a Policy,
    budgets: Budgets,
    progress: Option<&'a Arc<dyn Presenter>>,
    id: u64,
}

enum TurnAbort {
    Quiet,
    TurnTimeout(Duration),
    ToolLimit(usize),
    Cancelled(FailReason),
}

/// Counts distinct tool calls in a single turn without double-counting
/// repeated `tool_call` updates or the matching `session/request_permission`.
#[derive(Default)]
struct ToolTracker {
    seen: HashSet<String>,
    count: usize,
}

impl ToolTracker {
    fn record_update(&mut self, id: Option<&str>) -> bool {
        match id {
            Some(id) => {
                if self.seen.insert(id.to_string()) {
                    self.count += 1;
                    true
                } else {
                    false
                }
            }
            None => {
                self.count += 1;
                true
            }
        }
    }

    fn record_permission(&mut self, id: Option<&str>) -> bool {
        if let Some(id) = id
            && self.seen.insert(id.to_string())
        {
            self.count += 1;
            true
        } else {
            false
        }
    }

    fn count(&self) -> usize {
        self.count
    }
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
                // A queued request goes first; whatever the agent sent in the
                // meantime is read by `drain_stale` before the request runs.
                // In a fixed order, a late update of a cancelled turn is
                // handled the same way every time.
                biased;
                message = self.inbox.recv() => match message {
                    Some(Message::Ask(ask)) => self.take(ask).await,
                    Some(Message::Shutdown) | None => break,
                },
                event = next_event(&mut self.live) => self.between_turns(event).await,
                () = sleep_until(restart_at) => {
                    self.restart_at = None;
                    tokio::select! {
                        () = self.start_warm() => {}
                        _ = stop.cancelled() => {}
                    }
                }
                () = sleep_until(idle_at) => {
                    tracing::info!("stopping the idle agent");
                    self.stop_agent().await;
                }
            }
        }
        // Whatever is still queued gets an answer.
        while let Ok(message) = self.inbox.try_recv() {
            if let Message::Ask(ask) = message {
                self.finish(ask, cancelled(FailReason::AgentDown));
            }
        }
        self.stop_agent().await;
    }

    fn idle_deadline(&self) -> Option<Instant> {
        let idle = secs(self.config.agent.idle_stop_secs)?;
        self.live.as_ref()?;
        self.last_used.checked_add(idle)
    }

    fn set_agent(&self, state: AgentState) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.agent = state;
        }
    }

    /// Start the agent ahead of any request, with a session ready.
    async fn start_warm(&mut self) {
        if self.live.is_some() {
            return;
        }
        if let Err(e) = self.start_agent().await {
            tracing::warn!("cannot start the agent: {e:#}");
            self.schedule_restart();
            return;
        }
        match self.new_session().await {
            Ok(session) => {
                if let Some(live) = &mut self.live {
                    live.spare = Some(session);
                }
            }
            // An expired login shows up here; call_failed has stopped the
            // unauthenticated agent so the next request starts fresh.
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
        let argv = paths.agent_command(&self.config);
        let (conn, incoming) = Connection::spawn(&argv, &paths.workspace)?;
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
                // Nothing has been asked yet, so its stderr can only be about
                // starting up (a missing login, say) and may be logged.
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
        self.paths = Some(paths);
        self.live = Some(Live {
            conn,
            incoming,
            started: Instant::now(),
            spare: None,
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
        // Capped before it becomes a Duration, which panics on huge values.
        let delay = Duration::from_secs_f64((base * factor).min(MAX_RESTART_DELAY.as_secs_f64()));
        self.failures += 1;
        self.restart_at = Some(Instant::now() + delay);
        tracing::info!("restarting the agent in {:.1} s", delay.as_secs_f64());
    }

    /// The agent is gone: say why, forget its session, and restart it if it
    /// is meant to stay up.
    async fn agent_died(&mut self, live: Live) -> String {
        let lived = live.started.elapsed();
        let ended = live.conn.reap().await;
        // Its last words may quote a request, so they stay at debug.
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

    /// Read what the agent sent since the last turn ended: an update of a
    /// cancelled turn that came after its answer, a permission request that
    /// nothing waits for, or its exit. Without this, a follow-up in the same
    /// session would begin with the cancelled turn's last words.
    async fn drain_stale(&mut self) {
        loop {
            let event = match self.live.as_mut().map(|live| live.incoming.try_recv()) {
                Some(Ok(event)) => Some(event),
                Some(Err(mpsc::error::TryRecvError::Empty)) | None => return,
                Some(Err(mpsc::error::TryRecvError::Disconnected)) => None,
            };
            let ended = matches!(event, None | Some(Incoming::Exited));
            self.between_turns(event).await;
            if ended {
                return;
            }
        }
    }

    async fn between_turns(&mut self, event: Option<Incoming>) {
        match event {
            None | Some(Incoming::Exited) => {
                if let Some(live) = self.live.take() {
                    self.agent_died(live).await;
                }
            }
            Some(Incoming::Permission { id, .. }) => {
                // Nothing is running that could have asked.
                if let Some(live) = &self.live {
                    live.conn
                        .respond(id, json!({"outcome": {"outcome": "cancelled"}}));
                }
            }
            Some(Incoming::Update { .. }) => {}
        }
    }

    async fn take(&mut self, ask: Ask) {
        if let Some(reason) = ask.token.reason() {
            self.finish(ask, cancelled(reason));
            return;
        }
        if let Ok(mut shared) = self.shared.lock() {
            shared.running = Some((ask.id, Instant::now()));
        }
        if matches!(ask.sink, Sink::Present) {
            self.presenter.present(Presentation::Progress {
                id: ask.id,
                request: ask.text.clone(),
            });
        }
        let outcome = self.run_request(&ask).await;
        self.last_used = Instant::now();
        self.finish(ask, outcome);
    }

    fn finish(&mut self, ask: Ask, outcome: Outcome) {
        ask.token.finish();
        if let Ok(mut shared) = self.shared.lock() {
            shared.running = None;
            shared.handled += 1;
        }
        if self.persist_last
            && let Outcome::Answer { text, denied } = &outcome
        {
            let record = LastAnswer {
                version: store::SCHEMA_VERSION,
                id: ask.id,
                timestamp: store::unix_timestamp_secs(),
                request: ask.text.clone(),
                text: text.clone(),
                denied: denied.clone(),
            };
            if let Err(e) = self
                .config
                .state_dir()
                .and_then(|dir| store::save_last(&dir, &record))
            {
                tracing::warn!("cannot save the last answer: {e:#}");
            }
        }
        deliver(Some(&self.presenter), ask, outcome);
    }

    async fn run_request(&mut self, ask: &Ask) -> Outcome {
        let started = Instant::now();
        let token = Arc::clone(&ask.token);
        tracing::info!(
            "request {}: {} characters",
            ask.id,
            ask.text.chars().count()
        );
        tracing::debug!("request {}: {:?}", ask.id, ask.text);

        self.drain_stale().await;
        let stop = Arc::clone(&self.stop);
        let interrupts = Interrupts {
            request: &token,
            stop: &stop,
        };
        let mut retried_auth = false;
        let session = loop {
            let was_live = self.live.is_some();
            if !was_live {
                self.restart_at = None;
                // Only a shutdown interrupts the start. A newer request (a quick
                // "no, I meant…") needs the same agent, so killing this one half
                // started would only make it pay for a second start.
                let start = tokio::select! {
                    result = self.start_agent() => result,
                    reason = stop.cancelled() => {
                        // The half-started agent was dropped, which killed it.
                        self.set_agent(AgentState::Down);
                        return cancelled(reason);
                    }
                };
                if let Err(e) = start {
                    self.schedule_restart_if_warm();
                    return Outcome::failed(
                        FailReason::AgentDown,
                        format!("Cannot start the assistant: {e:#}"),
                    );
                }
                if let Some(reason) = token.reason() {
                    return cancelled(reason);
                }
            }

            let result = tokio::select! {
                result = self.session_for(ask.new) => result,
                reason = interrupts.wait() => return cancelled(reason),
            };
            match result {
                Ok(session) => break session,
                Err(outcome) => {
                    // If a pre-existing agent lost its login and was stopped
                    // for it, retry once on a fresh agent in case the user
                    // already signed in again.
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
                    return outcome;
                }
            }
        };

        let header = if self.config.context.is_enabled() {
            tokio::select! {
                header = self.context.gather(&self.config.context) => header,
                reason = interrupts.wait() => return cancelled(reason),
            }
        } else {
            None
        };
        if let Some(header) = &header {
            tracing::debug!("request {}: ambient context: {:?}", ask.id, header);
        }
        let prompt = context::prepend(header.as_deref(), &ask.text);

        let Some(mut live) = self.live.take() else {
            return Outcome::failed(FailReason::AgentDown, "the assistant exited");
        };
        let budgets = Budgets {
            inactivity: secs(self.config.agent.inactivity_timeout_secs)
                .unwrap_or(Duration::from_secs(45)),
            max_turn: secs(self.config.agent.max_turn_secs),
            max_tool_calls: (self.config.agent.max_tool_calls > 0)
                .then_some(self.config.agent.max_tool_calls),
        };
        let ctx = TurnContext {
            policy: &self.policy,
            budgets,
            progress: matches!(ask.sink, Sink::Present).then_some(&self.presenter),
            id: ask.id,
        };
        let end = turn(&mut live, &session, &prompt, interrupts, &ctx).await;

        if end.alive {
            self.live = Some(live);
            if end.auth_required {
                self.stop_agent().await;
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
            } else if let Some(conversation) = &mut self.conversation {
                conversation.last = Instant::now();
                conversation.turns += 1;
                let until = secs(self.config.conversation.follow_up_secs)
                    .and_then(|window| conversation.last.checked_add(window));
                self.set_follow_up(until);
            }
        } else {
            let detail = self.agent_died(live).await;
            if let Outcome::Failed {
                reason: FailReason::AgentDown,
                message,
            } = &end.outcome
            {
                return Outcome::failed(FailReason::AgentDown, format!("{message} ({detail})"));
            }
        }

        if matches!(end.outcome, Outcome::Answer { .. }) {
            self.failures = 0;
        }
        log_outcome(ask.id, &end.outcome, started);
        end.outcome
    }

    fn schedule_restart_if_warm(&mut self) {
        if self.config.agent.warm_start {
            self.schedule_restart();
        }
    }

    fn set_follow_up(&self, until: Option<Instant>) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.follow_up_until = until;
        }
    }

    /// The session this request continues, or a new one.
    async fn session_for(&mut self, new: bool) -> Result<String, Outcome> {
        let window = secs(self.config.conversation.follow_up_secs);
        if !new
            && let Some(conversation) = &self.conversation
            && window.is_some_and(|w| conversation.last.elapsed() < w)
        {
            tracing::info!(
                "continuing the conversation (turn {})",
                conversation.turns + 1
            );
            return Ok(conversation.session.clone());
        }

        if let Some(old) = self.conversation.take()
            && let Some(live) = &self.live
        {
            // Let the agent free it; nobody waits for the answer.
            drop(
                live.conn
                    .start_call("session/close", json!({"sessionId": old.session})),
            );
        }
        self.set_follow_up(None);

        let spare = self.live.as_mut().and_then(|live| live.spare.take());
        let session = match spare {
            Some(session) => session,
            None => self.new_session().await?,
        };
        tracing::info!("new conversation");
        self.conversation = Some(Conversation {
            session: session.clone(),
            last: Instant::now(),
            turns: 0,
        });
        Ok(session)
    }

    async fn new_session(&mut self) -> Result<String, Outcome> {
        let started = Instant::now();
        let (Some(live), Some(paths)) = (&self.live, &self.paths) else {
            return Err(Outcome::failed(
                FailReason::AgentDown,
                "the assistant is not running",
            ));
        };
        // The vault is deliberately not passed as `additionalDirectories`:
        // agy loads AGENTS.md, GEMINI.md and `.agents/` from every workspace
        // directory as trusted instructions, so one file written into the
        // vault (by a sync peer, a shared folder, or the agent itself after
        // reading a hostile page) would steer every later request. Outside
        // the workspace, the agent's reads and edits there arrive as
        // permission requests, which the daemon allows.
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

        let mut options = vec![];
        if !self.config.agent.model.is_empty() {
            options.push(("model", self.config.agent.model.clone()));
        }
        if !self.config.agent.thought_level.is_empty() {
            options.push(("thought_level", self.config.agent.thought_level.clone()));
        }
        // Default mode sends every tool call that needs approval to the
        // daemon; agy's "auto" would let its own classifier decide.
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

    /// Turn a failed call into an outcome, dealing with a dead agent.
    async fn call_failed(&mut self, method: &str, error: CallError) -> Outcome {
        match error {
            CallError::Rpc(e) => {
                if is_auth_required(&e) {
                    // agy latches a failed silent sign-in for the life of the
                    // process; stop it without scheduling a restart so the next
                    // request spawns a fresh agent that re-reads the keyring.
                    tracing::warn!(
                        "{method} failed: {e}; stopping the agent so the next request starts fresh"
                    );
                    self.stop_agent().await;
                } else {
                    tracing::warn!("{method} failed: {e}");
                }
                Outcome::failed(FailReason::AgentError, agent_error_message(&e))
            }
            CallError::Closed => {
                let detail = match self.live.take() {
                    Some(live) => self.agent_died(live).await,
                    None => String::new(),
                };
                Outcome::failed(
                    FailReason::AgentDown,
                    format!("The assistant exited ({detail})"),
                )
            }
            CallError::Timeout(after) => {
                tracing::warn!("{method} got no answer within {after:?}; restarting the agent");
                self.stop_agent().await;
                self.schedule_restart_if_warm();
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
fn deliver(presenter: Option<&Arc<dyn Presenter>>, ask: Ask, outcome: Outcome) {
    match ask.sink {
        Sink::Client(reply) => {
            // A client that left has already cancelled the request.
            let _ = reply.send(outcome.into_reply(ask.id));
        }
        Sink::Present => {
            let Some(presenter) = presenter else {
                return;
            };
            presenter.present(match outcome {
                Outcome::Answer { text, denied } => Presentation::Answer {
                    id: ask.id,
                    text,
                    denied,
                },
                Outcome::Failed { reason, message } => Presentation::Failure {
                    id: ask.id,
                    reason,
                    message,
                },
            });
        }
    }
}

fn is_auth_required(error: &acp::RpcError) -> bool {
    error.code == AUTH_REQUIRED || error.message.contains("Authentication required")
}

/// What to tell the user when the agent refused a call. agy 1.5 answers
/// `session/new` with `-32000 Authentication required` when it has no login
/// (seen with the keyring out of reach); that alone does not say what to do.
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

/// ACP's error code for a missing or expired login.
const AUTH_REQUIRED: i64 = -32000;

fn cancelled(reason: FailReason) -> Outcome {
    Outcome::failed(reason, cancel_message(reason))
}

fn cancel_message(reason: FailReason) -> &'static str {
    match reason {
        FailReason::Superseded => "A newer request replaced this one.",
        // Only the daemon's own shutdown cancels with this reason.
        FailReason::AgentDown => "The daemon is shutting down.",
        _ => "The request was cancelled.",
    }
}

/// What may end a request early: its own cancellation (a newer request,
/// `cancel`, a client that left) or the daemon's shutdown.
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
        Outcome::Answer { text, denied } => {
            tracing::info!(
                "request {id}: answered in {elapsed:.2} s, {} characters, {} refused",
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

/// The current value of one of the session's config options.
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

async fn next_event(live: &mut Option<Live>) -> Option<Incoming> {
    match live {
        Some(live) => live.incoming.recv().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

/// One prompt, from session/prompt to its stop reason.
async fn turn(
    live: &mut Live,
    session: &str,
    text: &str,
    interrupts: Interrupts<'_>,
    ctx: &TurnContext<'_>,
) -> TurnEnd {
    let policy = ctx.policy;
    let budgets = ctx.budgets;
    let id = ctx.id;
    let started = Instant::now();
    let mut response = live.conn.start_call(
        "session/prompt",
        json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
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
        // Events first: the reader queues every update before it hands over
        // the response that follows them, so the last chunks would otherwise
        // be lost to a response that is already waiting.
        let step = tokio::select! {
            biased;
            event = live.incoming.recv() => Step::Event(event),
            result = &mut response => Step::Responded(result),
            () = tokio::time::sleep_until(deadline.into()) => Step::Quiet,
            () = &mut turn_sleep => Step::TurnTimeout,
            reason = interrupts.wait() => Step::Cancelled(reason),
        };
        match step {
            Step::Responded(result) => break Ok(result),
            Step::Event(None | Some(Incoming::Exited)) => {
                return TurnEnd {
                    outcome: Outcome::failed(
                        FailReason::AgentDown,
                        "The assistant exited in the middle of the request",
                    ),
                    alive: false,
                    broken: true,
                    auth_required: false,
                };
            }
            Step::Event(Some(Incoming::Update {
                session: from,
                update,
            })) => {
                if from != session {
                    continue;
                }
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
                            if tracker.record_update(acp::tool_call_id(&update)) {
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
                            let will_deny = acp::tool_command(&update)
                                .as_deref()
                                .and_then(|c| policy.denied_word(c))
                                .is_some();
                            if !will_deny {
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
            Step::Event(Some(Incoming::Permission {
                id: request,
                params,
            })) => {
                if let Some(limit) = budgets.max_turn
                    && turn_deadline.is_some_and(|d| Instant::now() >= d)
                {
                    live.conn
                        .respond(request, json!({"outcome": {"outcome": "cancelled"}}));
                    break Err(TurnAbort::TurnTimeout(limit));
                }
                deadline = Instant::now() + budgets.inactivity;
                let _ =
                    tracker.record_permission(params.get("toolCall").and_then(acp::tool_call_id));
                if let Some(limit) = budgets.max_tool_calls
                    && tracker.count() > limit
                {
                    live.conn
                        .respond(request, json!({"outcome": {"outcome": "cancelled"}}));
                    break Err(TurnAbort::ToolLimit(limit));
                }
                let command = acp::permission_command(&params);
                let refused = command.as_deref().and_then(|c| policy.denied_word(c));
                let kind = params
                    .pointer("/toolCall/kind")
                    .and_then(Value::as_str)
                    .unwrap_or("other");
                let outcome = if let Some(word) = refused {
                    tracing::info!("request {id}: +{:.2} s refused a {word} command", at());
                    denied.push(command.unwrap_or_default());
                    acp::permission_outcome(&params, &["reject_once", "reject_always"])
                } else {
                    tracing::info!("request {id}: +{:.2} s allowed a {kind} call", at());
                    if let Some(command) = &command {
                        tracing::debug!("request {id}: allowed {command:?}");
                    }
                    emit_progress(
                        &mut last_progress,
                        params.get("toolCall").and_then(acp::tool_progress),
                    );
                    acp::permission_outcome(&params, &["allow_once", "allow_always"])
                };
                live.conn.respond(request, outcome);
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
            let alive = cancel_turn(live, session, &mut response).await;
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
                TurnAbort::Cancelled(reason) => (reason, cancel_message(reason).to_string(), false),
            };
            return TurnEnd {
                outcome: Outcome::failed(reason, message),
                alive,
                broken,
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
        },
        // A refused tool call ends agy's turn with no text at all, so the
        // daemon has to say what happened.
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
        broken: false,
        auth_required: false,
    }
}

/// Ask the agent to stop the turn and wait briefly for it to do so. Returns
/// false when the agent died or would not stop.
async fn cancel_turn(
    live: &mut Live,
    session: &str,
    response: &mut oneshot::Receiver<Result<Value, CallError>>,
) -> bool {
    live.conn
        .notify("session/cancel", json!({"sessionId": session}));
    let deadline = tokio::time::Instant::now() + CANCEL_GRACE;
    loop {
        // Events first, as in `turn`: the old turn's last chunks must be read
        // here, or a follow-up in the same session would start with them.
        tokio::select! {
            biased;
            event = live.incoming.recv() => match event {
                None | Some(Incoming::Exited) => return false,
                Some(Incoming::Permission { id, .. }) => {
                    live.conn.respond(id, json!({"outcome": {"outcome": "cancelled"}}));
                }
                Some(Incoming::Update { .. }) => {}
            },
            result = &mut *response => {
                return matches!(result, Ok(Ok(_)) | Ok(Err(CallError::Rpc(_))));
            }
            () = tokio::time::sleep_until(deadline) => {
                tracing::warn!("the agent did not stop the turn within {CANCEL_GRACE:?}");
                return false;
            }
        }
    }
}

/// Run one request without a daemon: start the agent, ask, stop it. For
/// `debug prompt`, which is how the agent's behaviour is measured.
pub async fn ask_once(config: Config, text: String) -> Result<Outcome> {
    let mut config = config;
    config.agent.warm_start = false;
    let presenter: Arc<dyn Presenter> = crate::present::Recorder::new();
    let (handle, task) = spawn_inner(Arc::new(config), presenter, Arc::new(SystemContext), false);
    let (tx, rx) = oneshot::channel();
    let id = handle.ask(text, true, Sink::Client(tx));
    let reply = rx.await.context("the conductor stopped")?;
    handle.shutdown();
    let _ = task.await;
    Ok(match reply {
        Reply::Answer { text, denied, .. } => Outcome::Answer { text, denied },
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
        assert!(tracker.record_update(Some("call_1")));
        assert!(!tracker.record_update(Some("call_1")));
        assert!(!tracker.record_permission(Some("call_1")));
        assert_eq!(tracker.count(), 1);

        assert!(tracker.record_permission(Some("call_2")));
        assert!(!tracker.record_update(Some("call_2")));
        assert_eq!(tracker.count(), 2);

        // Updates without an id count once; a matching id-less permission does not double-count.
        assert!(tracker.record_update(None));
        assert!(!tracker.record_permission(None));
        assert_eq!(tracker.count(), 3);
    }
}
