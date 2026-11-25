//! Persistent state stored under `$XDG_STATE_HOME/hey-computer/`.
//!
//! Uses a single private SQLite database (`history.db`, WAL mode, mode `0600`)
//! with an external-content FTS5 index over completed non-empty turns. Shared
//! by `hey-computer last`, `hey-computer tasks`, `hey-computer recall`, and
//! future viewer history. Legacy `last.json` is imported once on daemon start
//! and still accepted as a read-only fallback when `history.db` does not exist.

use std::fmt;
use std::io::Write;
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
pub use crate::protocol::WidgetSpec;
use crate::protocol::{FailReason, InputSource};

pub const DB_FILE: &str = "history.db";
pub const LAST_FILE: &str = "last.json";
pub const SPEAK_STATE_FILE: &str = "speak_enabled";
pub const SCHEMA_VERSION: u32 = 1;
pub const DB_SCHEMA_VERSION: i32 = 4;

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
"#;

fn migrate(conn: &mut Connection) -> Result<()> {
    let cur: i32 = conn.query_row("PRAGMA user_version;", [], |r| r.get(0))?;
    if cur == DB_SCHEMA_VERSION {
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
                         WHERE status = 'completed' AND length(trim(answer)) > 0 AND lane = ?1
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
                         WHERE status = 'completed' AND length(trim(answer)) > 0
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
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecallHit {
    pub id: u64,
    pub lane: Lane,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile: String,
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

    let Some(raw_query) = raw_query else {
        let mut stmt = conn.prepare(
            "SELECT id, lane, profile, created_at, request, answer
             FROM turns
             WHERE status = 'completed'
               AND length(trim(answer)) > 0
               AND (?1 IS NULL OR lane = ?1)
               AND (?2 IS NULL OR created_at >= ?2)
               AND (?3 IS NULL OR created_at < ?3)
             ORDER BY created_at DESC, id DESC
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![lane_param, opts.since_ms, opts.until_ms, limit],
            |row| {
                let id: i64 = row.get(0)?;
                let lane_str: String = row.get(1)?;
                let profile: String = row.get(2)?;
                let created_at_ms: i64 = row.get(3)?;
                let request: String = row.get(4)?;
                let answer: String = row.get(5)?;
                Ok(RecallHit {
                    id: id.max(0) as u64,
                    lane: Lane::parse(&lane_str).unwrap_or(Lane::Interactive),
                    profile,
                    created_at_ms,
                    created_local: format_local_timestamp(created_at_ms),
                    req_snippet: truncate_chars(&request, 120),
                    ans_snippet: truncate_chars(&answer, 240),
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
             t.id, t.lane, t.profile, t.created_at, t.request, t.answer,
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
         ORDER BY {order}
         LIMIT ?6"
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
                limit
            ],
            |row| {
                let id: i64 = row.get(0)?;
                let lane_str: String = row.get(1)?;
                let profile: String = row.get(2)?;
                let created_at_ms: i64 = row.get(3)?;
                let request: String = row.get(4)?;
                let answer: String = row.get(5)?;
                let req_snippet: String = row.get(6)?;
                let ans_snippet: String = row.get(7)?;
                let score: f64 = row.get(8)?;
                Ok(RecallHit {
                    id: id.max(0) as u64,
                    lane: Lane::parse(&lane_str).unwrap_or(Lane::Interactive),
                    profile,
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
        let profile_part = if hit.profile.is_empty() {
            String::new()
        } else {
            format!(" profile={}", hit.profile)
        };
        body.push_str(&format!(
            "[#{} {} lane={}{}]\n",
            hit.id, hit.created_local, hit.lane, profile_part
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
                     WHEN ?7 = 1 OR (?6 = 0 AND max_runs IS NOT NULL AND run_count + 1 >= max_runs)
                     THEN 0
                     ELSE enabled
                 END,
                 next_run_at = CASE
                     WHEN ?7 = 1 OR (?6 = 0 AND max_runs IS NOT NULL AND run_count + 1 >= max_runs)
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
}
