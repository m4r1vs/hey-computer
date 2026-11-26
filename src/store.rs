//! Persistent state stored under `$XDG_STATE_HOME/hey-computer/`.
//!
//! Uses a single private SQLite database (`history.db`, WAL mode, mode `0600`)
//! with an external-content FTS5 index over completed non-empty turns. Shared
//! by `hey-computer last`, `hey-computer tasks`, `hey-computer recall`, and
//! future viewer history. Legacy `last.json` is imported once on daemon start
//! and still accepted as a read-only fallback when `history.db` does not exist.

use std::fmt;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::attachments::AttachmentMeta;
use crate::config::History as HistoryConfig;
pub use crate::protocol::{DraftAction, WidgetSpec};
use crate::protocol::{FailReason, InputSource};

pub const DB_FILE: &str = "history.db";
pub const LAST_FILE: &str = "last.json";
pub const SPEAK_STATE_FILE: &str = "speak_enabled";
pub const SCHEMA_VERSION: u32 = 1;
pub const DB_SCHEMA_VERSION: i32 = 5;

const PRUNE_EVERY_TURNS: u64 = 100;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn default_version() -> u32 {
    SCHEMA_VERSION
}

/// Execution lane recorded for each turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    Interactive,
    Background,
    Restricted,
}

impl Lane {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Background => "background",
            Self::Restricted => "restricted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "interactive" => Some(Self::Interactive),
            "background" => Some(Self::Background),
            "restricted" => Some(Self::Restricted),
            _ => None,
        }
    }
}

impl fmt::Display for Lane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lifecycle state of a persisted turn or background task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Queued,
    Running,
    Completed,
    Suppressed,
    Failed,
    Cancelled,
}

impl TurnStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Suppressed => "suppressed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "suppressed" => Some(Self::Suppressed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

impl fmt::Display for TurnStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Origin recorded on each turn so `hey-computer distill` skips automated
/// scheduled, pre-collector, and self-distillation runs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOrigin {
    #[default]
    Cli,
    Voice,
    Typed,
    Schedule,
    ScheduleScript,
    Distill,
}

impl TurnOrigin {
    pub fn from_input_source(source: InputSource) -> Self {
        match source {
            InputSource::Cli => Self::Cli,
            InputSource::Voice => Self::Voice,
            InputSource::Typed => Self::Typed,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Voice => "voice",
            Self::Typed => "typed",
            Self::Schedule => "schedule",
            Self::ScheduleScript => "schedule_script",
            Self::Distill => "distill",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "voice" => Self::Voice,
            "typed" => Self::Typed,
            "schedule" => Self::Schedule,
            "schedule_script" => Self::ScheduleScript,
            "distill" => Self::Distill,
            _ => Self::Cli,
        }
    }

    pub fn is_default(&self) -> bool {
        *self == Self::Cli
    }

    pub fn is_distillable(self) -> bool {
        matches!(self, Self::Cli | Self::Voice | Self::Typed)
    }
}

/// One persisted turn or background task row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRecord {
    pub id: u64,
    pub lane: Lane,
    pub status: TurnStatus,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<u64>,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<i64>,
    pub request: String,
    pub answer: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppressed_token: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub widgets: Vec<WidgetSpec>,
    #[serde(default, skip_serializing_if = "TurnOrigin::is_default")]
    pub origin: TurnOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_id: Option<u64>,
}

/// The most recent successful answer, returned by `load_last` and used for
/// backwards-compatible migration of legacy `<state_dir>/last.json`.
///
/// Intentionally omits `deny_unknown_fields` so future fields remain
/// backwards-compatible when read by older binaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastAnswer {
    #[serde(default = "default_version")]
    pub version: u32,
    pub id: u64,
    pub timestamp: u64,
    /// The raw request text that produced this answer.
    pub request: String,
    /// The raw, trimmed answer text (not HTML-disarmed).
    pub text: String,
    /// Commands refused by the deny list during this turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied: Vec<String>,
    /// Metadata for files or images attached to the request (never raw bytes).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentMeta>,
    /// Structured `mcp-ui` widgets captured during this turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub widgets: Vec<WidgetSpec>,
    /// Conversation thread root ID when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<u64>,
    /// Profile name used for this turn.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile: String,
}

impl From<TurnRecord> for LastAnswer {
    fn from(turn: TurnRecord) -> Self {
        let ts_ms = turn.finished_at_ms.unwrap_or(turn.created_at_ms).max(0) as u64;
        Self {
            version: SCHEMA_VERSION,
            id: turn.id,
            timestamp: ts_ms / 1000,
            request: turn.request,
            text: turn.answer,
            denied: turn.denied,
            attachments: turn.attachments,
            widgets: turn.widgets,
            conversation_id: turn.conversation_id,
            profile: turn.profile,
        }
    }
}

/// Current Unix epoch time in seconds.
pub fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Current Unix epoch time in milliseconds.
pub fn unix_timestamp_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Fields needed to create a new persisted turn row when accepted by the daemon.
#[derive(Debug, Clone)]
pub struct NewTurn {
    pub id: u64,
    pub lane: Lane,
    pub status: TurnStatus,
    pub profile: String,
    pub created_at_ms: i64,
    pub request: String,
    pub attachments: Vec<AttachmentMeta>,
    pub origin: TurnOrigin,
    pub schedule_id: Option<u64>,
}

enum StoreCmd {
    CreateTurn(NewTurn),
    PromoteBackground {
        id: u64,
        profile: String,
    },
    MarkRunning {
        id: u64,
        lane: Lane,
        session_id: String,
        conversation_id: Option<u64>,
        profile: String,
        started_at_ms: i64,
    },
    FinishCompleted {
        id: u64,
        answer: String,
        denied: Vec<String>,
        widgets: Vec<WidgetSpec>,
        finished_at_ms: i64,
    },
    FinishSuppressed {
        id: u64,
        token: String,
        finished_at_ms: i64,
    },
    FinishFailed {
        id: u64,
        reason: String,
        message: String,
        finished_at_ms: i64,
    },
    FinishCancelled {
        id: u64,
        reason: String,
        message: String,
        finished_at_ms: i64,
    },
    CreateDraftCheckpoint {
        spec: NewDraftCheckpoint,
        reply: oneshot::Sender<Result<DraftCheckpoint>>,
    },
    ClaimDraftCheckpoint {
        token_or_id: String,
        action: DraftAction,
        now_ms: i64,
        reply: oneshot::Sender<Result<CheckpointClaim>>,
    },
    DismissDraftsForChat {
        channel: String,
        chat_target: String,
        now_ms: i64,
        reply: oneshot::Sender<Result<usize>>,
    },
    ListDraftCheckpoints {
        include_all: bool,
        limit: usize,
        reply: oneshot::Sender<Result<Vec<DraftCheckpoint>>>,
    },
    GetDraftCheckpoint {
        token_or_id: String,
        reply: oneshot::Sender<Result<Option<DraftCheckpoint>>>,
    },
    Flush(oneshot::Sender<()>),
    Shutdown,
}

/// Cloneable handle to the daemon's single-writer `hc-store` actor thread.
///
/// Using one dedicated writer thread guarantees strict lifecycle write order
/// (`create_turn` -> `mark_running` -> `finish_*`) and keeps Tokio worker
/// threads free of SQLite I/O and WAL checkpoints.
#[derive(Clone)]
pub struct Store {
    tx: mpsc::UnboundedSender<StoreCmd>,
    thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
}

impl Store {
    /// Open or create `<state_dir>/history.db`, apply migrations, import any
    /// legacy `last.json`, recover interrupted turns, prune old history, and
    /// start the writer actor. Returns `(Store, initial_last_id)`.
    pub fn open(state_dir: &Path, history: &HistoryConfig) -> Result<(Self, u64)> {
        let mut conn = open_writer_connection(state_dir)?;
        migrate(&mut conn)?;
        let recovered = recover_interrupted(&conn, unix_timestamp_ms())?;
        if recovered > 0 {
            tracing::info!("recovered {recovered} interrupted turn(s) from previous daemon run");
        }
        let pruned = prune_connection(&conn, history, unix_timestamp_ms())?;
        if pruned > 0 {
            tracing::info!("pruned {pruned} old history row(s)");
        }
        import_legacy_last_json(&mut conn, state_dir)?;
        if let Err(e) = maintain_connection(&conn) {
            tracing::warn!("history database maintenance on startup failed: {e:#}");
        }
        tighten_db_permissions(state_dir);
        let initial_last_id = read_last_id(&conn)?;

        let (tx, mut rx) = mpsc::unbounded_channel::<StoreCmd>();
        let history = history.clone();
        let state_dir_owned = state_dir.to_path_buf();
        let handle = std::thread::Builder::new()
            .name("hc-store".to_string())
            .spawn(move || {
                let mut turns_since_prune: u64 = 0;
                let maybe_prune = |conn: &Connection, count: &mut u64| {
                    *count += 1;
                    if *count >= PRUNE_EVERY_TURNS {
                        *count = 0;
                        match prune_connection(conn, &history, unix_timestamp_ms()) {
                            Ok(n) if n > 0 => {
                                tracing::info!("pruned {n} old history row(s)");
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!("history prune failed: {e:#}"),
                        }
                        if let Err(e) = maintain_connection(conn) {
                            tracing::warn!("history maintenance failed: {e:#}");
                        }
                    }
                };
                while let Some(cmd) = rx.blocking_recv() {
                    match cmd {
                        StoreCmd::CreateTurn(turn) => {
                            if let Err(e) = insert_turn(&conn, &turn) {
                                tracing::warn!("cannot record turn {}: {e:#}", turn.id);
                            }
                        }
                        StoreCmd::PromoteBackground { id, profile } => {
                            if let Err(e) = conn.execute(
                                "UPDATE turns SET lane = 'background', profile = ?2 WHERE id = ?1",
                                params![id as i64, profile],
                            ) {
                                tracing::warn!("cannot promote turn {id} to background: {e}");
                            }
                        }
                        StoreCmd::MarkRunning {
                            id,
                            lane,
                            session_id,
                            conversation_id,
                            profile,
                            started_at_ms,
                        } => {
                            if let Err(e) = conn.execute(
                                "UPDATE turns
                                 SET lane = ?2,
                                     status = 'running',
                                     session_id = ?3,
                                     conversation_id = ?4,
                                     profile = ?5,
                                     started_at = COALESCE(started_at, ?6)
                                 WHERE id = ?1",
                                params![
                                    id as i64,
                                    lane.as_str(),
                                    session_id,
                                    conversation_id.map(|c| c as i64),
                                    profile,
                                    started_at_ms
                                ],
                            ) {
                                tracing::warn!("cannot mark turn {id} running: {e}");
                            }
                        }
                        StoreCmd::FinishCompleted {
                            id,
                            answer,
                            denied,
                            widgets,
                            finished_at_ms,
                        } => {
                            let denied_json =
                                serde_json::to_string(&denied).unwrap_or_else(|_| "[]".to_string());
                            let widgets_json = serde_json::to_string(&widgets)
                                .unwrap_or_else(|_| "[]".to_string());
                            if let Err(e) = conn.execute(
                                "UPDATE turns
                                 SET status = 'completed',
                                     answer = ?2,
                                     denied_json = ?3,
                                     widgets_json = ?4,
                                     finished_at = ?5
                                 WHERE id = ?1",
                                params![
                                    id as i64,
                                    answer,
                                    denied_json,
                                    widgets_json,
                                    finished_at_ms
                                ],
                            ) {
                                tracing::warn!("cannot finish turn {id}: {e}");
                            } else {
                                maybe_prune(&conn, &mut turns_since_prune);
                            }
                        }
                        StoreCmd::FinishSuppressed {
                            id,
                            token,
                            finished_at_ms,
                        } => {
                            if let Err(e) = conn.execute(
                                "UPDATE turns
                                 SET status = 'suppressed',
                                     suppressed_token = ?2,
                                     finished_at = ?3
                                 WHERE id = ?1",
                                params![id as i64, token, finished_at_ms],
                            ) {
                                tracing::warn!("cannot mark turn {id} suppressed: {e}");
                            } else {
                                maybe_prune(&conn, &mut turns_since_prune);
                            }
                        }
                        StoreCmd::FinishFailed {
                            id,
                            reason,
                            message,
                            finished_at_ms,
                        } => {
                            if let Err(e) = conn.execute(
                                "UPDATE turns
                                 SET status = 'failed',
                                     fail_reason = ?2,
                                     error_message = ?3,
                                     finished_at = ?4
                                 WHERE id = ?1",
                                params![id as i64, reason, message, finished_at_ms],
                            ) {
                                tracing::warn!("cannot mark turn {id} failed: {e}");
                            } else {
                                maybe_prune(&conn, &mut turns_since_prune);
                            }
                        }
                        StoreCmd::FinishCancelled {
                            id,
                            reason,
                            message,
                            finished_at_ms,
                        } => {
                            if let Err(e) = conn.execute(
                                "UPDATE turns
                                 SET status = 'cancelled',
                                     fail_reason = ?2,
                                     error_message = ?3,
                                     finished_at = ?4
                                 WHERE id = ?1",
                                params![id as i64, reason, message, finished_at_ms],
                            ) {
                                tracing::warn!("cannot mark turn {id} cancelled: {e}");
                            } else {
                                maybe_prune(&conn, &mut turns_since_prune);
                            }
                        }
                        StoreCmd::CreateDraftCheckpoint { spec, reply } => {
                            let _ = reply.send(insert_draft_checkpoint(&conn, &spec));
                        }
                        StoreCmd::ClaimDraftCheckpoint {
                            token_or_id,
                            action,
                            now_ms,
                            reply,
                        } => {
                            let _ = reply.send(claim_draft_checkpoint_conn(
                                &mut conn,
                                &token_or_id,
                                action,
                                now_ms,
                            ));
                        }
                        StoreCmd::DismissDraftsForChat {
                            channel,
                            chat_target,
                            now_ms,
                            reply,
                        } => {
                            let _ = reply.send(dismiss_drafts_for_chat_conn(
                                &conn,
                                &channel,
                                &chat_target,
                                now_ms,
                            ));
                        }
                        StoreCmd::ListDraftCheckpoints {
                            include_all,
                            limit,
                            reply,
                        } => {
                            let _ =
                                reply.send(list_draft_checkpoints_conn(&conn, include_all, limit));
                        }
                        StoreCmd::GetDraftCheckpoint { token_or_id, reply } => {
                            let _ = reply.send(lookup_draft_checkpoint(&conn, &token_or_id));
                        }
                        StoreCmd::Flush(ack) => {
                            let _ = ack.send(());
                        }
                        StoreCmd::Shutdown => break,
                    }
                }
                if let Err(e) = maintain_connection(&conn) {
                    tracing::warn!("history maintenance on shutdown failed: {e:#}");
                }
                let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
                tighten_db_permissions(&state_dir_owned);
            })
            .context("spawning hc-store thread")?;

        Ok((
            Self {
                tx,
                thread: Arc::new(Mutex::new(Some(handle))),
            },
            initial_last_id,
        ))
    }

    pub fn create_turn(&self, turn: NewTurn) {
        let _ = self.tx.send(StoreCmd::CreateTurn(turn));
    }

    pub fn promote_to_background(&self, id: u64, profile: String) {
        let _ = self.tx.send(StoreCmd::PromoteBackground { id, profile });
    }

    pub fn mark_running(
        &self,
        id: u64,
        lane: Lane,
        session_id: String,
        conversation_id: Option<u64>,
        profile: String,
        started_at_ms: i64,
    ) {
        let _ = self.tx.send(StoreCmd::MarkRunning {
            id,
            lane,
            session_id,
            conversation_id,
            profile,
            started_at_ms,
        });
    }

    pub fn finish_completed(
        &self,
        id: u64,
        answer: String,
        denied: Vec<String>,
        finished_at_ms: i64,
    ) {
        self.finish_completed_with_widgets(id, answer, denied, Vec::new(), finished_at_ms);
    }

    pub fn finish_completed_with_widgets(
        &self,
        id: u64,
        answer: String,
        denied: Vec<String>,
        widgets: Vec<WidgetSpec>,
        finished_at_ms: i64,
    ) {
        let _ = self.tx.send(StoreCmd::FinishCompleted {
            id,
            answer,
            denied,
            widgets,
            finished_at_ms,
        });
    }

    pub fn finish_suppressed(&self, id: u64, token: String, finished_at_ms: i64) {
        let _ = self.tx.send(StoreCmd::FinishSuppressed {
            id,
            token,
            finished_at_ms,
        });
    }

    pub fn finish_failed(&self, id: u64, reason: FailReason, message: String, finished_at_ms: i64) {
        let _ = self.tx.send(StoreCmd::FinishFailed {
            id,
            reason: fail_reason_str(reason).to_string(),
            message,
            finished_at_ms,
        });
    }

    pub fn finish_cancelled(
        &self,
        id: u64,
        reason: FailReason,
        message: String,
        finished_at_ms: i64,
    ) {
        let _ = self.tx.send(StoreCmd::FinishCancelled {
            id,
            reason: fail_reason_str(reason).to_string(),
            message,
            finished_at_ms,
        });
    }

    /// Persist a new stateless notification draft checkpoint with a fresh
    /// 128-bit single-use token (`hc_d_<32hex>`).
    pub async fn create_draft_checkpoint(
        &self,
        spec: NewDraftCheckpoint,
    ) -> Result<DraftCheckpoint> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StoreCmd::CreateDraftCheckpoint { spec, reply })
            .map_err(|_| anyhow::anyhow!("history store actor stopped"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("history store actor dropped reply"))?
    }

    /// Atomically claim a draft checkpoint by token or numeric ID (`#12` / `12`)
    /// inside an `IMMEDIATE` transaction.
    pub async fn claim_draft_checkpoint(
        &self,
        token_or_id: String,
        action: DraftAction,
        now_ms: i64,
    ) -> Result<CheckpointClaim> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StoreCmd::ClaimDraftCheckpoint {
                token_or_id,
                action,
                now_ms,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("history store actor stopped"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("history store actor dropped reply"))?
    }

    /// Dismiss all pending draft checkpoints for `(channel, chat_target)` (for
    /// example when the user answers directly on their phone).
    pub async fn dismiss_drafts_for_chat(
        &self,
        channel: String,
        chat_target: String,
        now_ms: i64,
    ) -> Result<usize> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StoreCmd::DismissDraftsForChat {
                channel,
                chat_target,
                now_ms,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("history store actor stopped"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("history store actor dropped reply"))?
    }

    /// List draft checkpoints (pending only unless `include_all` is true), newest first.
    pub async fn list_draft_checkpoints(
        &self,
        include_all: bool,
        limit: usize,
    ) -> Result<Vec<DraftCheckpoint>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StoreCmd::ListDraftCheckpoints {
                include_all,
                limit,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("history store actor stopped"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("history store actor dropped reply"))?
    }

    /// Look up one draft checkpoint by token or numeric ID (`#12` / `12`).
    pub async fn get_draft_checkpoint(
        &self,
        token_or_id: String,
    ) -> Result<Option<DraftCheckpoint>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StoreCmd::GetDraftCheckpoint { token_or_id, reply })
            .map_err(|_| anyhow::anyhow!("history store actor stopped"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("history store actor dropped reply"))?
    }

    /// Wait until all previously queued writes have committed.
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(StoreCmd::Flush(tx)).is_ok() {
            let _ = rx.await;
        }
    }

    /// Drain pending writes, checkpoint the WAL, and stop the actor thread.
    pub async fn shutdown(&self) {
        let _ = self.tx.send(StoreCmd::Shutdown);
        let handle = self.thread.lock().ok().and_then(|mut h| h.take());
        if let Some(handle) = handle {
            let _ = tokio::task::spawn_blocking(move || {
                let _ = handle.join();
            })
            .await;
        }
    }
}

fn fail_reason_str(reason: FailReason) -> &'static str {
    match reason {
        FailReason::Cancelled => "cancelled",
        FailReason::Superseded => "superseded",
        FailReason::Timeout => "timeout",
        FailReason::LimitReached => "limit_reached",
        FailReason::AgentDown => "agent_down",
        FailReason::KeyringLocked => "keyring_locked",
        FailReason::AgentError => "agent_error",
        FailReason::Denied => "denied",
        FailReason::Empty => "empty",
    }
}

fn ensure_private_dir(state_dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state_dir)
        .with_context(|| format!("creating {}", state_dir.display()))?;
    std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restricting {}", state_dir.display()))?;
    Ok(())
}

fn open_writer_connection(state_dir: &Path) -> Result<Connection> {
    ensure_private_dir(state_dir)?;
    let db_path = state_dir.join(DB_FILE);

    // Pre-create the DB file with mode 0600 so SQLite's Unix VFS copies 0600
    // onto the -wal and -shm sidecars with no umask race window.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&db_path)
    {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("creating {}", db_path.display())),
    }
    std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", db_path.display()))?;

    let conn = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", db_path.display()))?;

    conn.execute_batch(
        "PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;
         PRAGMA auto_vacuum = INCREMENTAL;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA secure_delete = ON;
         PRAGMA temp_store = MEMORY;",
    )
    .context("configuring history database PRAGMAs")?;

    Ok(conn)
}

fn tighten_db_permissions(state_dir: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let path = state_dir.join(format!("{DB_FILE}{suffix}"));
        match std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::debug!("cannot restrict {}: {e}", path.display()),
        }
    }
}

fn open_reader_connection(state_dir: &Path) -> Result<Option<Connection>> {
    let db_path = state_dir.join(DB_FILE);
    if !db_path.exists() {
        return Ok(None);
    }

    let try_ro = || -> rusqlite::Result<Connection> {
        let conn = Connection::open_with_flags(
            &db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.execute_batch("PRAGMA busy_timeout = 5000;")?;
        // Probe schema readability (triggers WAL-shm check if a crash left an unrecovered WAL).
        let _: i32 = conn.query_row("PRAGMA user_version;", [], |r| r.get(0))?;
        Ok(conn)
    };

    let conn = match try_ro() {
        Ok(conn) => conn,
        Err(_) => {
            // If a prior SIGKILL left an unrecovered -wal without -shm, SQLite
            // needs RW once to recover it; lock the handle to query_only after.
            let conn = Connection::open_with_flags(
                &db_path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .with_context(|| format!("opening {}", db_path.display()))?;
            conn.execute_batch("PRAGMA busy_timeout = 5000; PRAGMA query_only = ON;")
                .context("configuring read-only history database")?;
            conn
        }
    };

    let version: i32 = conn
        .query_row("PRAGMA user_version;", [], |r| r.get(0))
        .context("reading history database schema version")?;
    if version > DB_SCHEMA_VERSION {
        bail!(
            "history database {} was created by a newer version of hey-computer (schema v{version}, supported v{DB_SCHEMA_VERSION})",
            db_path.display()
        );
    }
    if version == 0 {
        return Ok(None);
    }
    Ok(Some(conn))
}

const SCHEMA_V1: &str = r#"
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE turns (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    lane             TEXT    NOT NULL CHECK (lane IN ('interactive', 'background', 'restricted')),
    status           TEXT    NOT NULL CHECK (status IN (
                         'queued', 'running', 'completed', 'suppressed', 'failed', 'cancelled'
                     )),
    profile          TEXT    NOT NULL DEFAULT '',
    session_id       TEXT,
    conversation_id  INTEGER,
    created_at       INTEGER NOT NULL,
    started_at       INTEGER,
    finished_at      INTEGER,
    request          TEXT    NOT NULL,
    answer           TEXT    NOT NULL DEFAULT '',
    denied_json      TEXT    NOT NULL DEFAULT '[]',
    attachments_json TEXT    NOT NULL DEFAULT '[]',
    fail_reason      TEXT,
    error_message    TEXT,
    suppressed_token TEXT
) STRICT;

CREATE INDEX idx_turns_status_id        ON turns(status, id DESC);
CREATE INDEX idx_turns_lane_id          ON turns(lane, id DESC);
CREATE INDEX idx_turns_created_at       ON turns(created_at DESC);
CREATE INDEX idx_turns_conversation_id  ON turns(conversation_id, id ASC) WHERE conversation_id IS NOT NULL;
CREATE INDEX idx_turns_active           ON turns(id) WHERE status IN ('queued', 'running');

CREATE VIEW recallable_turns AS
SELECT id, request, answer
FROM turns
WHERE status = 'completed' AND length(trim(answer)) > 0;

CREATE VIRTUAL TABLE turns_fts USING fts5(
    request,
    answer,
    content='recallable_turns',
    content_rowid='id',
    tokenize='unicode61 remove_diacritics 2'
);

CREATE TRIGGER turns_ai AFTER INSERT ON turns
WHEN NEW.status = 'completed' AND length(trim(NEW.answer)) > 0
BEGIN
    INSERT INTO turns_fts(rowid, request, answer)
    VALUES (NEW.id, NEW.request, NEW.answer);
END;

CREATE TRIGGER turns_ad AFTER DELETE ON turns
WHEN OLD.status = 'completed' AND length(trim(OLD.answer)) > 0
BEGIN
    INSERT INTO turns_fts(turns_fts, rowid, request, answer)
    VALUES ('delete', OLD.id, OLD.request, OLD.answer);
END;

CREATE TRIGGER turns_au AFTER UPDATE OF status, request, answer ON turns
BEGIN
    INSERT INTO turns_fts(turns_fts, rowid, request, answer)
        SELECT 'delete', OLD.id, OLD.request, OLD.answer
        WHERE OLD.status = 'completed' AND length(trim(OLD.answer)) > 0;
    INSERT INTO turns_fts(rowid, request, answer)
        SELECT NEW.id, NEW.request, NEW.answer
        WHERE NEW.status = 'completed' AND length(trim(NEW.answer)) > 0;
END;
"#;

const SCHEMA_V2: &str = r#"
CREATE TABLE schedules (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    name                 TEXT    NOT NULL UNIQUE,
    enabled              INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    schedule_spec        TEXT    NOT NULL,
    prompt               TEXT    NOT NULL DEFAULT '',
    profile              TEXT    NOT NULL DEFAULT '',
    script               TEXT    NOT NULL DEFAULT '',
    no_agent             INTEGER NOT NULL DEFAULT 0 CHECK (no_agent IN (0, 1)),
    suppress_token       TEXT,
    failure_deliver      TEXT    NOT NULL DEFAULT 'notify'
                         CHECK (failure_deliver IN ('notify', 'log', 'silent')),
    max_runs             INTEGER,
    expires_at           INTEGER,
    run_count            INTEGER NOT NULL DEFAULT 0,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    next_run_at          INTEGER,
    last_run_at          INTEGER,
    last_status          TEXT,
    last_error           TEXT,
    created_at           INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL
) STRICT;

CREATE INDEX idx_schedules_due ON schedules(next_run_at ASC)
    WHERE enabled = 1 AND next_run_at IS NOT NULL;
"#;

const SCHEMA_V3: &str = r#"
ALTER TABLE schedules ADD COLUMN managed INTEGER NOT NULL DEFAULT 0 CHECK (managed IN (0, 1));
"#;

const SCHEMA_V4: &str = r#"
ALTER TABLE turns ADD COLUMN widgets_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE turns ADD COLUMN origin TEXT NOT NULL DEFAULT 'cli'
    CHECK (origin IN ('cli', 'voice', 'typed', 'schedule', 'schedule_script', 'distill'));
ALTER TABLE turns ADD COLUMN schedule_id INTEGER;

CREATE INDEX idx_turns_distill ON turns(created_at DESC, id DESC)
    WHERE status = 'completed'
      AND schedule_id IS NULL
      AND origin NOT IN ('schedule', 'schedule_script', 'distill');
CREATE INDEX IF NOT EXISTS idx_turns_schedule_id ON turns(schedule_id, id DESC)
    WHERE schedule_id IS NOT NULL;
"#;

const SCHEMA_V5: &str = r#"
CREATE TABLE draft_checkpoints (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    turn_id          INTEGER,
    token            TEXT    NOT NULL UNIQUE,
    channel          TEXT    NOT NULL,
    app_name         TEXT    NOT NULL,
    sender           TEXT    NOT NULL,
    chat_target      TEXT    NOT NULL DEFAULT '',
    source_notif_id  INTEGER,
    window_class     TEXT    NOT NULL DEFAULT '',
    context_note     TEXT    NOT NULL DEFAULT '',
    draft_text       TEXT    NOT NULL DEFAULT '',
    status           TEXT    NOT NULL DEFAULT 'pending'
                     CHECK (status IN ('pending', 'consumed', 'dismissed', 'expired')),
    consumed_action  TEXT
                     CHECK (consumed_action IS NULL OR consumed_action IN ('copy', 'paste', 'edit', 'dismiss')),
    created_at       INTEGER NOT NULL,
    expires_at       INTEGER NOT NULL,
    consumed_at      INTEGER
) STRICT;

CREATE INDEX idx_draft_checkpoints_expires
    ON draft_checkpoints(expires_at ASC)
    WHERE status = 'pending';

CREATE INDEX idx_draft_checkpoints_turn_id
    ON draft_checkpoints(turn_id)
    WHERE turn_id IS NOT NULL;

CREATE INDEX idx_draft_checkpoints_chat
    ON draft_checkpoints(channel, chat_target)
    WHERE status = 'pending';

CREATE INDEX idx_draft_checkpoints_source_notif
    ON draft_checkpoints(source_notif_id)
    WHERE source_notif_id IS NOT NULL;
"#;

const IDX_TURNS_SCHEDULE_ID: &str = "CREATE INDEX IF NOT EXISTS idx_turns_schedule_id \
    ON turns(schedule_id, id DESC) WHERE schedule_id IS NOT NULL;";

fn migrate(conn: &mut Connection) -> Result<()> {
    let cur: i32 = conn.query_row("PRAGMA user_version;", [], |r| r.get(0))?;
    if cur == DB_SCHEMA_VERSION {
        conn.execute_batch(IDX_TURNS_SCHEDULE_ID)
            .context("ensuring idx_turns_schedule_id index")?;
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut version: i32 = tx.query_row("PRAGMA user_version;", [], |r| r.get(0))?;
    if version > DB_SCHEMA_VERSION {
        bail!(
            "history database was created by a newer version of hey-computer (schema v{version}, supported v{DB_SCHEMA_VERSION})"
        );
    }
    if version == 0 {
        tx.execute_batch(SCHEMA_V1)
            .context("initializing history database schema v1")?;
        version = 1;
    }
    if version == 1 {
        tx.execute_batch(SCHEMA_V2)
            .context("migrating history database schema v1 -> v2")?;
        version = 2;
    }
    if version == 2 {
        tx.execute_batch(SCHEMA_V3)
            .context("migrating history database schema v2 -> v3")?;
        version = 3;
    }
    if version == 3 {
        tx.execute_batch(SCHEMA_V4)
            .context("migrating history database schema v3 -> v4")?;
        version = 4;
    }
    if version == 4 {
        tx.execute_batch(SCHEMA_V5)
            .context("migrating history database schema v4 -> v5")?;
        version = 5;
    }
    tx.pragma_update(None, "user_version", version)?;
    tx.commit()?;
    Ok(())
}

/// Optimize the FTS5 index, run SQLite's query-planner optimizer, and reclaim
/// up to 256 freed pages via incremental vacuum.
fn maintain_connection(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "INSERT INTO turns_fts(turns_fts) VALUES('optimize');
         PRAGMA optimize;
         PRAGMA incremental_vacuum(256);",
    )
    .context("running SQLite/FTS5 maintenance")?;
    Ok(())
}

fn import_legacy_last_json(conn: &mut Connection, state_dir: &Path) -> Result<()> {
    let already: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'migrated_last_json'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if already.is_some() {
        return Ok(());
    }
    let path = state_dir.join(LAST_FILE);
    let legacy = match load_legacy_last_json(state_dir) {
        Ok(Some(last)) => last,
        Ok(None) => return Ok(()),
        Err(e) => {
            tracing::warn!("skipping unreadable legacy last.json during migration: {e:#}");
            return Ok(());
        }
    };

    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let ts_ms = (legacy.timestamp as i64).saturating_mul(1000);
    let denied_json = serde_json::to_string(&legacy.denied).unwrap_or_else(|_| "[]".to_string());
    let attachments_json =
        serde_json::to_string(&legacy.attachments).unwrap_or_else(|_| "[]".to_string());
    if legacy.id > 0 {
        tx.execute(
            "INSERT OR IGNORE INTO turns
             (id, lane, status, profile, created_at, started_at, finished_at, request, answer, denied_json, attachments_json)
             VALUES (?1, 'interactive', 'completed', '', ?2, ?2, ?2, ?3, ?4, ?5, ?6)",
            params![
                legacy.id as i64,
                ts_ms,
                legacy.request,
                legacy.text,
                denied_json,
                attachments_json
            ],
        )?;
    } else {
        tx.execute(
            "INSERT INTO turns
             (lane, status, profile, created_at, started_at, finished_at, request, answer, denied_json, attachments_json)
             VALUES ('interactive', 'completed', '', ?1, ?1, ?1, ?2, ?3, ?4, ?5)",
            params![ts_ms, legacy.request, legacy.text, denied_json, attachments_json],
        )?;
    }
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('migrated_last_json', '1')",
        [],
    )?;
    tx.commit()?;

    if let Err(e) = std::fs::remove_file(&path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("migrated legacy last.json but could not remove it: {e}");
    }
    tracing::info!("migrated legacy last.json into history.db");
    Ok(())
}

fn recover_interrupted(conn: &Connection, now_ms: i64) -> Result<usize> {
    let n = conn.execute(
        "UPDATE turns
         SET status = 'failed',
             fail_reason = 'agent_down',
             error_message = 'daemon restarted before request finished',
             finished_at = ?1
         WHERE status IN ('queued', 'running')",
        params![now_ms],
    )?;
    Ok(n)
}

fn read_last_id(conn: &Connection) -> Result<u64> {
    let max_turn: i64 =
        conn.query_row("SELECT COALESCE(MAX(id), 0) FROM turns", [], |r| r.get(0))?;
    let seq: Option<i64> = conn
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'turns'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(max_turn.max(seq.unwrap_or(0)).max(0) as u64)
}

fn insert_turn(conn: &Connection, turn: &NewTurn) -> Result<()> {
    let attachments_json =
        serde_json::to_string(&turn.attachments).unwrap_or_else(|_| "[]".to_string());
    let started = (turn.status == TurnStatus::Running).then_some(turn.created_at_ms);
    conn.execute(
        "INSERT INTO turns (id, lane, status, profile, created_at, started_at, request, attachments_json, origin, schedule_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            turn.id as i64,
            turn.lane.as_str(),
            turn.status.as_str(),
            turn.profile,
            turn.created_at_ms,
            started,
            turn.request,
            attachments_json,
            turn.origin.as_str(),
            turn.schedule_id.map(|s| s as i64),
        ],
    )?;
    Ok(())
}

fn prune_connection(conn: &Connection, history: &HistoryConfig, now_ms: i64) -> Result<usize> {
    let mut deleted = 0usize;
    if history.suppressed_days > 0.0 && history.suppressed_days.is_finite() {
        let cutoff = now_ms - (history.suppressed_days * 86_400_000.0) as i64;
        deleted += conn.execute(
            "DELETE FROM turns WHERE status = 'suppressed' AND created_at < ?1",
            params![cutoff],
        )?;
    }
    if history.retention_days > 0.0 && history.retention_days.is_finite() {
        let cutoff = now_ms - (history.retention_days * 86_400_000.0) as i64;
        deleted += conn.execute(
            "DELETE FROM turns
             WHERE status NOT IN ('queued', 'running') AND created_at < ?1",
            params![cutoff],
        )?;
        if has_schedules_table(conn).unwrap_or(false) {
            let _ = conn.execute(
                "DELETE FROM schedules
                 WHERE managed = 0 AND enabled = 0 AND updated_at < ?1",
                params![cutoff],
            );
        }
    }
    if history.max_entries > 0 {
        // Prioritize preserving completed/failed/cancelled user turns ahead of
        // suppressed heartbeat rows when enforcing `max_entries`.
        deleted += conn.execute(
            "DELETE FROM turns
             WHERE status NOT IN ('queued', 'running')
               AND id NOT IN (
                   SELECT id FROM turns
                   WHERE status NOT IN ('queued', 'running')
                   ORDER BY CASE WHEN status = 'suppressed' THEN 1 ELSE 0 END ASC, id DESC
                   LIMIT ?1
               )",
            params![history.max_entries as i64],
        )?;
    }
    if has_draft_checkpoints_table(conn).unwrap_or(false) {
        // Expire overdue pending draft checkpoints and prune aged/excess terminal rows.
        let _ = conn.execute(
            "UPDATE draft_checkpoints
             SET status = 'expired'
             WHERE status = 'pending' AND expires_at <= ?1",
            params![now_ms],
        );
        let age_days = if history.suppressed_days > 0.0 && history.suppressed_days.is_finite() {
            history.suppressed_days
        } else {
            history.retention_days
        };
        if age_days > 0.0 && age_days.is_finite() {
            let cutoff = now_ms - (age_days * 86_400_000.0) as i64;
            deleted += conn.execute(
                "DELETE FROM draft_checkpoints
                 WHERE status != 'pending' AND created_at < ?1",
                params![cutoff],
            )?;
        }
        deleted += conn.execute(
            "DELETE FROM draft_checkpoints
             WHERE status != 'pending'
               AND id NOT IN (
                   SELECT id FROM draft_checkpoints
                   WHERE status != 'pending'
                   ORDER BY id DESC
                   LIMIT 1000
               )",
            [],
        )?;
    }
    if deleted > 0 {
        let _ = conn.execute_batch("PRAGMA optimize;");
        if deleted >= 500 {
            let _ = conn.execute_batch("PRAGMA incremental_vacuum;");
        }
    }
    Ok(deleted)
}

fn row_to_turn(row: &rusqlite::Row<'_>) -> rusqlite::Result<TurnRecord> {
    let id: i64 = row.get(0)?;
    let lane_str: String = row.get(1)?;
    let status_str: String = row.get(2)?;
    let profile: String = row.get(3)?;
    let session_id: Option<String> = row.get(4)?;
    let conversation_id: Option<i64> = row.get(5)?;
    let created_at_ms: i64 = row.get(6)?;
    let started_at_ms: Option<i64> = row.get(7)?;
    let finished_at_ms: Option<i64> = row.get(8)?;
    let request: String = row.get(9)?;
    let answer: String = row.get(10)?;
    let denied_json: String = row.get(11)?;
    let attachments_json: String = row.get(12)?;
    let fail_reason: Option<String> = row.get(13)?;
    let error_message: Option<String> = row.get(14)?;
    let suppressed_token: Option<String> = row.get(15)?;
    let widgets_json: String = row.get(16)?;
    let origin_str: String = row.get(17)?;
    let schedule_id: Option<i64> = row.get(18)?;

    Ok(TurnRecord {
        id: id.max(0) as u64,
        lane: Lane::parse(&lane_str).unwrap_or(Lane::Interactive),
        status: TurnStatus::parse(&status_str).unwrap_or(TurnStatus::Failed),
        profile,
        session_id,
        conversation_id: conversation_id.map(|c| c.max(0) as u64),
        created_at_ms,
        started_at_ms,
        finished_at_ms,
        request,
        answer,
        denied: serde_json::from_str(&denied_json).unwrap_or_default(),
        attachments: serde_json::from_str(&attachments_json).unwrap_or_default(),
        fail_reason,
        error_message,
        suppressed_token,
        widgets: serde_json::from_str(&widgets_json).unwrap_or_default(),
        origin: TurnOrigin::parse(&origin_str),
        schedule_id: schedule_id.map(|s| s.max(0) as u64),
    })
}

const TURN_COLUMNS: &str = "id, lane, status, profile, session_id, conversation_id, \
    created_at, started_at, finished_at, request, answer, denied_json, attachments_json, \
    fail_reason, error_message, suppressed_token, widgets_json, origin, schedule_id";

/// Load the newest completed answer in `lane` (`None` searches across all lanes),
/// falling back to legacy `<state_dir>/last.json` when `history.db` does not exist.
pub fn load_last_for_lane(state_dir: &Path, lane: Option<Lane>) -> Result<Option<LastAnswer>> {
    if let Some(conn) = open_reader_connection(state_dir)? {
        let turn = match lane {
            Some(lane) => conn
                .query_row(
                    &format!(
                        "SELECT {TURN_COLUMNS} FROM turns
                         WHERE status = 'completed'
                           AND (length(trim(answer)) > 0 OR widgets_json != '[]')
                           AND lane = ?1
                         ORDER BY id DESC LIMIT 1"
                    ),
                    params![lane.as_str()],
                    row_to_turn,
                )
                .optional()?,
            None => conn
                .query_row(
                    &format!(
                        "SELECT {TURN_COLUMNS} FROM turns
                         WHERE status = 'completed'
                           AND (length(trim(answer)) > 0 OR widgets_json != '[]')
                         ORDER BY id DESC LIMIT 1"
                    ),
                    [],
                    row_to_turn,
                )
                .optional()?,
        };
        return Ok(turn.map(LastAnswer::from));
    }
    if lane == Some(Lane::Background) || lane == Some(Lane::Restricted) {
        return Ok(None);
    }
    load_legacy_last_json(state_dir)
}

/// Load the newest completed interactive answer (or legacy `last.json`).
pub fn load_last(state_dir: &Path) -> Result<Option<LastAnswer>> {
    load_last_for_lane(state_dir, Some(Lane::Interactive))
}

fn load_legacy_last_json(state_dir: &Path) -> Result<Option<LastAnswer>> {
    let path = state_dir.join(LAST_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let answer: LastAnswer =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(answer))
}

/// Atomically write legacy `<state_dir>/last.json` with mode `0600` (retained
/// for backwards-compatibility tests).
pub fn save_last(state_dir: &Path, answer: &LastAnswer) -> Result<()> {
    ensure_private_dir(state_dir)?;
    let payload = serde_json::to_vec(answer).context("encoding the last answer")?;
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = state_dir.join(format!("last.{}.{n}.tmp", std::process::id()));
    let write_result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting {}", tmp.display()))?;
        file.write_all(&payload)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.flush()
            .with_context(|| format!("flushing {}", tmp.display()))?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    let target = state_dir.join(LAST_FILE);
    if let Err(e) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("replacing {}", target.display()));
    }
    Ok(())
}

/// Filter for listing turns/tasks from `history.db`.
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub lane: Option<Lane>,
    pub status: Option<TurnStatus>,
    pub include_suppressed: bool,
    pub limit: usize,
}

/// List recent turns matching `filter`, newest first.
pub fn list_turns(state_dir: &Path, filter: &ListFilter) -> Result<Vec<TurnRecord>> {
    let Some(conn) = open_reader_connection(state_dir)? else {
        return Ok(Vec::new());
    };
    let limit = filter.limit.clamp(1, 200) as i64;
    let lane_param = filter.lane.map(Lane::as_str);
    let status_param = filter.status.map(TurnStatus::as_str);
    let hide_suppressed = i64::from(filter.status.is_none() && !filter.include_suppressed);

    let mut stmt = conn.prepare(&format!(
        "SELECT {TURN_COLUMNS}
         FROM turns
         WHERE (?1 IS NULL OR lane = ?1)
           AND (?2 IS NULL OR status = ?2)
           AND (?3 = 0 OR status != 'suppressed')
         ORDER BY id DESC
         LIMIT ?4"
    ))?;
    let rows = stmt.query_map(
        params![lane_param, status_param, hide_suppressed, limit],
        row_to_turn,
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Fetch one turn by ID.
pub fn get_turn(state_dir: &Path, id: u64) -> Result<Option<TurnRecord>> {
    let Some(conn) = open_reader_connection(state_dir)? else {
        return Ok(None);
    };
    let turn = conn
        .query_row(
            &format!("SELECT {TURN_COLUMNS} FROM turns WHERE id = ?1"),
            params![id as i64],
            row_to_turn,
        )
        .optional()?;
    Ok(turn)
}

/// List recent turns linked to `schedule_id`, newest first (`ORDER BY id DESC`).
///
/// When `schedule_id` has zero directly linked turns and the schedule row in
/// `schedules` is a distillation job (name or script references `distill`),
/// falls back to pre-existing `origin = 'distill'` turns so historical
/// `weekly-distill` runs remain inspectable.
pub fn list_turns_for_schedule(
    state_dir: &Path,
    schedule_id: u64,
    limit: usize,
) -> Result<Vec<TurnRecord>> {
    let Some(conn) = open_reader_connection(state_dir)? else {
        return Ok(Vec::new());
    };
    let cap = limit.clamp(1, 200) as i64;
    let mut stmt = conn.prepare(&format!(
        "SELECT {TURN_COLUMNS}
         FROM turns
         WHERE schedule_id = ?1
         ORDER BY id DESC
         LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![schedule_id as i64, cap], row_to_turn)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    if out.is_empty() && has_schedules_table(&conn)? {
        let is_distill: bool = conn
            .query_row(
                "SELECT (lower(name) LIKE '%distill%' OR lower(script) LIKE '%distill%')
                 FROM schedules WHERE id = ?1",
                params![schedule_id as i64],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0)
            != 0;
        if is_distill {
            let mut fallback = conn.prepare(&format!(
                "SELECT {TURN_COLUMNS}
                 FROM turns
                 WHERE origin = 'distill'
                 ORDER BY id DESC
                 LIMIT ?1"
            ))?;
            let fb_rows = fallback.query_map(params![cap], row_to_turn)?;
            for row in fb_rows {
                out.push(row?);
            }
        }
    }
    Ok(out)
}

/// List recent turns belonging to a [`ScheduleRecord`], newest first, with
/// automatic `origin = 'distill'` fallback for distill jobs.
pub fn list_turns_for_schedule_job(
    state_dir: &Path,
    job: &ScheduleRecord,
    limit: usize,
) -> Result<Vec<TurnRecord>> {
    let turns = list_turns_for_schedule(state_dir, job.id, limit)?;
    if !turns.is_empty() {
        return Ok(turns);
    }
    let looks_distill = job.name.to_ascii_lowercase().contains("distill")
        || job.script.to_ascii_lowercase().contains("distill");
    if !looks_distill {
        return Ok(turns);
    }
    let Some(conn) = open_reader_connection(state_dir)? else {
        return Ok(Vec::new());
    };
    let cap = limit.clamp(1, 200) as i64;
    let mut stmt = conn.prepare(&format!(
        "SELECT {TURN_COLUMNS}
         FROM turns
         WHERE origin = 'distill'
         ORDER BY id DESC
         LIMIT ?1"
    ))?;
    let rows = stmt.query_map(params![cap], row_to_turn)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Load completed turns belonging to the same conversation as `target_id`,
/// ordered chronologically (`id ASC`) up to and including `target_id` (or the
/// whole thread when `include_later` is `true`). Handles pre-v4 background
/// rows whose `conversation_id` was `NULL` by including `id = root_cid`.
pub fn conversation_turns(
    state_dir: &Path,
    target_id: u64,
    include_later: bool,
    max_turns: usize,
) -> Result<Vec<TurnRecord>> {
    let Some(target) = get_turn(state_dir, target_id)? else {
        return Ok(Vec::new());
    };
    let Some(conn) = open_reader_connection(state_dir)? else {
        return Ok(Vec::new());
    };
    let root_cid = target.conversation_id.unwrap_or(target.id) as i64;
    let upper = if include_later {
        i64::MAX
    } else {
        target_id as i64
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT {TURN_COLUMNS}
         FROM turns
         WHERE (conversation_id = ?1 OR id = ?1)
           AND id <= ?2
           AND status = 'completed'
         ORDER BY id ASC"
    ))?;
    let rows = stmt.query_map(params![root_cid, upper], row_to_turn)?;
    let mut turns = Vec::new();
    for row in rows {
        turns.push(row?);
    }
    if max_turns > 0 && turns.len() > max_turns {
        let skip = turns.len() - max_turns;
        turns.drain(0..skip);
    }
    Ok(turns)
}

/// Eligible user-originated turns for `hey-computer distill`, plus how many
/// automated/scheduled turns were skipped in the same time window.
#[derive(Debug, Clone, Default)]
pub struct DistillCandidates {
    pub turns: Vec<TurnRecord>,
    pub skipped_scheduled_turns: usize,
}

/// Query completed user-originated turns (`schedule_id IS NULL`, non-scheduled
/// `origin`, no `suppressed_token`) since `since_ms`, ordered chronologically
/// (`id ASC`) after selecting up to `limit` most recent turns.
pub fn query_distill_candidates(
    state_dir: &Path,
    since_ms: Option<i64>,
    limit: usize,
) -> Result<DistillCandidates> {
    let Some(conn) = open_reader_connection(state_dir)? else {
        return Ok(DistillCandidates::default());
    };
    let skipped: i64 = conn.query_row(
        "SELECT COUNT(*) FROM turns
         WHERE (?1 IS NULL OR created_at >= ?1)
           AND status IN ('completed', 'suppressed')
           AND (schedule_id IS NOT NULL
                OR origin IN ('schedule', 'schedule_script', 'distill')
                OR lane = 'restricted'
                OR suppressed_token IS NOT NULL
                OR status = 'suppressed')",
        params![since_ms],
        |r| r.get(0),
    )?;
    let cap = limit.clamp(1, 500) as i64;
    let mut stmt = conn.prepare(&format!(
        "SELECT {TURN_COLUMNS} FROM turns
         WHERE status = 'completed'
           AND length(trim(answer)) > 0
           AND schedule_id IS NULL
           AND origin NOT IN ('schedule', 'schedule_script', 'distill')
           AND lane != 'restricted'
           AND suppressed_token IS NULL
           AND (?1 IS NULL OR created_at >= ?1)
         ORDER BY created_at DESC, id DESC
         LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![since_ms, cap], row_to_turn)?;
    let mut turns = Vec::new();
    for row in rows {
        turns.push(row?);
    }
    turns.reverse();
    Ok(DistillCandidates {
        turns,
        skipped_scheduled_turns: skipped.max(0) as usize,
    })
}

/// Read persisted runtime speak toggle (`<state_dir>/speak_enabled`), falling
/// back to `default_enabled` when absent.
pub fn load_speak_state(state_dir: &Path, default_enabled: bool) -> bool {
    let path = state_dir.join(SPEAK_STATE_FILE);
    match std::fs::read_to_string(path) {
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "on" | "yes" => true,
            "0" | "false" | "off" | "no" => false,
            _ => default_enabled,
        },
        Err(_) => default_enabled,
    }
}

/// Persist runtime speak toggle (`<state_dir>/speak_enabled`, mode `0600`).
pub fn save_speak_state(state_dir: &Path, enabled: bool) -> Result<()> {
    ensure_private_dir(state_dir)?;
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = state_dir.join(format!("{SPEAK_STATE_FILE}.{}.{n}.tmp", std::process::id()));
    let payload = if enabled { b"on\n" as &[u8] } else { b"off\n" };
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(payload)?;
        f.flush()?;
    }
    let target = state_dir.join(SPEAK_STATE_FILE);
    if let Err(e) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("replacing {}", target.display()));
    }
    Ok(())
}

/// Today's local calendar date formatted as `YYYY-MM-DD` via `localtime_r`.
pub fn local_today_ymd() -> String {
    let secs = unix_timestamp_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        tzset();
        !libc::localtime_r(&secs, &mut tm).is_null()
    };
    if !ok {
        return "1970-01-01".to_string();
    }
    format!(
        "{:04}-{:02}-{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallSort {
    Relevance,
    Recent,
}

#[derive(Debug, Clone)]
pub struct RecallOptions {
    pub query: Option<String>,
    pub lane: Option<Lane>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub limit: usize,
    pub sort: RecallSort,
    pub match_any: bool,
    pub raw_fts: bool,
    pub user_only: bool,
}

impl Default for RecallOptions {
    fn default() -> Self {
        Self {
            query: None,
            lane: None,
            since_ms: None,
            until_ms: None,
            limit: 5,
            sort: RecallSort::Relevance,
            match_any: false,
            raw_fts: false,
            user_only: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecallHit {
    pub id: u64,
    pub lane: Lane,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<u64>,
    #[serde(default)]
    pub origin: TurnOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_id: Option<u64>,
    #[serde(default)]
    pub widget_count: usize,
    pub created_at_ms: i64,
    pub created_local: String,
    pub request: String,
    pub answer: String,
    pub req_snippet: String,
    pub ans_snippet: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

/// Build a safe FTS5 `MATCH` expression from free-form text, neutralizing
/// operators (`AND`/`OR`/`NOT`/`NEAR`), column filters (`foo:bar`), hyphens,
/// and unmatched quotes. Returns `None` when no searchable tokens remain.
pub fn build_fts_query(input: &str, match_any: bool) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        if chars[i] == '"' {
            // Look ahead for a matching closing quote.
            if let Some(rel_end) = chars[i + 1..].iter().position(|&c| c == '"') {
                let end = i + 1 + rel_end;
                let phrase: String = chars[i + 1..end].iter().collect();
                let trimmed = phrase.trim();
                if trimmed.chars().any(char::is_alphanumeric) {
                    terms.push(format!("\"{}\"", trimmed.replace('"', "\"\"")));
                }
                i = end + 1;
                continue;
            }
            // Unmatched opening quote: drop it and continue scanning tokens.
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '"' {
            i += 1;
        }
        let raw: String = chars[start..i].iter().collect();
        let (prefix, core) = match raw.strip_suffix('*') {
            Some(rest) if !rest.is_empty() => (true, rest),
            _ => (false, raw.as_str()),
        };
        if !core.chars().any(char::is_alphanumeric) {
            continue;
        }
        let escaped = core.replace('"', "\"\"");
        if prefix {
            terms.push(format!("\"{escaped}\"*"));
        } else {
            terms.push(format!("\"{escaped}\""));
        }
    }

    if terms.is_empty() {
        None
    } else if match_any {
        Some(terms.join(" OR "))
    } else {
        Some(terms.join(" "))
    }
}

/// Search completed non-empty turns using FTS5 (or chronological scan when no
/// query is provided).
pub fn recall(state_dir: &Path, opts: &RecallOptions) -> Result<Vec<RecallHit>> {
    let Some(conn) = open_reader_connection(state_dir)? else {
        return Ok(Vec::new());
    };
    let limit = opts.limit.clamp(1, 50) as i64;
    let lane_param = opts.lane.map(Lane::as_str);

    let raw_query = opts
        .query
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty());

    let user_only_flag = i64::from(opts.user_only);

    let Some(raw_query) = raw_query else {
        let mut stmt = conn.prepare(
            "SELECT id, lane, profile, conversation_id, origin, schedule_id, widgets_json,
                    created_at, request, answer
             FROM turns
             WHERE status = 'completed'
               AND (length(trim(answer)) > 0 OR widgets_json != '[]')
               AND (?1 IS NULL OR lane = ?1)
               AND (?2 IS NULL OR created_at >= ?2)
               AND (?3 IS NULL OR created_at < ?3)
               AND (?4 = 0 OR (schedule_id IS NULL AND origin NOT IN ('schedule', 'schedule_script', 'distill') AND lane != 'restricted'))
             ORDER BY created_at DESC, id DESC
             LIMIT ?5",
        )?;
        let rows = stmt.query_map(
            params![
                lane_param,
                opts.since_ms,
                opts.until_ms,
                user_only_flag,
                limit
            ],
            |row| {
                let id: i64 = row.get(0)?;
                let lane_str: String = row.get(1)?;
                let profile: String = row.get(2)?;
                let conversation_id: Option<i64> = row.get(3)?;
                let origin_str: String = row.get(4)?;
                let schedule_id: Option<i64> = row.get(5)?;
                let widgets_json: String = row.get(6)?;
                let created_at_ms: i64 = row.get(7)?;
                let request: String = row.get(8)?;
                let answer: String = row.get(9)?;
                let widgets: Vec<WidgetSpec> =
                    serde_json::from_str(&widgets_json).unwrap_or_default();
                let widget_count = widgets.len();
                let ans_snippet = if answer.trim().is_empty() && !widgets.is_empty() {
                    summarize_widgets(&widgets)
                } else {
                    truncate_chars(&answer, 240)
                };
                Ok(RecallHit {
                    id: id.max(0) as u64,
                    lane: Lane::parse(&lane_str).unwrap_or(Lane::Interactive),
                    profile,
                    conversation_id: conversation_id.map(|c| c.max(0) as u64),
                    origin: TurnOrigin::parse(&origin_str),
                    schedule_id: schedule_id.map(|s| s.max(0) as u64),
                    widget_count,
                    created_at_ms,
                    created_local: format_local_timestamp(created_at_ms),
                    req_snippet: truncate_chars(&request, 120),
                    ans_snippet,
                    request,
                    answer,
                    score: None,
                })
            },
        )?;
        let mut hits = Vec::new();
        for row in rows {
            hits.push(row?);
        }
        return Ok(hits);
    };

    let fts_query = if opts.raw_fts {
        raw_query.to_string()
    } else {
        match build_fts_query(raw_query, opts.match_any) {
            Some(q) => q,
            None => return Ok(Vec::new()),
        }
    };

    let now_ms = unix_timestamp_ms();
    let order = match opts.sort {
        RecallSort::Relevance => "score ASC, t.created_at DESC, t.id DESC",
        RecallSort::Recent => "t.created_at DESC, t.id DESC",
    };
    let sql = format!(
        "SELECT
             t.id, t.lane, t.profile, t.conversation_id, t.origin, t.schedule_id, t.widgets_json,
             t.created_at, t.request, t.answer,
             snippet(turns_fts, 0, '«', '»', '…', 20) AS req_snippet,
             snippet(turns_fts, 1, '«', '»', '…', 48) AS ans_snippet,
             bm25(turns_fts, 1.5, 1.0)
               / (1.0 + (MAX(0, ?1 - t.created_at) / (86400000.0 * 30.0))) AS score
         FROM turns_fts
         JOIN turns t ON t.id = turns_fts.rowid
         WHERE turns_fts MATCH ?2
           AND t.status = 'completed'
           AND length(trim(t.answer)) > 0
           AND (?3 IS NULL OR t.lane = ?3)
           AND (?4 IS NULL OR t.created_at >= ?4)
           AND (?5 IS NULL OR t.created_at < ?5)
           AND (?6 = 0 OR (t.schedule_id IS NULL AND t.origin NOT IN ('schedule', 'schedule_script', 'distill') AND t.lane != 'restricted'))
         ORDER BY {order}
         LIMIT ?7"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(
            params![
                now_ms,
                fts_query,
                lane_param,
                opts.since_ms,
                opts.until_ms,
                user_only_flag,
                limit
            ],
            |row| {
                let id: i64 = row.get(0)?;
                let lane_str: String = row.get(1)?;
                let profile: String = row.get(2)?;
                let conversation_id: Option<i64> = row.get(3)?;
                let origin_str: String = row.get(4)?;
                let schedule_id: Option<i64> = row.get(5)?;
                let widgets_json: String = row.get(6)?;
                let created_at_ms: i64 = row.get(7)?;
                let request: String = row.get(8)?;
                let answer: String = row.get(9)?;
                let req_snippet: String = row.get(10)?;
                let ans_snippet: String = row.get(11)?;
                let score: f64 = row.get(12)?;
                let widget_count = serde_json::from_str::<Vec<serde_json::Value>>(&widgets_json)
                    .map(|v| v.len())
                    .unwrap_or(0);
                Ok(RecallHit {
                    id: id.max(0) as u64,
                    lane: Lane::parse(&lane_str).unwrap_or(Lane::Interactive),
                    profile,
                    conversation_id: conversation_id.map(|c| c.max(0) as u64),
                    origin: TurnOrigin::parse(&origin_str),
                    schedule_id: schedule_id.map(|s| s.max(0) as u64),
                    widget_count,
                    created_at_ms,
                    created_local: format_local_timestamp(created_at_ms),
                    request,
                    answer,
                    req_snippet,
                    ans_snippet,
                    score: Some(score),
                })
            },
        )
        .context("running FTS5 recall query")?;
    let mut hits = Vec::new();
    for row in rows {
        hits.push(row?);
    }
    Ok(hits)
}

/// Parse a human time bound (`24h`, `7d`, `2w`, `today`, `yesterday`, or
/// `YYYY-MM-DD` at local midnight) into Unix epoch milliseconds.
pub fn parse_time_spec(spec: &str, now_ms: i64) -> Result<i64> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        bail!("time specification is empty");
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower == "today" {
        return local_midnight_ms(now_ms, 0);
    }
    if lower == "yesterday" {
        return local_midnight_ms(now_ms, -1);
    }
    if let Some((num_str, unit)) = lower
        .len()
        .checked_sub(1)
        .map(|split| lower.split_at(split))
        && matches!(unit, "m" | "h" | "d" | "w")
        && let Ok(n) = num_str.parse::<i64>()
        && n >= 0
    {
        let mult_ms: i64 = match unit {
            "m" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            "w" => 7 * 86_400_000,
            _ => unreachable!(),
        };
        return Ok(now_ms.saturating_sub(n.saturating_mul(mult_ms)));
    }

    let parts: Vec<&str> = trimmed.split('-').collect();
    if parts.len() == 3
        && let (Ok(year), Ok(month), Ok(day)) = (
            parts[0].parse::<i32>(),
            parts[1].parse::<i32>(),
            parts[2].parse::<i32>(),
        )
        && (1970..=9999).contains(&year)
        && (1..=12).contains(&month)
        && (1..=31).contains(&day)
    {
        return local_date_to_ms(year, month, day);
    }

    bail!(
        "unrecognized time `{spec}` (expected e.g. 24h, 7d, 2w, today, yesterday, or YYYY-MM-DD)"
    );
}

/// Parse an `--until` time bound: relative specs (`24h`, `7d`) behave like
/// [`parse_time_spec`], while calendar dates (`today`, `yesterday`,
/// `YYYY-MM-DD`) resolve to `00:00:00` of the **next** local day so
/// `created_at < ?until` includes the entire named calendar day.
pub fn parse_until_time_spec(spec: &str, now_ms: i64) -> Result<i64> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        bail!("time specification is empty");
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower == "today" {
        return local_midnight_ms(now_ms, 1);
    }
    if lower == "yesterday" {
        return local_midnight_ms(now_ms, 0);
    }
    let parts: Vec<&str> = trimmed.split('-').collect();
    if parts.len() == 3
        && let (Ok(year), Ok(month), Ok(day)) = (
            parts[0].parse::<i32>(),
            parts[1].parse::<i32>(),
            parts[2].parse::<i32>(),
        )
        && (1970..=9999).contains(&year)
        && (1..=12).contains(&month)
        && (1..=31).contains(&day)
    {
        return local_date_end_to_ms(year, month, day);
    }
    parse_time_spec(spec, now_ms)
}

/// Parse a single calendar day (`today`, `yesterday`, or `YYYY-MM-DD`) into
/// `[start_ms, end_ms)` spanning local `00:00:00` through `23:59:59.999`.
pub fn parse_day_range(spec: &str, now_ms: i64) -> Result<(i64, i64)> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        bail!("date specification is empty (expected `today`, `yesterday`, or `YYYY-MM-DD`)");
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower == "today" {
        return Ok((local_midnight_ms(now_ms, 0)?, local_midnight_ms(now_ms, 1)?));
    }
    if lower == "yesterday" {
        return Ok((
            local_midnight_ms(now_ms, -1)?,
            local_midnight_ms(now_ms, 0)?,
        ));
    }
    let parts: Vec<&str> = trimmed.split('-').collect();
    if parts.len() == 3
        && let (Ok(year), Ok(month), Ok(day)) = (
            parts[0].parse::<i32>(),
            parts[1].parse::<i32>(),
            parts[2].parse::<i32>(),
        )
        && (1970..=9999).contains(&year)
        && (1..=12).contains(&month)
        && (1..=31).contains(&day)
    {
        return Ok((
            local_date_to_ms(year, month, day)?,
            local_date_end_to_ms(year, month, day)?,
        ));
    }
    bail!("unrecognized date `{spec}` (expected `today`, `yesterday`, or `YYYY-MM-DD`)");
}

unsafe extern "C" {
    fn tzset();
}

fn local_midnight_ms(now_ms: i64, day_offset: i32) -> Result<i64> {
    let secs = (now_ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        tzset();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            bail!("localtime_r failed");
        }
        tm.tm_hour = 0;
        tm.tm_min = 0;
        tm.tm_sec = 0;
        tm.tm_mday += day_offset;
        tm.tm_isdst = -1;
        let t = libc::mktime(&mut tm);
        if t == -1 {
            bail!("mktime failed");
        }
        Ok((t as i64) * 1000)
    }
}

fn local_date_to_ms(year: i32, month: i32, day: i32) -> Result<i64> {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = year - 1900;
    tm.tm_mon = month - 1;
    tm.tm_mday = day;
    tm.tm_isdst = -1;
    let t = unsafe {
        tzset();
        libc::mktime(&mut tm)
    };
    if t == -1 || tm.tm_year != year - 1900 || tm.tm_mon != month - 1 || tm.tm_mday != day {
        bail!("invalid calendar date {year:04}-{month:02}-{day:02}");
    }
    Ok((t as i64) * 1000)
}

fn local_date_end_to_ms(year: i32, month: i32, day: i32) -> Result<i64> {
    // Validate the calendar date first so `2027-02-31` is rejected.
    let _ = local_date_to_ms(year, month, day)?;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = year - 1900;
    tm.tm_mon = month - 1;
    tm.tm_mday = day + 1;
    tm.tm_isdst = -1;
    let t = unsafe {
        tzset();
        libc::mktime(&mut tm)
    };
    if t == -1 {
        bail!("invalid calendar date {year:04}-{month:02}-{day:02}");
    }
    Ok((t as i64) * 1000)
}

/// Format a Unix epoch ms timestamp as `YYYY-MM-DD HH:MM` in local time.
pub fn format_local_timestamp(epoch_ms: i64) -> String {
    let secs = (epoch_ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        tzset();
        !libc::localtime_r(&secs, &mut tm).is_null()
    };
    if !ok {
        return format!("{epoch_ms}");
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

/// Escape `<` / `>` and strip control/bidi characters (keeping `\n` and `\t`)
/// so recalled text cannot break out of `<untrusted_recall>` delimiters.
pub fn sanitize_recall_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if is_bidi_or_bad_control(c) {
            continue;
        }
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

fn is_bidi_or_bad_control(c: char) -> bool {
    if c.is_control() && !matches!(c, '\n' | '\t') {
        return true;
    }
    matches!(
        c,
        '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{061C}'
            | '\u{FEFF}'
    )
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", text[..idx].trim_end()),
        None => text.to_string(),
    }
}

fn summarize_widgets(widgets: &[WidgetSpec]) -> String {
    let titles: Vec<&str> = widgets
        .iter()
        .map(|w| w.title.trim())
        .filter(|t| !t.is_empty())
        .collect();
    if titles.is_empty() {
        format!("[{} widget(s)]", widgets.len())
    } else {
        truncate_chars(&format!("[widget: {}]", titles.join(", ")), 200)
    }
}

/// Format a compact relative duration string between `target_ms` and `now_ms`.
/// Past timestamps yield e.g. `"now"`, `"4m"`, `"2h"`, `"3d"`, `"2w"`; future
/// timestamps yield e.g. `"in <1m"`, `"in 14m"`, `"in 2h"`, `"in 3d"`.
pub fn format_relative_time(target_ms: i64, now_ms: i64) -> String {
    let diff_ms = target_ms - now_ms;
    if diff_ms > 0 {
        let secs = diff_ms / 1000;
        if secs < 60 {
            return "in <1m".to_string();
        }
        let mins = secs / 60;
        if mins < 60 {
            return format!("in {mins}m");
        }
        let hours = mins / 60;
        if hours < 48 {
            return format!("in {hours}h");
        }
        let days = hours / 24;
        if days < 14 {
            return format!("in {days}d");
        }
        return format!("in {}w", days / 7);
    }

    let ago_secs = (-diff_ms) / 1000;
    if ago_secs < 60 {
        return "now".to_string();
    }
    let mins = ago_secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h");
    }
    let days = hours / 24;
    if days < 14 {
        return format!("{days}d");
    }
    if days < 60 {
        return format!("{}w", days / 7);
    }
    let full = format_local_timestamp(target_ms);
    full.get(..10).unwrap_or(&full).to_string()
}

/// Format a past timestamp as `"just now"`, `"4m ago"`, `"2h ago"`, `"3d ago"`,
/// or a local date for older runs (used in `schedule list/show --json`).
pub fn format_relative_ago(target_ms: i64, now_ms: i64) -> String {
    let ago_secs = (now_ms - target_ms).max(0) / 1000;
    if ago_secs < 60 {
        return "just now".to_string();
    }
    let mins = ago_secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 48 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 14 {
        return format!("{days}d ago");
    }
    if days < 60 {
        return format!("{}w ago", days / 7);
    }
    let full = format_local_timestamp(target_ms);
    full.get(..10).unwrap_or(&full).to_string()
}

/// Format recall hits for text output, wrapped in `<untrusted_recall>` by
/// default for prompt-injection hygiene.
pub fn format_recall_output(
    hits: &[RecallHit],
    full: bool,
    max_chars: usize,
    plain: bool,
) -> String {
    if hits.is_empty() {
        return if plain {
            "No matching history.".to_string()
        } else {
            "<untrusted_recall>\nNo matching history.\n</untrusted_recall>".to_string()
        };
    }

    let mut body = String::new();
    for (idx, hit) in hits.iter().enumerate() {
        if idx > 0 {
            body.push_str("\n---\n");
        }
        let root_conv = hit.conversation_id.unwrap_or(hit.id);
        let profile_part = if hit.profile.is_empty() {
            String::new()
        } else {
            format!(" profile={}", hit.profile)
        };
        let widgets_part = if hit.widget_count > 0 {
            format!(" widgets={}", hit.widget_count)
        } else {
            String::new()
        };
        body.push_str(&format!(
            "[#{} {} conv={} lane={} origin={}{}{}]\n",
            hit.id,
            hit.created_local,
            root_conv,
            hit.lane,
            hit.origin.as_str(),
            profile_part,
            widgets_part
        ));
        let (req, ans) = if full {
            (
                truncate_chars(&hit.request, max_chars),
                truncate_chars(&hit.answer, max_chars),
            )
        } else {
            (hit.req_snippet.clone(), hit.ans_snippet.clone())
        };
        if plain {
            body.push_str(&format!("Q: {req}\nA: {ans}\n"));
        } else {
            body.push_str(&format!(
                "Q: {}\nA: {}\n",
                sanitize_recall_text(&req),
                sanitize_recall_text(&ans)
            ));
        }
    }

    if plain {
        body.trim_end().to_string()
    } else {
        format!("<untrusted_recall>\n{}</untrusted_recall>", body)
    }
}

/// Load a single turn (`full_thread = false`) or its full chronological
/// conversation thread (`full_thread = true`) and format it wrapped in
/// `<untrusted_recall>` with [`sanitize_recall_text`] unless `plain` is set.
pub fn format_recall_thread(
    state_dir: &Path,
    turn_id: u64,
    full_thread: bool,
    max_chars: usize,
    plain: bool,
) -> Result<Option<String>> {
    let turns = if full_thread {
        let mut thread = conversation_turns(state_dir, turn_id, true, 0)?;
        if thread.is_empty()
            && let Some(single) = get_turn(state_dir, turn_id)?
        {
            thread.push(single);
        }
        thread
    } else {
        match get_turn(state_dir, turn_id)? {
            Some(turn) => vec![turn],
            None => Vec::new(),
        }
    };
    if turns.is_empty() {
        return Ok(None);
    }

    let mut body = String::new();
    for (idx, turn) in turns.iter().enumerate() {
        if idx > 0 {
            body.push_str("\n---\n");
        }
        let root_conv = turn.conversation_id.unwrap_or(turn.id);
        let profile_part = if turn.profile.is_empty() {
            String::new()
        } else {
            format!(" profile={}", turn.profile)
        };
        let widgets_part = if !turn.widgets.is_empty() {
            format!(" widgets={}", turn.widgets.len())
        } else {
            String::new()
        };
        let status_part = if turn.status != TurnStatus::Completed {
            format!(" status={}", turn.status)
        } else {
            String::new()
        };
        body.push_str(&format!(
            "[#{} {} conv={} lane={} origin={}{}{}{}]\n",
            turn.id,
            format_local_timestamp(turn.created_at_ms),
            root_conv,
            turn.lane,
            turn.origin.as_str(),
            profile_part,
            widgets_part,
            status_part
        ));
        let req = if max_chars > 0 {
            truncate_chars(&turn.request, max_chars)
        } else {
            turn.request.clone()
        };
        let ans_raw = if !turn.answer.trim().is_empty() {
            turn.answer.clone()
        } else if !turn.widgets.is_empty() {
            summarize_widgets(&turn.widgets)
        } else if let Some(err) = &turn.error_message {
            format!("[error: {err}]")
        } else if let Some(tok) = &turn.suppressed_token {
            format!("[suppressed: {tok}]")
        } else {
            String::new()
        };
        let ans = if max_chars > 0 {
            truncate_chars(&ans_raw, max_chars)
        } else {
            ans_raw
        };
        if plain {
            body.push_str(&format!("Q: {req}\nA: {ans}\n"));
        } else {
            body.push_str(&format!(
                "Q: {}\nA: {}\n",
                sanitize_recall_text(&req),
                sanitize_recall_text(&ans)
            ));
        }
    }

    if plain {
        Ok(Some(body.trim_end().to_string()))
    } else {
        Ok(Some(format!(
            "<untrusted_recall>\n{body}</untrusted_recall>"
        )))
    }
}

/// Options for [`history_feed`].
#[derive(Debug, Clone)]
pub struct HistoryFeedOptions {
    pub query: Option<String>,
    pub lane: Option<Lane>,
    pub since_ms: Option<i64>,
    pub limit: usize,
    pub no_threads: bool,
    pub include_failed: bool,
    pub include_suppressed: bool,
}

impl Default for HistoryFeedOptions {
    fn default() -> Self {
        Self {
            query: None,
            lane: None,
            since_ms: None,
            limit: 60,
            no_threads: false,
            include_failed: false,
            include_suppressed: false,
        }
    }
}

/// Compact representation of an active or queued turn for Rofi / UI feeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveTaskItem {
    pub id: u64,
    pub lane: Lane,
    pub status: TurnStatus,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile: String,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    pub elapsed_secs: u64,
    pub title: String,
    pub request: String,
    pub origin: TurnOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_name: Option<String>,
}

/// Compact single-line sanitized past conversation or turn entry for Rofi / UI feeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryItem {
    pub id: u64,
    pub conversation_id: u64,
    pub turn_count: usize,
    pub lane: Lane,
    pub status: TurnStatus,
    pub origin: TurnOrigin,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_name: Option<String>,
    pub created_at_ms: i64,
    pub created_local: String,
    pub created_date: String,
    pub relative_time: String,
    pub title: String,
    pub request_preview: String,
    pub answer_preview: String,
    pub widget_count: usize,
    pub has_answer: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_preview: Option<String>,
}

/// Single-shot read-only SQLite feed returned by `hey-computer history --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryFeed {
    pub now_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    pub active_tasks: Vec<ActiveTaskItem>,
    pub items: Vec<HistoryItem>,
}

/// Collapse whitespace, strip control/bidi chars, and truncate to `max` chars
/// for safe single-line Rofi / terminal display (never emits `\0` or `\x1f`).
pub fn compact_single_line(raw: &str, max: usize) -> String {
    let mut cleaned = String::with_capacity(raw.len().min(max * 4));
    let mut prev_space = true;
    for ch in raw.chars() {
        if is_bidi_or_bad_control(ch) || ch == '\x1f' {
            continue;
        }
        if ch.is_whitespace() {
            if !prev_space {
                cleaned.push(' ');
                prev_space = true;
            }
        } else {
            cleaned.push(ch);
            prev_space = false;
        }
    }
    truncate_chars(cleaned.trim(), max)
}

fn extract_markdown_heading(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let hashes = trimmed.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with([' ', '\t']) {
            let heading = trimmed[hashes..].trim().trim_end_matches('#').trim();
            if !heading.is_empty() {
                return Some(compact_single_line(heading, 84));
            }
        }
        break;
    }
    None
}

fn strip_collector_output_block(text: &str) -> &str {
    match text.find("<untrusted_collector_output>") {
        Some(idx) => text[..idx].trim_end(),
        None => text,
    }
}

fn derive_history_title(
    origin: TurnOrigin,
    schedule_name: Option<&str>,
    root_request: &str,
    answer: &str,
    widgets: &[WidgetSpec],
) -> String {
    if let Some(sched) = schedule_name {
        if let Some(heading) = extract_markdown_heading(answer) {
            return heading;
        }
        return crate::scheduler::humanize_schedule_name(sched);
    }
    if origin == TurnOrigin::Distill {
        if let Some(heading) = extract_markdown_heading(answer) {
            return heading;
        }
        return "Memory & Skill Distillation".to_string();
    }
    if crate::notify_filter::is_notification_enrichment_prompt(root_request) {
        return crate::notify_filter::format_notification_title(root_request, 84)
            .unwrap_or_else(|| "Notification Draft".to_string());
    }
    if matches!(origin, TurnOrigin::Schedule | TurnOrigin::ScheduleScript)
        && let Some(heading) = extract_markdown_heading(answer)
    {
        return heading;
    }
    let req = compact_single_line(strip_collector_output_block(root_request), 84);
    if !req.is_empty() {
        return req;
    }
    if let Some(heading) = extract_markdown_heading(answer) {
        return heading;
    }
    if let Some(w) = widgets.first()
        && !w.title.trim().is_empty()
    {
        return compact_single_line(&w.title, 84);
    }
    "(untitled)".to_string()
}

fn derive_answer_preview(
    status: TurnStatus,
    answer: &str,
    widgets: &[WidgetSpec],
    error_message: Option<&str>,
    fail_reason: Option<&str>,
    suppressed_token: Option<&str>,
) -> String {
    if !answer.trim().is_empty() {
        // Strip leading `#` markers from first heading line so previews read cleanly.
        let mut lines = Vec::new();
        for line in answer.lines().take(6) {
            let t = line.trim();
            if t.is_empty() || t.starts_with("```") || t.starts_with("~~~") {
                continue;
            }
            let stripped = t.trim_start_matches('#').trim();
            if !stripped.is_empty() {
                lines.push(stripped);
            }
        }
        return compact_single_line(&lines.join(" "), 180);
    }
    if !widgets.is_empty() {
        return compact_single_line(&summarize_widgets(widgets), 180);
    }
    match status {
        TurnStatus::Failed | TurnStatus::Cancelled => {
            let detail = error_message.or(fail_reason).unwrap_or(status.as_str());
            compact_single_line(detail, 140)
        }
        TurnStatus::Suppressed => compact_single_line(
            &format!(
                "suppressed ({})",
                suppressed_token.unwrap_or("HEARTBEAT_OK")
            ),
            140,
        ),
        _ => String::new(),
    }
}

/// Fast single-shot read-only SQLite feed of active background/interactive tasks
/// plus chronological (thread-collapsed) or FTS5-matched past history items.
pub fn history_feed(state_dir: &Path, opts: &HistoryFeedOptions) -> Result<HistoryFeed> {
    let now_ms = unix_timestamp_ms();
    let raw_query = opts
        .query
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(str::to_string);

    let Some(conn) = open_reader_connection(state_dir)? else {
        return Ok(HistoryFeed {
            now_ms,
            query: raw_query,
            active_tasks: Vec::new(),
            items: Vec::new(),
        });
    };

    let has_sched = has_schedules_table(&conn)?;
    let sched_join = if has_sched {
        "LEFT JOIN schedules s ON s.id = t.schedule_id"
    } else {
        ""
    };
    let sched_col = if has_sched { "s.name" } else { "NULL" };

    // 1. Active & queued tasks.
    let mut active_tasks = Vec::new();
    {
        let sql = format!(
            "SELECT t.id, t.lane, t.status, t.profile, t.created_at, t.started_at,
                    t.request, t.origin, t.schedule_id, {sched_col}
             FROM turns t
             {sched_join}
             WHERE t.status IN ('queued', 'running')
             ORDER BY CASE WHEN t.status = 'running' THEN 0 ELSE 1 END ASC, t.id ASC
             LIMIT 20"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            let lane_str: String = row.get(1)?;
            let status_str: String = row.get(2)?;
            let profile: String = row.get(3)?;
            let created_at_ms: i64 = row.get(4)?;
            let started_at_ms: Option<i64> = row.get(5)?;
            let request: String = row.get(6)?;
            let origin_str: String = row.get(7)?;
            let schedule_id: Option<i64> = row.get(8)?;
            let schedule_name: Option<String> = row.get(9)?;
            let origin = TurnOrigin::parse(&origin_str);
            let ref_ms = started_at_ms.unwrap_or(created_at_ms);
            let elapsed_secs = ((now_ms - ref_ms).max(0) / 1000) as u64;
            let is_notif = crate::notify_filter::is_notification_enrichment_prompt(&request);
            let title = if let Some(ref sname) = schedule_name {
                crate::scheduler::humanize_schedule_name(sname)
            } else if is_notif {
                crate::notify_filter::format_notification_title(&request, 84)
                    .unwrap_or_else(|| "Notification Draft".to_string())
            } else {
                compact_single_line(strip_collector_output_block(&request), 84)
            };
            let req_clean = if is_notif {
                crate::notify_filter::format_notification_preview(&request, 240)
                    .unwrap_or_else(|| title.clone())
            } else {
                compact_single_line(strip_collector_output_block(&request), 240)
            };
            Ok(ActiveTaskItem {
                id: id.max(0) as u64,
                lane: Lane::parse(&lane_str).unwrap_or(Lane::Background),
                status: TurnStatus::parse(&status_str).unwrap_or(TurnStatus::Running),
                profile,
                created_at_ms,
                started_at_ms,
                elapsed_secs,
                title,
                request: req_clean,
                origin,
                schedule_id: schedule_id.map(|v| v.max(0) as u64),
                schedule_name,
            })
        })?;
        for row in rows {
            active_tasks.push(row?);
        }
    }

    let limit = opts.limit.clamp(1, 200) as i64;
    let lane_param = opts.lane.map(Lane::as_str);

    // 2. History items: FTS5 search when query is provided, else chronological (optionally thread-collapsed).
    let mut items = Vec::new();
    if let Some(ref q) = raw_query {
        if let Some(fts_query) = build_fts_query(q, false) {
            let sql = format!(
                "SELECT t.id, COALESCE(t.conversation_id, t.id) AS conv_id, 1 AS turn_count,
                        t.lane, t.status, t.origin, t.profile, t.schedule_id, {sched_col},
                        t.created_at, t.request, t.request AS root_request, t.answer,
                        t.widgets_json, t.error_message, t.fail_reason, t.suppressed_token,
                        bm25(turns_fts, 1.5, 1.0)
                          / (1.0 + (MAX(0, ?1 - t.created_at) / (86400000.0 * 30.0))) AS score
                 FROM turns_fts
                 JOIN turns t ON t.id = turns_fts.rowid
                 {sched_join}
                 WHERE turns_fts MATCH ?2
                   AND t.status = 'completed'
                   AND length(trim(t.answer)) > 0
                   AND (?3 IS NULL OR t.lane = ?3)
                   AND (?4 IS NULL OR t.created_at >= ?4)
                 ORDER BY score ASC, t.created_at DESC, t.id DESC
                 LIMIT ?5"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(
                    params![now_ms, fts_query, lane_param, opts.since_ms, limit],
                    |row| row_to_history_item(row, now_ms),
                )
                .context("running FTS5 history feed query")?;
            for row in rows {
                items.push(row?);
            }
        }
    } else {
        let status_clause = if opts.include_suppressed {
            "((t.status = 'completed' AND (length(trim(t.answer)) > 0 OR t.widgets_json != '[]')) \
              OR t.status IN ('failed', 'cancelled', 'suppressed'))"
        } else if opts.include_failed {
            "((t.status = 'completed' AND (length(trim(t.answer)) > 0 OR t.widgets_json != '[]')) \
              OR t.status IN ('failed', 'cancelled'))"
        } else {
            "(t.status = 'completed' AND (length(trim(t.answer)) > 0 OR t.widgets_json != '[]'))"
        };

        let sql = if opts.no_threads {
            format!(
                "SELECT t.id, COALESCE(t.conversation_id, t.id) AS conv_id, 1 AS turn_count,
                        t.lane, t.status, t.origin, t.profile, t.schedule_id, {sched_col},
                        t.created_at, t.request, t.request AS root_request, t.answer,
                        t.widgets_json, t.error_message, t.fail_reason, t.suppressed_token
                 FROM turns t
                 {sched_join}
                 WHERE {status_clause}
                   AND (?1 IS NULL OR t.lane = ?1)
                   AND (?2 IS NULL OR t.created_at >= ?2)
                 ORDER BY t.created_at DESC, t.id DESC
                 LIMIT ?3"
            )
        } else {
            format!(
                "WITH filtered AS (
                     SELECT t.id, COALESCE(t.conversation_id, t.id) AS conv_id,
                            t.lane, t.status, t.origin, t.profile, t.schedule_id,
                            {sched_col} AS schedule_name,
                            t.created_at, t.request, t.answer, t.widgets_json,
                            t.error_message, t.fail_reason, t.suppressed_token,
                            ROW_NUMBER() OVER (
                                PARTITION BY COALESCE(t.conversation_id, t.id)
                                ORDER BY t.id DESC
                            ) AS rn,
                            COUNT(*) OVER (
                                PARTITION BY COALESCE(t.conversation_id, t.id)
                            ) AS turn_count,
                            FIRST_VALUE(t.request) OVER (
                                PARTITION BY COALESCE(t.conversation_id, t.id)
                                ORDER BY t.id ASC
                            ) AS root_request
                     FROM turns t
                     {sched_join}
                     WHERE {status_clause}
                       AND (?1 IS NULL OR t.lane = ?1)
                       AND (?2 IS NULL OR t.created_at >= ?2)
                 )
                 SELECT id, conv_id, turn_count, lane, status, origin, profile,
                        schedule_id, schedule_name, created_at, request, root_request,
                        answer, widgets_json, error_message, fail_reason, suppressed_token
                 FROM filtered
                 WHERE rn = 1
                 ORDER BY created_at DESC, id DESC
                 LIMIT ?3"
            )
        };
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![lane_param, opts.since_ms, limit], |row| {
            row_to_history_item(row, now_ms)
        })?;
        for row in rows {
            items.push(row?);
        }
    }

    Ok(HistoryFeed {
        now_ms,
        query: raw_query,
        active_tasks,
        items,
    })
}

fn row_to_history_item(row: &rusqlite::Row<'_>, now_ms: i64) -> rusqlite::Result<HistoryItem> {
    let id: i64 = row.get(0)?;
    let conv_id: i64 = row.get(1)?;
    let turn_count: i64 = row.get(2)?;
    let lane_str: String = row.get(3)?;
    let status_str: String = row.get(4)?;
    let origin_str: String = row.get(5)?;
    let profile: String = row.get(6)?;
    let schedule_id: Option<i64> = row.get(7)?;
    let schedule_name: Option<String> = row.get(8)?;
    let created_at_ms: i64 = row.get(9)?;
    let request: String = row.get(10)?;
    let root_request: String = row.get(11)?;
    let answer: String = row.get(12)?;
    let widgets_json: String = row.get(13)?;
    let error_message: Option<String> = row.get(14)?;
    let fail_reason: Option<String> = row.get(15)?;
    let suppressed_token: Option<String> = row.get(16)?;

    let status = TurnStatus::parse(&status_str).unwrap_or(TurnStatus::Failed);
    let origin = TurnOrigin::parse(&origin_str);
    let widgets: Vec<WidgetSpec> = serde_json::from_str(&widgets_json).unwrap_or_default();
    let widget_count = widgets.len();
    let created_local = format_local_timestamp(created_at_ms);
    let created_date = created_local
        .get(..10)
        .unwrap_or(&created_local)
        .to_string();
    let title = derive_history_title(
        origin,
        schedule_name.as_deref(),
        &root_request,
        &answer,
        &widgets,
    );
    let request_preview = if crate::notify_filter::is_notification_enrichment_prompt(&request) {
        crate::notify_filter::format_notification_preview(&request, 140)
            .unwrap_or_else(|| title.clone())
    } else if schedule_id.is_some()
        || matches!(origin, TurnOrigin::Schedule | TurnOrigin::ScheduleScript)
    {
        compact_single_line(strip_collector_output_block(&request), 140)
    } else {
        compact_single_line(&request, 140)
    };
    let answer_preview = derive_answer_preview(
        status,
        &answer,
        &widgets,
        error_message.as_deref(),
        fail_reason.as_deref(),
        suppressed_token.as_deref(),
    );
    let has_answer = !answer.trim().is_empty() || widget_count > 0;
    let error_preview = if matches!(status, TurnStatus::Failed | TurnStatus::Cancelled) {
        Some(compact_single_line(
            error_message
                .as_deref()
                .or(fail_reason.as_deref())
                .unwrap_or(status.as_str()),
            140,
        ))
    } else {
        None
    };

    Ok(HistoryItem {
        id: id.max(0) as u64,
        conversation_id: conv_id.max(0) as u64,
        turn_count: turn_count.max(1) as usize,
        lane: Lane::parse(&lane_str).unwrap_or(Lane::Interactive),
        status,
        origin,
        profile,
        schedule_id: schedule_id.map(|v| v.max(0) as u64),
        schedule_name,
        created_at_ms,
        created_local,
        created_date,
        relative_time: format_relative_time(created_at_ms, now_ms),
        title,
        request_preview,
        answer_preview,
        widget_count,
        has_answer,
        error_preview,
    })
}

/// How scheduled job failures are delivered.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureDeliver {
    #[default]
    Notify,
    Log,
    Silent,
}

impl FailureDeliver {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Notify => "notify",
            Self::Log => "log",
            Self::Silent => "silent",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "notify" => Some(Self::Notify),
            "log" => Some(Self::Log),
            "silent" => Some(Self::Silent),
            _ => None,
        }
    }
}

impl fmt::Display for FailureDeliver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One row from the `schedules` table (schema v3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleRecord {
    pub id: u64,
    pub name: String,
    pub enabled: bool,
    #[serde(default)]
    pub managed: bool,
    pub schedule_spec: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub script: String,
    pub no_agent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppress_token: Option<String>,
    pub failure_deliver: FailureDeliver,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_runs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    pub run_count: u64,
    pub consecutive_failures: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Input fields for creating a scheduled job.
#[derive(Debug, Clone)]
pub struct NewSchedule {
    pub name: String,
    pub schedule_spec: String,
    pub prompt: String,
    pub profile: String,
    pub script: String,
    pub no_agent: bool,
    pub suppress_token: Option<String>,
    pub failure_deliver: FailureDeliver,
    pub max_runs: Option<u64>,
    pub expires_at: Option<i64>,
    pub next_run_at: i64,
}

/// Patch fields for editing an existing scheduled job.
#[derive(Debug, Default, Clone)]
pub struct SchedulePatch {
    pub schedule_spec: Option<String>,
    pub next_run_at: Option<Option<i64>>,
    pub prompt: Option<String>,
    pub profile: Option<String>,
    pub script: Option<String>,
    pub no_agent: Option<bool>,
    pub suppress_token: Option<Option<String>>,
    pub failure_deliver: Option<FailureDeliver>,
    pub max_runs: Option<Option<u64>>,
    pub expires_at: Option<Option<i64>>,
}

const SCHEDULE_COLUMNS: &str = "id, name, enabled, schedule_spec, prompt, profile, script, \
    no_agent, suppress_token, failure_deliver, max_runs, expires_at, run_count, \
    consecutive_failures, next_run_at, last_run_at, last_status, last_error, \
    created_at, updated_at, managed";

fn row_to_schedule(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScheduleRecord> {
    let id: i64 = row.get(0)?;
    let name: String = row.get(1)?;
    let enabled: i64 = row.get(2)?;
    let schedule_spec: String = row.get(3)?;
    let prompt: String = row.get(4)?;
    let profile: String = row.get(5)?;
    let script: String = row.get(6)?;
    let no_agent: i64 = row.get(7)?;
    let suppress_token: Option<String> = row.get(8)?;
    let failure_deliver_str: String = row.get(9)?;
    let max_runs: Option<i64> = row.get(10)?;
    let expires_at: Option<i64> = row.get(11)?;
    let run_count: i64 = row.get(12)?;
    let consecutive_failures: i64 = row.get(13)?;
    let next_run_at: Option<i64> = row.get(14)?;
    let last_run_at: Option<i64> = row.get(15)?;
    let last_status: Option<String> = row.get(16)?;
    let last_error: Option<String> = row.get(17)?;
    let created_at: i64 = row.get(18)?;
    let updated_at: i64 = row.get(19)?;
    let managed: i64 = row.get(20).unwrap_or(0);
    Ok(ScheduleRecord {
        id: id.max(0) as u64,
        name,
        enabled: enabled != 0,
        managed: managed != 0,
        schedule_spec,
        prompt,
        profile,
        script,
        no_agent: no_agent != 0,
        suppress_token,
        failure_deliver: FailureDeliver::parse(&failure_deliver_str).unwrap_or_default(),
        max_runs: max_runs.map(|v| v.max(0) as u64),
        expires_at,
        run_count: run_count.max(0) as u64,
        consecutive_failures: consecutive_failures.max(0) as u64,
        next_run_at,
        last_run_at,
        last_status,
        last_error,
        created_at,
        updated_at,
    })
}

fn has_schedules_table(conn: &Connection) -> Result<bool> {
    let version: i32 = conn.query_row("PRAGMA user_version;", [], |r| r.get(0))?;
    Ok(version >= 2)
}

fn open_migrated_writer(state_dir: &Path) -> Result<Connection> {
    let mut conn = open_writer_connection(state_dir)?;
    migrate(&mut conn)?;
    tighten_db_permissions(state_dir);
    Ok(conn)
}

fn lookup_schedule(conn: &Connection, id_or_name: &str) -> Result<Option<ScheduleRecord>> {
    if let Ok(id) = id_or_name.parse::<u64>()
        && let Some(row) = conn
            .query_row(
                &format!("SELECT {SCHEDULE_COLUMNS} FROM schedules WHERE id = ?1"),
                params![id as i64],
                row_to_schedule,
            )
            .optional()?
    {
        return Ok(Some(row));
    }
    let row = conn
        .query_row(
            &format!("SELECT {SCHEDULE_COLUMNS} FROM schedules WHERE name = ?1"),
            params![id_or_name],
            row_to_schedule,
        )
        .optional()?;
    Ok(row)
}

/// List scheduled jobs directly from `history.db` (read-only; works when daemon is stopped).
pub fn list_schedules(state_dir: &Path, include_disabled: bool) -> Result<Vec<ScheduleRecord>> {
    let Some(conn) = open_reader_connection(state_dir)? else {
        return Ok(Vec::new());
    };
    if !has_schedules_table(&conn)? {
        return Ok(Vec::new());
    }
    let sql = if include_disabled {
        format!("SELECT {SCHEDULE_COLUMNS} FROM schedules ORDER BY enabled DESC, id ASC")
    } else {
        format!(
            "SELECT {SCHEDULE_COLUMNS} FROM schedules WHERE enabled = 1 ORDER BY next_run_at ASC, id ASC"
        )
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_to_schedule)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Fetch one scheduled job by numeric ID or exact name (read-only).
pub fn get_schedule(state_dir: &Path, id_or_name: &str) -> Result<Option<ScheduleRecord>> {
    let Some(conn) = open_reader_connection(state_dir)? else {
        return Ok(None);
    };
    if !has_schedules_table(&conn)? {
        return Ok(None);
    }
    lookup_schedule(&conn, id_or_name)
}

/// Insert a new scheduled job, enforcing `max_jobs`. If a non-managed disabled
/// one-shot or expired job with the same `name` already exists, reuses and
/// reactivates that row cleanly so recurring voice reminders do not collide on
/// `UNIQUE(name)`.
pub fn create_schedule(
    state_dir: &Path,
    spec: &NewSchedule,
    max_jobs: usize,
) -> Result<ScheduleRecord> {
    let conn = open_migrated_writer(state_dir)?;
    let now_ms = unix_timestamp_ms();
    if let Some(existing) = lookup_schedule(&conn, &spec.name)? {
        let existing_one_shot = crate::scheduler::ScheduleSpec::parse(&existing.schedule_spec)
            .map(|s| s.is_one_shot())
            .unwrap_or(false);
        let replaceable = !existing.managed
            && !existing.enabled
            && (existing_one_shot || existing.last_status.as_deref() == Some("expired"));
        if replaceable {
            conn.execute(
                "UPDATE schedules
                 SET enabled = 1,
                     schedule_spec = ?2,
                     prompt = ?3,
                     profile = ?4,
                     script = ?5,
                     no_agent = ?6,
                     suppress_token = ?7,
                     failure_deliver = ?8,
                     max_runs = ?9,
                     expires_at = ?10,
                     run_count = 0,
                     consecutive_failures = 0,
                     next_run_at = ?11,
                     last_run_at = NULL,
                     last_status = NULL,
                     last_error = NULL,
                     updated_at = ?12
                 WHERE id = ?1",
                params![
                    existing.id as i64,
                    spec.schedule_spec,
                    spec.prompt,
                    spec.profile,
                    spec.script,
                    i64::from(spec.no_agent),
                    spec.suppress_token,
                    spec.failure_deliver.as_str(),
                    spec.max_runs.map(|v| v as i64),
                    spec.expires_at,
                    spec.next_run_at,
                    now_ms,
                ],
            )?;
            return lookup_schedule(&conn, &existing.id.to_string())?
                .context("reactivated schedule could not be read back");
        }
    }

    let active_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM schedules WHERE enabled = 1 OR managed = 1",
        [],
        |r| r.get(0),
    )?;
    if active_count as usize >= max_jobs {
        bail!("maximum number of scheduled jobs reached (scheduler.max_jobs = {max_jobs})");
    }
    let total_count: i64 = conn.query_row("SELECT COUNT(*) FROM schedules", [], |r| r.get(0))?;
    if total_count as usize >= max_jobs {
        let _ = conn.execute(
            "DELETE FROM schedules WHERE id IN (
                 SELECT id FROM schedules
                 WHERE managed = 0 AND enabled = 0
                 ORDER BY updated_at ASC
                 LIMIT ?1
             )",
            params![(total_count as usize + 1 - max_jobs) as i64],
        );
    }
    conn.execute(
        "INSERT INTO schedules (
            name, enabled, managed, schedule_spec, prompt, profile, script, no_agent,
            suppress_token, failure_deliver, max_runs, expires_at, next_run_at,
            created_at, updated_at
        ) VALUES (?1, 1, 0, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
        params![
            spec.name,
            spec.schedule_spec,
            spec.prompt,
            spec.profile,
            spec.script,
            i64::from(spec.no_agent),
            spec.suppress_token,
            spec.failure_deliver.as_str(),
            spec.max_runs.map(|v| v as i64),
            spec.expires_at,
            spec.next_run_at,
            now_ms,
        ],
    )
    .with_context(|| format!("creating schedule `{}`", spec.name))?;
    let id = conn.last_insert_rowid() as u64;
    lookup_schedule(&conn, &id.to_string())?.context("created schedule could not be read back")
}

/// Reconcile declarative `[scheduler.jobs.<name>]` definitions into `history.db`
/// (`managed = 1`), preserving `run_count` and `last_run_at` across daemon
/// restarts and removing stale `managed = 1` entries no longer in `config.toml`.
pub fn sync_managed_schedules(
    state_dir: &Path,
    jobs: &std::collections::BTreeMap<String, crate::config::ScheduledJobConfig>,
    now_ms: i64,
) -> Result<()> {
    if jobs.is_empty() {
        let Some(reader) = open_reader_connection(state_dir)? else {
            return Ok(());
        };
        if !has_schedules_table(&reader)? {
            return Ok(());
        }
        let version: i32 = reader.query_row("PRAGMA user_version;", [], |r| r.get(0))?;
        if version < 3 {
            return Ok(());
        }
        let has_managed: bool = reader.query_row(
            "SELECT EXISTS(SELECT 1 FROM schedules WHERE managed = 1)",
            [],
            |r| r.get(0),
        )?;
        drop(reader);
        if !has_managed {
            return Ok(());
        }
    }

    let mut conn = open_migrated_writer(state_dir)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let existing_managed: Vec<String> = {
        let mut stmt = tx.prepare("SELECT name FROM schedules WHERE managed = 1")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        out
    };
    for old_name in existing_managed {
        if !jobs.contains_key(&old_name) {
            tx.execute(
                "DELETE FROM schedules WHERE managed = 1 AND name = ?1",
                params![old_name],
            )?;
        }
    }

    for (name, cfg) in jobs {
        let spec_trimmed = cfg.schedule.trim();
        let parsed = crate::scheduler::ScheduleSpec::parse(spec_trimmed)?;
        let fresh_next = if cfg.enabled {
            Some(parsed.first_run_after(now_ms)?)
        } else {
            None
        };
        let profile = if cfg.profile == "default" {
            ""
        } else {
            cfg.profile.as_str()
        };
        let existing = lookup_schedule(&tx, name)?;
        match existing {
            Some(cur) => {
                let keep_next = cfg.enabled
                    && cur.enabled
                    && cur.schedule_spec == spec_trimmed
                    && cur.next_run_at.is_some_and(|t| t > now_ms);
                let next_run_at = if keep_next {
                    cur.next_run_at
                } else {
                    fresh_next
                };
                tx.execute(
                    "UPDATE schedules
                     SET enabled = ?2,
                         managed = 1,
                         schedule_spec = ?3,
                         prompt = ?4,
                         profile = ?5,
                         script = ?6,
                         no_agent = ?7,
                         suppress_token = ?8,
                         failure_deliver = ?9,
                         max_runs = NULL,
                         expires_at = NULL,
                         next_run_at = ?10,
                         updated_at = ?11
                     WHERE id = ?1",
                    params![
                        cur.id as i64,
                        i64::from(cfg.enabled),
                        spec_trimmed,
                        cfg.prompt,
                        profile,
                        cfg.script,
                        i64::from(cfg.no_agent),
                        cfg.suppress_token,
                        cfg.failure_deliver.as_str(),
                        next_run_at,
                        now_ms,
                    ],
                )?;
            }
            None => {
                tx.execute(
                    "INSERT INTO schedules (
                        name, enabled, managed, schedule_spec, prompt, profile, script,
                        no_agent, suppress_token, failure_deliver, next_run_at,
                        created_at, updated_at
                    ) VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
                    params![
                        name,
                        i64::from(cfg.enabled),
                        spec_trimmed,
                        cfg.prompt,
                        profile,
                        cfg.script,
                        i64::from(cfg.no_agent),
                        cfg.suppress_token,
                        cfg.failure_deliver.as_str(),
                        fresh_next,
                        now_ms,
                    ],
                )?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// Apply partial edits to a scheduled job.
pub fn update_schedule(
    state_dir: &Path,
    id_or_name: &str,
    patch: &SchedulePatch,
) -> Result<ScheduleRecord> {
    let conn = open_migrated_writer(state_dir)?;
    let Some(mut job) = lookup_schedule(&conn, id_or_name)? else {
        bail!("schedule `{id_or_name}` not found");
    };
    if let Some(s) = &patch.schedule_spec {
        job.schedule_spec = s.clone();
    }
    if let Some(next) = patch.next_run_at {
        job.next_run_at = next;
    }
    if let Some(p) = &patch.prompt {
        job.prompt = p.clone();
    }
    if let Some(p) = &patch.profile {
        job.profile = p.clone();
    }
    if let Some(s) = &patch.script {
        job.script = s.clone();
    }
    if let Some(n) = patch.no_agent {
        job.no_agent = n;
    }
    if let Some(tok) = &patch.suppress_token {
        job.suppress_token = tok.clone();
    }
    if let Some(fd) = patch.failure_deliver {
        job.failure_deliver = fd;
    }
    if let Some(mr) = patch.max_runs {
        job.max_runs = mr;
    }
    if let Some(exp) = patch.expires_at {
        job.expires_at = exp;
    }
    if job.prompt.trim().is_empty() && job.script.trim().is_empty() {
        bail!("schedule must keep a non-empty --prompt, --script, or both");
    }
    let now_ms = unix_timestamp_ms();
    conn.execute(
        "UPDATE schedules
         SET schedule_spec = ?2,
             prompt = ?3,
             profile = ?4,
             script = ?5,
             no_agent = ?6,
             suppress_token = ?7,
             failure_deliver = ?8,
             max_runs = ?9,
             expires_at = ?10,
             next_run_at = ?11,
             updated_at = ?12
         WHERE id = ?1",
        params![
            job.id as i64,
            job.schedule_spec,
            job.prompt,
            job.profile,
            job.script,
            i64::from(job.no_agent),
            job.suppress_token,
            job.failure_deliver.as_str(),
            job.max_runs.map(|v| v as i64),
            job.expires_at,
            job.next_run_at,
            now_ms,
        ],
    )?;
    lookup_schedule(&conn, &job.id.to_string())?.context("updated schedule could not be read back")
}

/// Enable (resume) or disable (pause) a scheduled job.
pub fn set_schedule_enabled(
    state_dir: &Path,
    id_or_name: &str,
    enabled: bool,
    next_run_at: Option<i64>,
) -> Result<ScheduleRecord> {
    let conn = open_migrated_writer(state_dir)?;
    let Some(job) = lookup_schedule(&conn, id_or_name)? else {
        bail!("schedule `{id_or_name}` not found");
    };
    if job.prompt.trim().is_empty() && job.script.trim().is_empty() {
        bail!("schedule must keep a non-empty --prompt, --script, or both");
    }
    let now_ms = unix_timestamp_ms();
    conn.execute(
        "UPDATE schedules
         SET enabled = ?2,
             next_run_at = ?3,
             consecutive_failures = CASE WHEN ?2 = 1 THEN 0 ELSE consecutive_failures END,
             updated_at = ?4
         WHERE id = ?1",
        params![job.id as i64, i64::from(enabled), next_run_at, now_ms],
    )?;
    lookup_schedule(&conn, &job.id.to_string())?.context("updated schedule could not be read back")
}

/// Delete a scheduled job by numeric ID or exact name.
pub fn delete_schedule(state_dir: &Path, id_or_name: &str) -> Result<Option<ScheduleRecord>> {
    let conn = open_migrated_writer(state_dir)?;
    let Some(job) = lookup_schedule(&conn, id_or_name)? else {
        return Ok(None);
    };
    if job.managed {
        bail!(
            "schedule `{}` is managed by config.toml (use `schedule pause` or remove it from config)",
            job.name
        );
    }
    conn.execute(
        "DELETE FROM schedules WHERE id = ?1",
        params![job.id as i64],
    )?;
    Ok(Some(job))
}

/// Return all enabled schedules whose `next_run_at <= now_ms`.
pub fn due_schedules(state_dir: &Path, now_ms: i64) -> Result<Vec<ScheduleRecord>> {
    let Some(conn) = open_reader_connection(state_dir)? else {
        return Ok(Vec::new());
    };
    if !has_schedules_table(&conn)? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT {SCHEDULE_COLUMNS}
         FROM schedules
         WHERE enabled = 1 AND next_run_at IS NOT NULL AND next_run_at <= ?1
         ORDER BY next_run_at ASC, id ASC"
    ))?;
    let rows = stmt.query_map(params![now_ms], row_to_schedule)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Earliest `next_run_at` among enabled jobs, for sleeping efficiently.
pub fn next_schedule_wake_ms(state_dir: &Path) -> Result<Option<i64>> {
    let Some(conn) = open_reader_connection(state_dir)? else {
        return Ok(None);
    };
    if !has_schedules_table(&conn)? {
        return Ok(None);
    }
    let next: Option<i64> = conn.query_row(
        "SELECT MIN(next_run_at) FROM schedules WHERE enabled = 1 AND next_run_at IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    Ok(next)
}

pub fn advance_schedule_next_run(
    state_dir: &Path,
    id: u64,
    next_run_at: Option<i64>,
    now_ms: i64,
) -> Result<()> {
    let conn = open_migrated_writer(state_dir)?;
    conn.execute(
        "UPDATE schedules SET next_run_at = ?2, updated_at = ?3 WHERE id = ?1",
        params![id as i64, next_run_at, now_ms],
    )?;
    Ok(())
}

pub fn disable_expired_schedule(state_dir: &Path, id: u64, now_ms: i64) -> Result<()> {
    let conn = open_migrated_writer(state_dir)?;
    conn.execute(
        "UPDATE schedules
         SET enabled = 0, next_run_at = NULL, last_status = 'expired', updated_at = ?2
         WHERE id = ?1",
        params![id as i64, now_ms],
    )?;
    Ok(())
}

/// Disable any enabled schedule whose `expires_at <= now_ms`, even when its
/// `next_run_at` is after `expires_at` or `NULL`.
pub fn sweep_expired_schedules(state_dir: &Path, now_ms: i64) -> Result<usize> {
    let Some(reader) = open_reader_connection(state_dir)? else {
        return Ok(0);
    };
    if !has_schedules_table(&reader)? {
        return Ok(0);
    }
    let has_expired: bool = reader.query_row(
        "SELECT EXISTS(SELECT 1 FROM schedules WHERE enabled = 1 AND expires_at IS NOT NULL AND expires_at <= ?1)",
        params![now_ms],
        |r| r.get(0),
    )?;
    drop(reader);
    if !has_expired {
        return Ok(0);
    }
    let conn = open_migrated_writer(state_dir)?;
    let n = conn.execute(
        "UPDATE schedules
         SET enabled = 0, next_run_at = NULL, last_status = 'expired', updated_at = ?1
         WHERE enabled = 1 AND expires_at IS NOT NULL AND expires_at <= ?1",
        params![now_ms],
    )?;
    Ok(n)
}

/// Atomically record a schedule firing outcome in SQLite without clobbering
/// concurrent CLI edits (`max_runs`, `enabled`, `consecutive_failures`).
/// Returns the updated `consecutive_failures` count.
#[allow(clippy::too_many_arguments)]
pub fn record_schedule_outcome(
    state_dir: &Path,
    id: u64,
    last_run_at: i64,
    updated_at: i64,
    last_status: &str,
    last_error: Option<&str>,
    failed: bool,
    manual: bool,
    force_disable: bool,
) -> Result<u64> {
    let conn = open_migrated_writer(state_dir)?;
    let new_failures: i64 = conn
        .query_row(
            "UPDATE schedules
             SET run_count = run_count + 1,
                 consecutive_failures = CASE WHEN ?5 = 1 THEN consecutive_failures + 1 ELSE 0 END,
                 last_run_at = ?2,
                 last_status = ?3,
                 last_error = ?4,
                 enabled = CASE
                     WHEN (?7 = 1 AND (next_run_at IS NULL OR updated_at < ?2))
                       OR (?6 = 0 AND max_runs IS NOT NULL AND run_count + 1 >= max_runs)
                     THEN 0
                     ELSE enabled
                 END,
                 next_run_at = CASE
                     WHEN (?7 = 1 AND (next_run_at IS NULL OR updated_at < ?2))
                       OR (?6 = 0 AND max_runs IS NOT NULL AND run_count + 1 >= max_runs)
                     THEN NULL
                     ELSE next_run_at
                 END,
                 updated_at = ?8
             WHERE id = ?1
             RETURNING consecutive_failures",
            params![
                id as i64,
                last_run_at,
                last_status,
                last_error,
                i64::from(failed),
                i64::from(manual),
                i64::from(force_disable),
                updated_at,
            ],
            |r| r.get(0),
        )
        .unwrap_or(i64::from(failed));
    Ok(new_failures.max(0) as u64)
}

/// Record a collector-skipped schedule firing without clobbering any prior
/// outcome (`last_status`, `last_run_at`, `last_error`, `run_count`, or
/// `consecutive_failures`).
///
/// For a brand-new schedule (`last_status IS NULL`), records `'skipped'`.
/// When `force_disable` is true (e.g. a one-shot schedule with no next run),
/// disables the schedule and clears `next_run_at` without incrementing `run_count`,
/// unless the collector script itself rescheduled the job (`next_run_at IS NOT NULL`
/// and `updated_at >= started_at`).
pub fn record_schedule_skip(
    state_dir: &Path,
    id: u64,
    started_at: i64,
    updated_at: i64,
    force_disable: bool,
) -> Result<()> {
    let conn = open_migrated_writer(state_dir)?;
    conn.execute(
        "UPDATE schedules
         SET last_status = COALESCE(last_status, 'skipped'),
             enabled = CASE
                 WHEN ?4 = 1 AND (next_run_at IS NULL OR updated_at < ?2)
                 THEN 0
                 ELSE enabled
             END,
             next_run_at = CASE
                 WHEN ?4 = 1 AND (next_run_at IS NULL OR updated_at < ?2)
                 THEN NULL
                 ELSE next_run_at
             END,
             updated_at = ?3
         WHERE id = ?1",
        params![id as i64, started_at, updated_at, i64::from(force_disable)],
    )?;
    Ok(())
}

/// Lifecycle state of a persisted stateless notification draft checkpoint (schema v5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftStatus {
    Pending,
    Consumed,
    Dismissed,
    Expired,
}

impl DraftStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Consumed => "consumed",
            Self::Dismissed => "dismissed",
            Self::Expired => "expired",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "consumed" => Some(Self::Consumed),
            "dismissed" => Some(Self::Dismissed),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

impl fmt::Display for DraftStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One row from the `draft_checkpoints` table (schema v5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftCheckpoint {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    pub token: String,
    pub channel: String,
    pub app_name: String,
    pub sender: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub chat_target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_notif_id: Option<u32>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub window_class: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub context_note: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub draft_text: String,
    pub status: DraftStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_action: Option<DraftAction>,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_at_ms: Option<i64>,
}

/// Fields needed to create a new stateless draft checkpoint row.
#[derive(Debug, Clone)]
pub struct NewDraftCheckpoint {
    pub turn_id: Option<u64>,
    pub channel: String,
    pub app_name: String,
    pub sender: String,
    pub chat_target: String,
    pub source_notif_id: Option<u32>,
    pub window_class: String,
    pub context_note: String,
    pub draft_text: String,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
}

/// Outcome of an atomic single-use draft checkpoint claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointClaim {
    /// Transitioned from `pending` -> `consumed`/`dismissed` in this call.
    Claimed(DraftCheckpoint),
    /// Already consumed/dismissed earlier (idempotent replay / double-click).
    AlreadyUsed(DraftCheckpoint),
    /// Past `expires_at_ms` (transitioned to `expired` if still `pending`).
    Expired(DraftCheckpoint),
    /// Unknown token or ID.
    NotFound,
}

const DRAFT_CHECKPOINT_COLUMNS: &str = "id, turn_id, token, channel, app_name, sender, \
    chat_target, source_notif_id, window_class, context_note, draft_text, status, \
    consumed_action, created_at, expires_at, consumed_at";

fn has_draft_checkpoints_table(conn: &Connection) -> Result<bool> {
    let version: i32 = conn.query_row("PRAGMA user_version;", [], |r| r.get(0))?;
    Ok(version >= 5)
}

fn fill_random_bytes(buf: &mut [u8]) -> Result<()> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let rc =
            unsafe { libc::getrandom(buf[filled..].as_mut_ptr().cast(), buf.len() - filled, 0) };
        if rc > 0 {
            filled += rc as usize;
        } else if rc == -1 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        } else {
            break;
        }
    }
    if filled < buf.len() {
        let mut urandom = std::fs::File::open("/dev/urandom")
            .context("opening /dev/urandom fallback for draft token")?;
        urandom
            .read_exact(&mut buf[filled..])
            .context("reading random bytes from /dev/urandom")?;
    }
    Ok(())
}

/// Generate a 128-bit single-use draft resume token (`hc_d_<32hex>`).
pub fn generate_draft_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    fill_random_bytes(&mut bytes)?;
    let mut token = String::with_capacity(5 + 32);
    token.push_str("hc_d_");
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(token, "{byte:02x}");
    }
    Ok(token)
}

fn row_to_draft_checkpoint(row: &rusqlite::Row<'_>) -> rusqlite::Result<DraftCheckpoint> {
    let id: i64 = row.get(0)?;
    let turn_id: Option<i64> = row.get(1)?;
    let token: String = row.get(2)?;
    let channel: String = row.get(3)?;
    let app_name: String = row.get(4)?;
    let sender: String = row.get(5)?;
    let chat_target: String = row.get(6)?;
    let source_notif_id: Option<i64> = row.get(7)?;
    let window_class: String = row.get(8)?;
    let context_note: String = row.get(9)?;
    let draft_text: String = row.get(10)?;
    let status_str: String = row.get(11)?;
    let consumed_action_str: Option<String> = row.get(12)?;
    let created_at_ms: i64 = row.get(13)?;
    let expires_at_ms: i64 = row.get(14)?;
    let consumed_at_ms: Option<i64> = row.get(15)?;

    Ok(DraftCheckpoint {
        id: id.max(0) as u64,
        turn_id: turn_id.map(|v| v.max(0) as u64),
        token,
        channel,
        app_name,
        sender,
        chat_target,
        source_notif_id: source_notif_id.and_then(|v| u32::try_from(v).ok()),
        window_class,
        context_note,
        draft_text,
        status: DraftStatus::parse(&status_str).unwrap_or(DraftStatus::Expired),
        consumed_action: consumed_action_str.as_deref().and_then(DraftAction::parse),
        created_at_ms,
        expires_at_ms,
        consumed_at_ms,
    })
}

fn insert_draft_checkpoint(
    conn: &Connection,
    spec: &NewDraftCheckpoint,
) -> Result<DraftCheckpoint> {
    let token = generate_draft_token()?;
    conn.execute(
        "INSERT INTO draft_checkpoints (
            turn_id, token, channel, app_name, sender, chat_target,
            source_notif_id, window_class, context_note, draft_text,
            status, created_at, expires_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending', ?11, ?12)",
        params![
            spec.turn_id.map(|v| v as i64),
            token,
            spec.channel,
            spec.app_name,
            spec.sender,
            spec.chat_target,
            spec.source_notif_id.map(i64::from),
            spec.window_class,
            spec.context_note,
            spec.draft_text,
            spec.created_at_ms,
            spec.expires_at_ms,
        ],
    )
    .context("inserting draft checkpoint")?;
    let id = conn.last_insert_rowid().max(0) as u64;
    Ok(DraftCheckpoint {
        id,
        turn_id: spec.turn_id,
        token,
        channel: spec.channel.clone(),
        app_name: spec.app_name.clone(),
        sender: spec.sender.clone(),
        chat_target: spec.chat_target.clone(),
        source_notif_id: spec.source_notif_id,
        window_class: spec.window_class.clone(),
        context_note: spec.context_note.clone(),
        draft_text: spec.draft_text.clone(),
        status: DraftStatus::Pending,
        consumed_action: None,
        created_at_ms: spec.created_at_ms,
        expires_at_ms: spec.expires_at_ms,
        consumed_at_ms: None,
    })
}

fn lookup_draft_checkpoint(
    conn: &Connection,
    token_or_id: &str,
) -> Result<Option<DraftCheckpoint>> {
    let trimmed = token_or_id.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if let Some(row) = conn
        .query_row(
            &format!("SELECT {DRAFT_CHECKPOINT_COLUMNS} FROM draft_checkpoints WHERE token = ?1"),
            params![trimmed],
            row_to_draft_checkpoint,
        )
        .optional()?
    {
        return Ok(Some(row));
    }
    let numeric = trimmed.strip_prefix('#').unwrap_or(trimmed);
    if let Ok(id) = numeric.parse::<u64>() {
        let row = conn
            .query_row(
                &format!("SELECT {DRAFT_CHECKPOINT_COLUMNS} FROM draft_checkpoints WHERE id = ?1"),
                params![id as i64],
                row_to_draft_checkpoint,
            )
            .optional()?;
        return Ok(row);
    }
    Ok(None)
}

fn claim_draft_checkpoint_conn(
    conn: &mut Connection,
    token_or_id: &str,
    action: DraftAction,
    now_ms: i64,
) -> Result<CheckpointClaim> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let Some(mut row) = lookup_draft_checkpoint(&tx, token_or_id)? else {
        tx.commit()?;
        return Ok(CheckpointClaim::NotFound);
    };
    let outcome = match row.status {
        DraftStatus::Pending if now_ms >= row.expires_at_ms => {
            tx.execute(
                "UPDATE draft_checkpoints
                 SET status = 'expired'
                 WHERE id = ?1 AND status = 'pending'",
                params![row.id as i64],
            )?;
            row.status = DraftStatus::Expired;
            CheckpointClaim::Expired(row)
        }
        DraftStatus::Pending => {
            let next_status = if action == DraftAction::Dismiss {
                DraftStatus::Dismissed
            } else {
                DraftStatus::Consumed
            };
            tx.execute(
                "UPDATE draft_checkpoints
                 SET status = ?2, consumed_action = ?3, consumed_at = ?4
                 WHERE id = ?1 AND status = 'pending'",
                params![row.id as i64, next_status.as_str(), action.as_str(), now_ms],
            )?;
            row.status = next_status;
            row.consumed_action = Some(action);
            row.consumed_at_ms = Some(now_ms);
            CheckpointClaim::Claimed(row)
        }
        DraftStatus::Expired => CheckpointClaim::Expired(row),
        DraftStatus::Consumed | DraftStatus::Dismissed => CheckpointClaim::AlreadyUsed(row),
    };
    tx.commit()?;
    Ok(outcome)
}

fn dismiss_drafts_for_chat_conn(
    conn: &Connection,
    channel: &str,
    chat_target: &str,
    now_ms: i64,
) -> Result<usize> {
    let ch = channel.trim();
    let target = chat_target.trim();
    if ch.is_empty() || target.is_empty() {
        return Ok(0);
    }
    let updated = conn.execute(
        "UPDATE draft_checkpoints
         SET status = 'dismissed',
             consumed_action = 'dismiss',
             consumed_at = ?3
         WHERE status = 'pending'
           AND lower(channel) = lower(?1)
           AND (
               (chat_target != '' AND lower(chat_target) = lower(?2))
               OR (chat_target = '' AND lower(sender) = lower(?2))
           )",
        params![ch, target, now_ms],
    )?;
    Ok(updated)
}

fn list_draft_checkpoints_conn(
    conn: &Connection,
    include_all: bool,
    limit: usize,
) -> Result<Vec<DraftCheckpoint>> {
    let cap = limit.clamp(1, 200) as i64;
    let sql = if include_all {
        format!(
            "SELECT {DRAFT_CHECKPOINT_COLUMNS}
             FROM draft_checkpoints
             ORDER BY id DESC
             LIMIT ?1"
        )
    } else {
        format!(
            "SELECT {DRAFT_CHECKPOINT_COLUMNS}
             FROM draft_checkpoints
             WHERE status = 'pending'
             ORDER BY id DESC
             LIMIT ?1"
        )
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![cap], row_to_draft_checkpoint)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Offline helper: persist a new stateless draft checkpoint directly in `history.db`.
pub fn create_draft_checkpoint(
    state_dir: &Path,
    spec: &NewDraftCheckpoint,
) -> Result<DraftCheckpoint> {
    let conn = open_migrated_writer(state_dir)?;
    insert_draft_checkpoint(&conn, spec)
}

/// Offline helper: atomically claim a draft checkpoint by token or `#id` / `id`
/// inside an `IMMEDIATE` transaction (works even when the daemon is stopped).
pub fn claim_draft_checkpoint(
    state_dir: &Path,
    token_or_id: &str,
    action: DraftAction,
    now_ms: i64,
) -> Result<CheckpointClaim> {
    let mut conn = open_migrated_writer(state_dir)?;
    claim_draft_checkpoint_conn(&mut conn, token_or_id, action, now_ms)
}

/// Offline helper: dismiss all pending draft checkpoints for `(channel, chat_target)`.
pub fn dismiss_drafts_for_chat(
    state_dir: &Path,
    channel: &str,
    chat_target: &str,
    now_ms: i64,
) -> Result<usize> {
    let conn = open_migrated_writer(state_dir)?;
    dismiss_drafts_for_chat_conn(&conn, channel, chat_target, now_ms)
}

/// Offline read-only helper: list draft checkpoints from `history.db`, newest first.
pub fn list_draft_checkpoints(
    state_dir: &Path,
    include_all: bool,
    limit: usize,
) -> Result<Vec<DraftCheckpoint>> {
    let Some(conn) = open_reader_connection(state_dir)? else {
        return Ok(Vec::new());
    };
    if !has_draft_checkpoints_table(&conn)? {
        return Ok(Vec::new());
    }
    list_draft_checkpoints_conn(&conn, include_all, limit)
}

/// Offline read-only helper: look up one draft checkpoint by token or `#id` / `id`.
pub fn get_draft_checkpoint(
    state_dir: &Path,
    token_or_id: &str,
) -> Result<Option<DraftCheckpoint>> {
    let Some(conn) = open_reader_connection(state_dir)? else {
        return Ok(None);
    };
    if !has_draft_checkpoints_table(&conn)? {
        return Ok(None);
    }
    lookup_draft_checkpoint(&conn, token_or_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hc-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn save_and_load_round_trip_with_private_permissions() {
        let dir = scratch("roundtrip");
        assert_eq!(load_last(&dir).unwrap(), None);

        let record = LastAnswer {
            version: SCHEMA_VERSION,
            id: 7,
            timestamp: unix_timestamp_secs(),
            request: "check <script>".to_string(),
            text: "Result: <script>alert(1)</script>".to_string(),
            denied: vec!["sudo id".to_string()],
            attachments: vec![],
            widgets: vec![],
            conversation_id: None,
            profile: String::new(),
        };
        save_last(&dir, &record).unwrap();

        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        let file_mode = std::fs::metadata(dir.join(LAST_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(load_last(&dir).unwrap(), Some(record));

        // Opening the SQLite store migrates legacy last.json once and removes it.
        let (store, last_id) = Store::open(&dir, &HistoryConfig::default()).unwrap();
        assert_eq!(last_id, 7);
        assert!(!dir.join(LAST_FILE).exists());
        assert_eq!(
            load_last(&dir).unwrap().unwrap().text,
            "Result: <script>alert(1)</script>"
        );
        let db_mode = std::fs::metadata(dir.join(DB_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(db_mode, 0o600);
        for sidecar in ["history.db-wal", "history.db-shm"] {
            let path = dir.join(sidecar);
            if path.exists() {
                let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{sidecar} must be 0600");
            }
        }
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn sqlite_lifecycle_monotonic_ids_crash_recovery_and_pruning() {
        let dir = scratch("lifecycle");
        let (store, initial) = Store::open(&dir, &HistoryConfig::default()).unwrap();
        assert_eq!(initial, 0);

        let now = unix_timestamp_ms();
        store.create_turn(NewTurn {
            id: 1,
            lane: Lane::Interactive,
            status: TurnStatus::Running,
            profile: String::new(),
            created_at_ms: now - 10_000,
            request: "München nixos-rebuild command".to_string(),
            attachments: vec![],
            origin: TurnOrigin::Cli,
            schedule_id: None,
        });
        store.finish_completed(
            1,
            "Use sudo nixos-rebuild switch --flake .".to_string(),
            vec![],
            now - 9_000,
        );

        // Background task completed, plus a suppressed heartbeat, plus an active task left running.
        store.create_turn(NewTurn {
            id: 2,
            lane: Lane::Background,
            status: TurnStatus::Queued,
            profile: "deep".to_string(),
            created_at_ms: now - 5_000,
            request: "research flake".to_string(),
            attachments: vec![],
            origin: TurnOrigin::Cli,
            schedule_id: None,
        });
        store.mark_running(
            2,
            Lane::Background,
            "sess-2".to_string(),
            None,
            "deep".to_string(),
            now - 4_500,
        );
        store.finish_completed(2, "Flake summary".to_string(), vec![], now - 3_000);

        store.create_turn(NewTurn {
            id: 3,
            lane: Lane::Background,
            status: TurnStatus::Running,
            profile: String::new(),
            created_at_ms: now - 2_000,
            request: "heartbeat".to_string(),
            attachments: vec![],
            origin: TurnOrigin::Cli,
            schedule_id: None,
        });
        store.finish_suppressed(3, "HEARTBEAT_OK".to_string(), now - 1_500);

        store.create_turn(NewTurn {
            id: 4,
            lane: Lane::Background,
            status: TurnStatus::Running,
            profile: String::new(),
            created_at_ms: now - 1_000,
            request: "still running when killed".to_string(),
            attachments: vec![],
            origin: TurnOrigin::Cli,
            schedule_id: None,
        });

        store.flush().await;

        // Default `load_last` returns interactive only (id 1), not background (id 2).
        assert_eq!(load_last(&dir).unwrap().unwrap().id, 1);
        assert_eq!(load_last_for_lane(&dir, None).unwrap().unwrap().id, 2);

        // Suppressed turns are excluded from default list_turns and from recall.
        let default_list = list_turns(
            &dir,
            &ListFilter {
                lane: None,
                status: None,
                include_suppressed: false,
                limit: 20,
            },
        )
        .unwrap();
        assert!(!default_list.iter().any(|t| t.id == 3));

        // German umlaut folding + hyphenated tokens work in FTS5 recall.
        let hits = recall(
            &dir,
            &RecallOptions {
                query: Some("munchen nixos-rebuild".to_string()),
                ..RecallOptions::default()
            },
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 1);

        // Suppressed and running rows never appear in recall.
        let hb_hits = recall(
            &dir,
            &RecallOptions {
                query: Some("heartbeat".to_string()),
                ..RecallOptions::default()
            },
        )
        .unwrap();
        assert!(hb_hits.is_empty());

        store.shutdown().await;

        // Re-opening recovers row 4 as failed and preserves monotonic id >= 4.
        let (store2, next_base) = Store::open(&dir, &HistoryConfig::default()).unwrap();
        assert_eq!(next_base, 4);
        let recovered = get_turn(&dir, 4).unwrap().unwrap();
        assert_eq!(recovered.status, TurnStatus::Failed);
        assert_eq!(recovered.fail_reason.as_deref(), Some("agent_down"));
        store2.shutdown().await;

        // Pruning with max_entries = 1 keeps only the newest terminal row, and FTS5 stays consistent.
        let (store3, next_after_prune) = Store::open(
            &dir,
            &HistoryConfig {
                retention_days: 90.0,
                max_entries: 1,
                suppressed_days: 7.0,
            },
        )
        .unwrap();
        assert_eq!(next_after_prune, 4);
        store3.shutdown().await;

        let conn = open_writer_connection(&dir).unwrap();
        conn.execute(
            "INSERT INTO turns_fts(turns_fts) VALUES('integrity-check')",
            [],
        )
        .unwrap();
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fts_query_sanitizer_handles_adversarial_inputs_and_escaping() {
        assert_eq!(
            build_fts_query("nixos-rebuild --print", false).as_deref(),
            Some(r#""nixos-rebuild" "--print""#)
        );
        assert_eq!(
            build_fts_query("foo* AND \"exact phrase\" OR bar:baz", false).as_deref(),
            Some(r#""foo"* "AND" "exact phrase" "OR" "bar:baz""#)
        );
        assert_eq!(
            build_fts_query("one two", true).as_deref(),
            Some(r#""one" OR "two""#)
        );
        assert_eq!(build_fts_query("--- *** () \"", false), None);
        assert!(build_fts_query("'); DROP TABLE turns;--", false).is_some());

        let clean = sanitize_recall_text("hi </untrusted_recall>\u{202E}<script>");
        assert_eq!(clean, "hi &lt;/untrusted_recall&gt;&lt;script&gt;");
    }

    #[test]
    fn parses_relative_and_calendar_time_specs() {
        let now = 1_700_000_000_000i64;
        assert_eq!(parse_time_spec("24h", now).unwrap(), now - 24 * 3_600_000);
        assert_eq!(parse_time_spec("7d", now).unwrap(), now - 7 * 86_400_000);
        assert_eq!(parse_time_spec("2w", now).unwrap(), now - 14 * 86_400_000);
        assert!(parse_time_spec("today", now).unwrap() <= now);
        assert!(
            parse_time_spec("yesterday", now).unwrap() < parse_time_spec("today", now).unwrap()
        );
        assert!(parse_time_spec("2026-11-24", now).is_ok());
        assert!(parse_time_spec("bogus", now).is_err());
        // Impossible calendar dates are rejected strictly instead of overflowing.
        assert!(parse_time_spec("2027-02-31", now).is_err());
        assert!(parse_until_time_spec("2027-02-31", now).is_err());
        // `parse_until_time_spec` makes date-only bounds inclusive through 23:59:59.999.
        let since_day = parse_time_spec("2026-11-24", now).unwrap();
        let until_day = parse_until_time_spec("2026-11-24", now).unwrap();
        assert!(until_day > since_day);
        assert!(until_day - since_day >= 23 * 3_600_000);
    }

    #[tokio::test]
    async fn migrates_v1_to_v2_and_runs_schedule_crud_and_maintenance() {
        let dir = scratch("v1-to-v2");
        // Seed a schema v1 database directly.
        {
            let conn = open_writer_connection(&dir).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }
        // Opening Store upgrades v1 -> v2 in place and runs FTS5/SQLite maintenance.
        let (store, _) = Store::open(&dir, &HistoryConfig::default()).unwrap();
        let now = unix_timestamp_ms();
        let created = create_schedule(
            &dir,
            &NewSchedule {
                name: "ci-watch".to_string(),
                schedule_spec: "every 15m".to_string(),
                prompt: "check CI".to_string(),
                profile: "deep".to_string(),
                script: "echo fail".to_string(),
                no_agent: false,
                suppress_token: Some("HEARTBEAT_OK".to_string()),
                failure_deliver: FailureDeliver::Notify,
                max_runs: Some(5),
                expires_at: None,
                next_run_at: now - 1000,
            },
            128,
        )
        .unwrap();
        assert_eq!(created.name, "ci-watch");
        assert!(created.enabled);
        assert_eq!(list_schedules(&dir, false).unwrap().len(), 1);
        assert_eq!(due_schedules(&dir, now).unwrap().len(), 1);

        let paused = set_schedule_enabled(&dir, "ci-watch", false, None).unwrap();
        assert!(!paused.enabled);
        assert!(list_schedules(&dir, false).unwrap().is_empty());
        assert_eq!(list_schedules(&dir, true).unwrap().len(), 1);

        assert!(
            delete_schedule(&dir, &created.id.to_string())
                .unwrap()
                .is_some()
        );
        assert!(list_schedules(&dir, true).unwrap().is_empty());

        store.shutdown().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn managed_schedules_oneshot_reuse_and_suppressed_first_pruning() {
        use crate::config::ScheduledJobConfig;
        use std::collections::BTreeMap;

        let dir = scratch("v3-managed-and-prune");
        // Seed schema v2 database directly to verify v2 -> v3 migration.
        {
            let conn = open_writer_connection(&dir).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(SCHEMA_V2).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }

        let (store, _) = Store::open(
            &dir,
            &HistoryConfig {
                retention_days: 90.0,
                max_entries: 2,
                suppressed_days: 7.0,
            },
        )
        .unwrap();
        let now = unix_timestamp_ms();

        // 1. Declarative managed schedules sync, reject `delete_schedule`, and
        //    clean up when removed from config.
        let mut jobs = BTreeMap::new();
        jobs.insert(
            "morning-digest".to_string(),
            ScheduledJobConfig {
                enabled: true,
                schedule: "*/5 5-14 * * *".to_string(),
                prompt: "digest v1".to_string(),
                profile: "deep".to_string(),
                script: "echo hi".to_string(),
                no_agent: false,
                suppress_token: Some("HEARTBEAT_OK".to_string()),
                failure_deliver: FailureDeliver::Notify,
            },
        );
        sync_managed_schedules(&dir, &jobs, now).unwrap();
        let seeded = get_schedule(&dir, "morning-digest").unwrap().unwrap();
        assert!(seeded.managed);
        assert_eq!(seeded.prompt, "digest v1");
        assert!(delete_schedule(&dir, "morning-digest").is_err());

        jobs.get_mut("morning-digest").unwrap().prompt = "digest v2".to_string();
        sync_managed_schedules(&dir, &jobs, now + 1000).unwrap();
        let updated = get_schedule(&dir, "morning-digest").unwrap().unwrap();
        assert_eq!(updated.id, seeded.id);
        assert_eq!(updated.prompt, "digest v2");

        jobs.clear();
        sync_managed_schedules(&dir, &jobs, now + 2000).unwrap();
        assert!(get_schedule(&dir, "morning-digest").unwrap().is_none());

        // 2. Disabled one-shot schedule (`at ...`) is reused cleanly on name collision,
        //    whereas a paused recurring schedule still errors on duplicate name.
        let oneshot = create_schedule(
            &dir,
            &NewSchedule {
                name: "tea-timer".to_string(),
                schedule_spec: "in 5m".to_string(),
                prompt: "steep tea".to_string(),
                profile: String::new(),
                script: String::new(),
                no_agent: false,
                suppress_token: None,
                failure_deliver: FailureDeliver::Notify,
                max_runs: None,
                expires_at: None,
                next_run_at: now + 300_000,
            },
            128,
        )
        .unwrap();
        record_schedule_outcome(
            &dir,
            oneshot.id,
            now + 300_000,
            now + 300_000,
            "ok",
            None,
            false,
            false,
            true,
        )
        .unwrap();
        let reused = create_schedule(
            &dir,
            &NewSchedule {
                name: "tea-timer".to_string(),
                schedule_spec: "in 10m".to_string(),
                prompt: "steep green tea".to_string(),
                profile: String::new(),
                script: String::new(),
                no_agent: false,
                suppress_token: None,
                failure_deliver: FailureDeliver::Notify,
                max_runs: None,
                expires_at: None,
                next_run_at: now + 600_000,
            },
            128,
        )
        .unwrap();
        assert_eq!(reused.id, oneshot.id);
        assert!(reused.enabled);
        assert_eq!(reused.prompt, "steep green tea");

        // 3. Under `max_entries = 2`, newer `suppressed` noise is evicted before
        //    older `completed` user conversations.
        for id in [1u64, 2] {
            store.create_turn(NewTurn {
                id,
                lane: Lane::Interactive,
                status: TurnStatus::Queued,
                profile: String::new(),
                created_at_ms: now + (id as i64) * 10,
                request: format!("important question {id}"),
                attachments: vec![],
                origin: TurnOrigin::Cli,
                schedule_id: None,
            });
            store.finish_completed(id, format!("important answer {id}"), vec![], now + 100);
        }
        store.create_turn(NewTurn {
            id: 3,
            lane: Lane::Background,
            status: TurnStatus::Queued,
            profile: String::new(),
            created_at_ms: now + 500,
            request: "noisy heartbeat".to_string(),
            attachments: vec![],
            origin: TurnOrigin::Cli,
            schedule_id: None,
        });
        store.finish_suppressed(3, "HEARTBEAT_OK".to_string(), now + 600);
        store.shutdown().await;

        let (store2, _) = Store::open(
            &dir,
            &HistoryConfig {
                retention_days: 90.0,
                max_entries: 2,
                suppressed_days: 7.0,
            },
        )
        .unwrap();
        assert!(get_turn(&dir, 1).unwrap().is_some());
        assert!(get_turn(&dir, 2).unwrap().is_some());
        assert!(get_turn(&dir, 3).unwrap().is_none());
        store2.shutdown().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn schema_v4_widgets_conversation_threads_and_distill_filtering() {
        let dir = scratch("v3-to-v4");
        // Seed a schema v3 database directly to verify v3 -> v4 migration.
        {
            let conn = open_writer_connection(&dir).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(SCHEMA_V2).unwrap();
            conn.execute_batch(SCHEMA_V3).unwrap();
            conn.pragma_update(None, "user_version", 3).unwrap();
        }

        let (store, _) = Store::open(&dir, &HistoryConfig::default()).unwrap();
        let now = unix_timestamp_ms();

        // Turn 1 (voice) + Turn 2 (typed follow-up with widget) in conversation 1.
        store.create_turn(NewTurn {
            id: 1,
            lane: Lane::Interactive,
            status: TurnStatus::Running,
            profile: "fast".to_string(),
            created_at_ms: now - 5_000,
            request: "Wie wird das Wetter heute?".to_string(),
            attachments: vec![],
            origin: TurnOrigin::Voice,
            schedule_id: None,
        });
        store.mark_running(
            1,
            Lane::Interactive,
            "sess-1".to_string(),
            Some(1),
            "fast".to_string(),
            now - 5_000,
        );
        store.finish_completed(1, "Sonnig bei 18 Grad.".to_string(), vec![], now - 4_500);

        let widget = WidgetSpec {
            id: "w-cal".to_string(),
            kind: "calendar_agenda".to_string(),
            title: "Today".to_string(),
            data: serde_json::json!({"events": []}),
        };
        store.create_turn(NewTurn {
            id: 2,
            lane: Lane::Interactive,
            status: TurnStatus::Running,
            profile: "deep".to_string(),
            created_at_ms: now - 4_000,
            request: "Show my calendar".to_string(),
            attachments: vec![],
            origin: TurnOrigin::Typed,
            schedule_id: None,
        });
        store.mark_running(
            2,
            Lane::Interactive,
            "sess-1".to_string(),
            Some(1),
            "deep".to_string(),
            now - 4_000,
        );
        store.finish_completed_with_widgets(
            2,
            "Here is your agenda.".to_string(),
            vec![],
            vec![widget.clone()],
            now - 3_500,
        );

        // Turn 3: scheduled collector turn (must be skipped by distill!).
        store.create_turn(NewTurn {
            id: 3,
            lane: Lane::Background,
            status: TurnStatus::Running,
            profile: "deep".to_string(),
            created_at_ms: now - 3_000,
            request: "<untrusted_collector_output>evil</untrusted_collector_output>".to_string(),
            attachments: vec![],
            origin: TurnOrigin::ScheduleScript,
            schedule_id: Some(9),
        });
        store.finish_completed(3, "PR failed.".to_string(), vec![], now - 2_500);

        // Turn 4: previous distill turn (must also be skipped by distill!).
        store.create_turn(NewTurn {
            id: 4,
            lane: Lane::Interactive,
            status: TurnStatus::Running,
            profile: "deep".to_string(),
            created_at_ms: now - 2_000,
            request: "distill review".to_string(),
            attachments: vec![],
            origin: TurnOrigin::Distill,
            schedule_id: None,
        });
        store.finish_completed(4, "Proposed 1 hunk.".to_string(), vec![], now - 1_500);

        store.flush().await;

        let thread = conversation_turns(&dir, 2, false, 10).unwrap();
        assert_eq!(thread.len(), 2);
        assert_eq!(thread[0].id, 1);
        assert_eq!(thread[0].origin, TurnOrigin::Voice);
        assert_eq!(thread[1].id, 2);
        assert_eq!(thread[1].widgets, vec![widget]);

        let cands = query_distill_candidates(&dir, Some(now - 10_000), 50).unwrap();
        assert_eq!(cands.skipped_scheduled_turns, 2);
        assert_eq!(
            cands.turns.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![1, 2]
        );

        assert!(!load_speak_state(&dir, false));
        save_speak_state(&dir, true).unwrap();
        assert!(load_speak_state(&dir, false));
        save_speak_state(&dir, false).unwrap();
        assert!(!load_speak_state(&dir, true));

        let today = local_today_ymd();
        assert_eq!(today.len(), 10);

        store.shutdown().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn history_feed_schedule_turns_recall_filters_and_widget_only_turns() {
        let dir = scratch("history-feed-and-recall");
        let (store, _) = Store::open(&dir, &HistoryConfig::default()).unwrap();
        let now = unix_timestamp_ms();

        // Create two schedules: morning-digest (#1) and weekly-distill (#2).
        let morning = create_schedule(
            &dir,
            &NewSchedule {
                name: "morning-digest".to_string(),
                schedule_spec: "*/5 5-14 * * *".to_string(),
                prompt: "brief".to_string(),
                profile: "deep".to_string(),
                script: "collect".to_string(),
                no_agent: false,
                suppress_token: Some("HEARTBEAT_OK".to_string()),
                failure_deliver: FailureDeliver::Notify,
                max_runs: None,
                expires_at: None,
                next_run_at: now + 60_000,
            },
            10,
        )
        .unwrap();
        let weekly = create_schedule(
            &dir,
            &NewSchedule {
                name: "weekly-distill".to_string(),
                schedule_spec: "0 18 * * 0".to_string(),
                prompt: String::new(),
                profile: String::new(),
                script: "hey-computer distill -b".to_string(),
                no_agent: true,
                suppress_token: None,
                failure_deliver: FailureDeliver::Log,
                max_runs: None,
                expires_at: None,
                next_run_at: now + 120_000,
            },
            10,
        )
        .unwrap();

        // Multi-turn user conversation: turn 1 -> turn 2 (conv_id = 1).
        store.create_turn(NewTurn {
            id: 1,
            lane: Lane::Interactive,
            status: TurnStatus::Running,
            profile: String::new(),
            created_at_ms: now - 20_000,
            request: "How does NixOS flake locking work?".to_string(),
            attachments: vec![],
            origin: TurnOrigin::Voice,
            schedule_id: None,
        });
        store.mark_running(
            1,
            Lane::Interactive,
            "s1".to_string(),
            Some(1),
            String::new(),
            now - 20_000,
        );
        store.finish_completed(
            1,
            "Flake locks pin inputs in flake.lock.".to_string(),
            vec![],
            now - 19_000,
        );

        store.create_turn(NewTurn {
            id: 2,
            lane: Lane::Interactive,
            status: TurnStatus::Running,
            profile: String::new(),
            created_at_ms: now - 15_000,
            request: "And how do I update just one input?".to_string(),
            attachments: vec![],
            origin: TurnOrigin::Typed,
            schedule_id: None,
        });
        store.mark_running(
            2,
            Lane::Interactive,
            "s1".to_string(),
            Some(1),
            String::new(),
            now - 15_000,
        );
        store.finish_completed(
            2,
            "Run nix flake update <input>.".to_string(),
            vec![],
            now - 14_000,
        );

        // Turn 3: scheduled Morning Brief turn.
        store.create_turn(NewTurn {
            id: 3,
            lane: Lane::Background,
            status: TurnStatus::Running,
            profile: "deep".to_string(),
            created_at_ms: now - 10_000,
            request: "long collector prompt".to_string(),
            attachments: vec![],
            origin: TurnOrigin::ScheduleScript,
            schedule_id: Some(morning.id),
        });
        store.finish_completed(
            3,
            "# Morning Brief — Wednesday\n\nQuiet start ahead.".to_string(),
            vec![],
            now - 9_000,
        );

        // Turn 4: widget-only interactive turn (empty answer, 1 widget).
        let widget = WidgetSpec {
            id: "w-1".to_string(),
            kind: "transit_board".to_string(),
            title: "U3 Departures".to_string(),
            data: serde_json::json!({}),
        };
        store.create_turn(NewTurn {
            id: 4,
            lane: Lane::Interactive,
            status: TurnStatus::Running,
            profile: String::new(),
            created_at_ms: now - 8_000,
            request: "Next train?".to_string(),
            attachments: vec![],
            origin: TurnOrigin::Voice,
            schedule_id: None,
        });
        store.finish_completed_with_widgets(4, String::new(), vec![], vec![widget], now - 7_500);

        // Turn 5: historical distill turn with schedule_id = None (tests fallback!).
        store.create_turn(NewTurn {
            id: 5,
            lane: Lane::Background,
            status: TurnStatus::Running,
            profile: "deep".to_string(),
            created_at_ms: now - 6_000,
            request: "distill review".to_string(),
            attachments: vec![],
            origin: TurnOrigin::Distill,
            schedule_id: None,
        });
        store.finish_completed(
            5,
            "No memory changes needed.".to_string(),
            vec![],
            now - 5_500,
        );

        // Turn 6: currently running background task.
        store.create_turn(NewTurn {
            id: 6,
            lane: Lane::Background,
            status: TurnStatus::Running,
            profile: "deep".to_string(),
            created_at_ms: now - 3_000,
            request: "Summarize PR #88 diff".to_string(),
            attachments: vec![],
            origin: TurnOrigin::Typed,
            schedule_id: None,
        });

        store.flush().await;

        // 1. `load_last_for_lane` includes widget-only completed turn #4!
        let last_interactive = load_last_for_lane(&dir, Some(Lane::Interactive))
            .unwrap()
            .unwrap();
        assert_eq!(last_interactive.id, 4);
        assert_eq!(last_interactive.widgets.len(), 1);

        // 2. `list_turns_for_schedule` returns #3 for morning-digest and falls back to #5 for weekly-distill!
        let morning_turns = list_turns_for_schedule(&dir, morning.id, 10).unwrap();
        assert_eq!(
            morning_turns.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![3]
        );
        let weekly_turns = list_turns_for_schedule(&dir, weekly.id, 10).unwrap();
        assert_eq!(
            weekly_turns.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![5]
        );

        // 3. `recall` with `user_only = true` skips scheduled (#3) and distill (#5) turns,
        //    and includes widget-only turn (#4) in empty-query recall.
        let user_hits = recall(
            &dir,
            &RecallOptions {
                user_only: true,
                limit: 10,
                ..RecallOptions::default()
            },
        )
        .unwrap();
        assert_eq!(
            user_hits.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![4, 2, 1]
        );
        assert_eq!(user_hits[0].widget_count, 1);
        assert_eq!(user_hits[1].conversation_id, Some(1));
        assert_eq!(user_hits[1].origin, TurnOrigin::Typed);

        let formatted = format_recall_output(&user_hits, false, 1200, false);
        assert!(formatted.contains("conv=1"), "{formatted}");
        assert!(formatted.contains("origin=voice"), "{formatted}");
        assert!(formatted.contains("widgets=1"), "{formatted}");

        // 4. `format_recall_thread` returns full chronological conversation wrapped in `<untrusted_recall>`.
        let thread_text = format_recall_thread(&dir, 2, true, 1200, false)
            .unwrap()
            .unwrap();
        assert!(thread_text.starts_with("<untrusted_recall>\n"));
        assert!(thread_text.contains("[#1 "));
        assert!(thread_text.contains("[#2 "));
        assert!(thread_text.contains("flake.lock"));
        assert!(thread_text.contains("nix flake update"));

        // 5. `history_feed` surfaces active task #6 and collapses conversation 1 (#1 + #2 -> id #2, turn_count 2).
        let feed = history_feed(&dir, &HistoryFeedOptions::default()).unwrap();
        assert_eq!(feed.active_tasks.len(), 1);
        assert_eq!(feed.active_tasks[0].id, 6);
        let conv_item = feed.items.iter().find(|i| i.id == 2).unwrap();
        assert_eq!(conv_item.conversation_id, 1);
        assert_eq!(conv_item.turn_count, 2);
        assert_eq!(conv_item.title, "How does NixOS flake locking work?");
        let brief_item = feed.items.iter().find(|i| i.id == 3).unwrap();
        assert_eq!(brief_item.schedule_name.as_deref(), Some("morning-digest"));
        assert_eq!(brief_item.title, "Morning Brief — Wednesday");

        // 6. `history_feed` with query uses FTS5.
        let search_feed = history_feed(
            &dir,
            &HistoryFeedOptions {
                query: Some("flake.lock".to_string()),
                ..HistoryFeedOptions::default()
            },
        )
        .unwrap();
        assert_eq!(search_feed.items.len(), 1);
        assert_eq!(search_feed.items[0].id, 1);

        // 7. `parse_day_range` accepts calendar dates and rejects relative specs.
        let (s, e) = parse_day_range("2026-11-24", now).unwrap();
        assert!(e > s);
        assert!(parse_day_range("today", now).is_ok());
        assert!(parse_day_range("yesterday", now).is_ok());
        assert!(parse_day_range("24h", now).is_err());

        store.shutdown().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn schema_v5_draft_checkpoints_atomic_claims_pruning_and_restricted_exclusion() {
        let dir = scratch("v4-to-v5-drafts");
        // Seed a schema v4 database directly to verify v4 -> v5 migration.
        {
            let conn = open_writer_connection(&dir).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(SCHEMA_V2).unwrap();
            conn.execute_batch(SCHEMA_V3).unwrap();
            conn.execute_batch(SCHEMA_V4).unwrap();
            conn.pragma_update(None, "user_version", 4).unwrap();
        }

        let (store, _) = Store::open(&dir, &HistoryConfig::default()).unwrap();
        let now = unix_timestamp_ms();

        // 1. Token format (`hc_d_<32hex>`) and uniqueness.
        let tok1 = generate_draft_token().unwrap();
        let tok2 = generate_draft_token().unwrap();
        assert!(tok1.starts_with("hc_d_"));
        assert_eq!(tok1.len(), 37);
        assert!(
            tok1[5..]
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        );
        assert_ne!(tok1, tok2);

        // 2. Create draft checkpoints via Store actor + offline helper.
        let cp1 = store
            .create_draft_checkpoint(NewDraftCheckpoint {
                turn_id: Some(10),
                channel: "signal".to_string(),
                app_name: "Signal".to_string(),
                sender: "Alice".to_string(),
                chat_target: "+15550101".to_string(),
                source_notif_id: Some(77),
                window_class: "signal".to_string(),
                context_note: "Asking about dinner".to_string(),
                draft_text: "Ja gerne, 19:00 passt!".to_string(),
                created_at_ms: now,
                expires_at_ms: now + 60_000,
            })
            .await
            .unwrap();
        assert_eq!(cp1.status, DraftStatus::Pending);
        assert!(cp1.token.starts_with("hc_d_"));
        assert_eq!(cp1.source_notif_id, Some(77));
        assert_eq!(cp1.context_note, "Asking about dinner");

        let cp2 = create_draft_checkpoint(
            &dir,
            &NewDraftCheckpoint {
                turn_id: Some(11),
                channel: "whatsapp".to_string(),
                app_name: "Whatsapp".to_string(),
                sender: "Bob".to_string(),
                chat_target: "bob@s.whatsapp.net".to_string(),
                source_notif_id: Some(78),
                window_class: "whatsapp-electron".to_string(),
                context_note: String::new(),
                draft_text: "Bin gleich da!".to_string(),
                created_at_ms: now,
                expires_at_ms: now + 10_000,
            },
        )
        .unwrap();

        let cp3 = store
            .create_draft_checkpoint(NewDraftCheckpoint {
                turn_id: Some(12),
                channel: "whatsapp".to_string(),
                app_name: "Whatsapp".to_string(),
                sender: "Bob".to_string(),
                chat_target: "bob@s.whatsapp.net".to_string(),
                source_notif_id: Some(79),
                window_class: "whatsapp-electron".to_string(),
                context_note: "Follow-up".to_string(),
                draft_text: "Alles klar".to_string(),
                created_at_ms: now,
                expires_at_ms: now + 60_000,
            })
            .await
            .unwrap();

        // Lookup by token, `#id`, and numeric id.
        assert_eq!(
            store.get_draft_checkpoint(cp1.token.clone()).await.unwrap(),
            Some(cp1.clone())
        );
        assert_eq!(
            get_draft_checkpoint(&dir, &format!("#{}", cp1.id)).unwrap(),
            Some(cp1.clone())
        );
        assert_eq!(
            get_draft_checkpoint(&dir, &cp1.id.to_string()).unwrap(),
            Some(cp1.clone())
        );
        assert_eq!(list_draft_checkpoints(&dir, false, 20).unwrap().len(), 3);

        // 3. Atomic claim: first claim succeeds, replay returns `AlreadyUsed`.
        let first_claim = store
            .claim_draft_checkpoint(cp1.token.clone(), DraftAction::Copy, now + 1_000)
            .await
            .unwrap();
        match first_claim {
            CheckpointClaim::Claimed(claimed) => {
                assert_eq!(claimed.id, cp1.id);
                assert_eq!(claimed.status, DraftStatus::Consumed);
                assert_eq!(claimed.consumed_action, Some(DraftAction::Copy));
                assert_eq!(claimed.consumed_at_ms, Some(now + 1_000));
            }
            other => panic!("expected Claimed, got {other:?}"),
        }
        let replay =
            claim_draft_checkpoint(&dir, &cp1.token, DraftAction::Paste, now + 2_000).unwrap();
        assert!(matches!(
            replay,
            CheckpointClaim::AlreadyUsed(row) if row.consumed_action == Some(DraftAction::Copy)
        ));
        assert_eq!(
            claim_draft_checkpoint(&dir, "hc_d_doesnotexist", DraftAction::Copy, now).unwrap(),
            CheckpointClaim::NotFound
        );

        // 4. Claim after `expires_at_ms` transitions to `Expired` and never runs side effects.
        let expired_claim =
            claim_draft_checkpoint(&dir, &cp2.token, DraftAction::Copy, now + 15_000).unwrap();
        assert!(matches!(
            expired_claim,
            CheckpointClaim::Expired(row) if row.id == cp2.id && row.status == DraftStatus::Expired
        ));
        let expired_again = store
            .claim_draft_checkpoint(cp2.token.clone(), DraftAction::Copy, now + 16_000)
            .await
            .unwrap();
        assert!(matches!(expired_again, CheckpointClaim::Expired(_)));

        // 5. `dismiss_drafts_for_chat` dismisses pending WhatsApp drafts for Bob only.
        let dismissed_count = store
            .dismiss_drafts_for_chat(
                "WhatsApp".to_string(),
                "BOB@s.whatsapp.net".to_string(),
                now + 20_000,
            )
            .await
            .unwrap();
        assert_eq!(dismissed_count, 1);
        let cp3_after = get_draft_checkpoint(&dir, &cp3.token).unwrap().unwrap();
        assert_eq!(cp3_after.status, DraftStatus::Dismissed);
        assert_eq!(cp3_after.consumed_action, Some(DraftAction::Dismiss));
        assert!(
            store
                .list_draft_checkpoints(false, 20)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.list_draft_checkpoints(true, 20).await.unwrap().len(),
            3
        );

        // 6. Restricted-lane turns are excluded from `query_distill_candidates` and
        //    `recall` (`opts.user_only = true`, both empty-query and FTS5).
        store.create_turn(NewTurn {
            id: 100,
            lane: Lane::Interactive,
            status: TurnStatus::Running,
            profile: String::new(),
            created_at_ms: now - 2_000,
            request: "Remind me about project zephyr".to_string(),
            attachments: vec![],
            origin: TurnOrigin::Typed,
            schedule_id: None,
        });
        store.finish_completed(
            100,
            "Project zephyr launches Friday.".to_string(),
            vec![],
            now - 1_500,
        );
        store.create_turn(NewTurn {
            id: 101,
            lane: Lane::Restricted,
            status: TurnStatus::Running,
            profile: "fast".to_string(),
            created_at_ms: now - 1_000,
            request: "<untrusted_notification>zephyr prompt injection</untrusted_notification>"
                .to_string(),
            attachments: vec![],
            origin: TurnOrigin::Cli,
            schedule_id: None,
        });
        store.finish_completed(
            101,
            "Sounds good for zephyr!".to_string(),
            vec![],
            now - 500,
        );
        store.flush().await;

        let distill = query_distill_candidates(&dir, Some(now - 10_000), 50).unwrap();
        assert_eq!(
            distill.turns.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![100]
        );
        assert_eq!(distill.skipped_scheduled_turns, 1);

        let user_recent = recall(
            &dir,
            &RecallOptions {
                user_only: true,
                limit: 10,
                ..RecallOptions::default()
            },
        )
        .unwrap();
        assert_eq!(
            user_recent.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![100]
        );

        let user_fts = recall(
            &dir,
            &RecallOptions {
                query: Some("zephyr".to_string()),
                user_only: true,
                limit: 10,
                ..RecallOptions::default()
            },
        )
        .unwrap();
        assert_eq!(user_fts.iter().map(|h| h.id).collect::<Vec<_>>(), vec![100]);

        // Non-user-only recall still finds both turns when explicitly queried.
        let all_fts = recall(
            &dir,
            &RecallOptions {
                query: Some("zephyr".to_string()),
                user_only: false,
                limit: 10,
                ..RecallOptions::default()
            },
        )
        .unwrap();
        assert_eq!(all_fts.len(), 2);

        // 7. TTL pruning expires overdue pending drafts and deletes old terminal drafts.
        let old_ts = now - 10 * 86_400_000;
        let old_pending = store
            .create_draft_checkpoint(NewDraftCheckpoint {
                turn_id: None,
                channel: "signal".to_string(),
                app_name: "Signal".to_string(),
                sender: "Old".to_string(),
                chat_target: "+15550999".to_string(),
                source_notif_id: None,
                window_class: "signal".to_string(),
                context_note: String::new(),
                draft_text: "stale".to_string(),
                created_at_ms: old_ts,
                expires_at_ms: old_ts + 60_000,
            })
            .await
            .unwrap();
        store.shutdown().await;

        // Re-opening runs `prune_connection`: marks `old_pending` expired and
        // deletes it because `created_at < now - suppressed_days (7d)`.
        let (store2, _) = Store::open(&dir, &HistoryConfig::default()).unwrap();
        assert!(
            get_draft_checkpoint(&dir, &old_pending.token)
                .unwrap()
                .is_none()
        );
        // Recent terminal drafts (cp1/cp2/cp3) are retained within 7 days.
        assert!(get_draft_checkpoint(&dir, &cp1.token).unwrap().is_some());
        store2.shutdown().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn record_schedule_skip_preserves_prior_outcome_and_counts() {
        let dir = scratch("schedule-skip");
        let now = unix_timestamp_ms();
        let next_run = now + 300_000;

        // 1. Brand-new schedule (`last_status == None`) -> skip sets `Some("skipped")`,
        //    leaves `last_run_at == None` and `run_count == 0`.
        let recurring = create_schedule(
            &dir,
            &NewSchedule {
                name: "morning-digest".to_string(),
                schedule_spec: "*/5 5-14 * * *".to_string(),
                prompt: "digest".to_string(),
                profile: "deep".to_string(),
                script: "true".to_string(),
                no_agent: false,
                suppress_token: Some("HEARTBEAT_OK".to_string()),
                failure_deliver: FailureDeliver::Notify,
                max_runs: Some(2),
                expires_at: None,
                next_run_at: next_run,
            },
            128,
        )
        .unwrap();
        assert_eq!(recurring.last_status, None);
        assert_eq!(recurring.last_run_at, None);
        assert_eq!(recurring.run_count, 0);

        record_schedule_skip(&dir, recurring.id, now + 900, now + 1_000, false).unwrap();
        let after_initial_skip = get_schedule(&dir, "morning-digest").unwrap().unwrap();
        assert_eq!(after_initial_skip.last_status.as_deref(), Some("skipped"));
        assert_eq!(after_initial_skip.last_run_at, None);
        assert_eq!(after_initial_skip.run_count, 0);
        assert!(after_initial_skip.enabled);
        assert_eq!(after_initial_skip.next_run_at, Some(next_run));

        // 2. Schedule with `last_status = Some("ok")`, `last_run_at = Some(t)`, `run_count = 1`,
        //    `max_runs = Some(2)` -> skip preserves `"ok"`, `Some(t)`, `run_count = 1`, `enabled = true`.
        let ok_ts = now + 2_000;
        record_schedule_outcome(
            &dir,
            recurring.id,
            ok_ts,
            ok_ts + 50,
            "ok",
            None,
            false,
            false,
            false,
        )
        .unwrap();
        record_schedule_skip(&dir, recurring.id, ok_ts + 4_900, ok_ts + 5_000, false).unwrap();
        let after_ok_skip = get_schedule(&dir, "morning-digest").unwrap().unwrap();
        assert_eq!(after_ok_skip.last_status.as_deref(), Some("ok"));
        assert_eq!(after_ok_skip.last_run_at, Some(ok_ts));
        assert_eq!(after_ok_skip.run_count, 1);
        assert!(after_ok_skip.enabled);
        assert_eq!(after_ok_skip.next_run_at, Some(next_run));

        // 3. Schedule with `last_status = Some("suppressed")` -> skip preserves `"suppressed"`,
        //    `last_run_at`, and `run_count`.
        let suppressed_job = create_schedule(
            &dir,
            &NewSchedule {
                name: "quiet-watch".to_string(),
                schedule_spec: "every 5m".to_string(),
                prompt: "check".to_string(),
                profile: String::new(),
                script: "true".to_string(),
                no_agent: false,
                suppress_token: Some("HEARTBEAT_OK".to_string()),
                failure_deliver: FailureDeliver::Notify,
                max_runs: None,
                expires_at: None,
                next_run_at: next_run,
            },
            128,
        )
        .unwrap();
        let supp_ts = now + 3_000;
        record_schedule_outcome(
            &dir,
            suppressed_job.id,
            supp_ts,
            supp_ts + 50,
            "suppressed",
            None,
            false,
            false,
            false,
        )
        .unwrap();
        record_schedule_skip(
            &dir,
            suppressed_job.id,
            supp_ts + 4_900,
            supp_ts + 5_000,
            false,
        )
        .unwrap();
        let after_supp_skip = get_schedule(&dir, "quiet-watch").unwrap().unwrap();
        assert_eq!(after_supp_skip.last_status.as_deref(), Some("suppressed"));
        assert_eq!(after_supp_skip.last_run_at, Some(supp_ts));
        assert_eq!(after_supp_skip.run_count, 1);
        assert!(after_supp_skip.enabled);

        // 4. Schedule with `last_status = Some("failed")`, `last_error = Some("boom")`,
        //    `consecutive_failures = 2` -> skip preserves `"failed"`, `Some("boom")`, `consecutive_failures = 2`.
        let failing_job = create_schedule(
            &dir,
            &NewSchedule {
                name: "flaky-watch".to_string(),
                schedule_spec: "every 5m".to_string(),
                prompt: "check".to_string(),
                profile: String::new(),
                script: "true".to_string(),
                no_agent: false,
                suppress_token: None,
                failure_deliver: FailureDeliver::Notify,
                max_runs: None,
                expires_at: None,
                next_run_at: next_run,
            },
            128,
        )
        .unwrap();
        let fail1_ts = now + 4_000;
        let fail2_ts = now + 5_000;
        assert_eq!(
            record_schedule_outcome(
                &dir,
                failing_job.id,
                fail1_ts,
                fail1_ts + 10,
                "failed",
                Some("first"),
                true,
                false,
                false,
            )
            .unwrap(),
            1
        );
        assert_eq!(
            record_schedule_outcome(
                &dir,
                failing_job.id,
                fail2_ts,
                fail2_ts + 10,
                "failed",
                Some("boom"),
                true,
                false,
                false,
            )
            .unwrap(),
            2
        );
        record_schedule_skip(
            &dir,
            failing_job.id,
            fail2_ts + 4_900,
            fail2_ts + 5_000,
            false,
        )
        .unwrap();
        let after_fail_skip = get_schedule(&dir, "flaky-watch").unwrap().unwrap();
        assert_eq!(after_fail_skip.last_status.as_deref(), Some("failed"));
        assert_eq!(after_fail_skip.last_error.as_deref(), Some("boom"));
        assert_eq!(after_fail_skip.last_run_at, Some(fail2_ts));
        assert_eq!(after_fail_skip.consecutive_failures, 2);
        assert_eq!(after_fail_skip.run_count, 2);

        // 5. One-shot skip (`force_disable = true`) -> disables schedule and clears
        //    `next_run_at` while keeping `run_count = 0`.
        let oneshot = create_schedule(
            &dir,
            &NewSchedule {
                name: "once-check".to_string(),
                schedule_spec: "in 5m".to_string(),
                prompt: "check once".to_string(),
                profile: String::new(),
                script: "true".to_string(),
                no_agent: false,
                suppress_token: None,
                failure_deliver: FailureDeliver::Notify,
                max_runs: None,
                expires_at: None,
                next_run_at: next_run,
            },
            128,
        )
        .unwrap();
        record_schedule_skip(&dir, oneshot.id, now + 9_000, now + 10_000, true).unwrap();
        let after_oneshot_skip = get_schedule(&dir, "once-check").unwrap().unwrap();
        assert!(!after_oneshot_skip.enabled);
        assert_eq!(after_oneshot_skip.next_run_at, None);
        assert_eq!(after_oneshot_skip.run_count, 0);
        assert_eq!(after_oneshot_skip.last_status.as_deref(), Some("skipped"));
        assert_eq!(after_oneshot_skip.last_run_at, None);

        // 6. One-shot collector self-deferral (`schedule edit --schedule "in 20m"`
        //    running during the collector after `started_at`) preserves `enabled = true`
        //    and the deferred `next_run_at`.
        let defer_job = create_schedule(
            &dir,
            &NewSchedule {
                name: "day-plan-checkin-midday".to_string(),
                schedule_spec: "at 2026-11-26 12:45".to_string(),
                prompt: "reassess".to_string(),
                profile: String::new(),
                script: "true".to_string(),
                no_agent: false,
                suppress_token: Some("HEARTBEAT_OK".to_string()),
                failure_deliver: FailureDeliver::Notify,
                max_runs: None,
                expires_at: None,
                next_run_at: next_run,
            },
            128,
        )
        .unwrap();
        let started_at = now + 20_000;
        advance_schedule_next_run(&dir, defer_job.id, None, started_at - 10).unwrap();
        let deferred_next = started_at + 20 * 60_000;
        advance_schedule_next_run(&dir, defer_job.id, Some(deferred_next), started_at + 100)
            .unwrap();
        record_schedule_skip(&dir, defer_job.id, started_at, started_at + 200, true).unwrap();
        let after_defer_skip = get_schedule(&dir, "day-plan-checkin-midday")
            .unwrap()
            .unwrap();
        assert!(after_defer_skip.enabled);
        assert_eq!(after_defer_skip.next_run_at, Some(deferred_next));
        assert_eq!(after_defer_skip.last_status.as_deref(), Some("skipped"));
        assert_eq!(after_defer_skip.run_count, 0);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
