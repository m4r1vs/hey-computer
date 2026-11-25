//! The protocol spoken over `$XDG_RUNTIME_DIR/hey-computer.sock`.
//!
//! One JSON object per line in each direction. A client sends one request
//! and reads replies until the daemon closes the connection:
//!
//! - `{"type":"ask","text":"…"}` → `{"type":"accepted","id":1}`, and the
//!   daemon presents the answer itself.
//! - `{"type":"ask","text":"…","wait":true}` → `accepted`, then one
//!   `answer` or `failed` line; the daemon presents nothing. Closing the
//!   connection before that cancels the request, and so does shutting down
//!   only the writing side (as `socat` and `nc -N` do at the end of input):
//!   a waiting client keeps its side open until the answer arrives.
//! - `{"type":"ask","text":"…","wait":true,"stream":true,"reply_to":7}` →
//!   `accepted`, zero or more `progress` / `chunk` / `widget` / `reset` lines,
//!   and a terminal `answer` or `failed` line.
//! - `"new": true` on an ask starts a fresh conversation.
//! - `{"type":"cancel"}` (or `{"type":"cancel","id":7}`) →
//!   `{"type":"cancelled","id":1}` (`null` when nothing matched).
//! - `{"type":"status"}` → `{"type":"status",…}`.
//! - `{"type":"speak","action":"toggle"}` → `{"type":"speak_status",…}`.
//! - `{"type":"read_aloud","text":"…"}` → `{"type":"speak_started","id":…}`,
//!   then `{"type":"speak_finished","id":…}` when playback completes or is
//!   stopped; closing the connection stops speech immediately.
//! - `{"type":"schedule_reload"}` → `{"type":"ok","message":"…"}` (wakes the
//!   daemon scheduler after a CLI schedule edit).
//! - `{"type":"schedule_run","target":"…"}` → `{"type":"ok","message":"…"}`
//!   (triggers an immediate test run of a scheduled job).
//!
//! A line the daemon cannot use gets `{"type":"error","message":"…"}`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The longest request line the daemon reads. A spoken request is a few
/// hundred bytes; this only bounds a broken or hostile client.
pub const MAX_REQUEST_LINE: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    File,
    Image,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub kind: AttachmentKind,
    pub path: PathBuf,
}

/// Structured widget card or custom interactive HTML/SVG surface captured from
/// `hey-computer mcp-ui` (`show_widget` / `propose_widget_template`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WidgetSpec {
    pub id: String,
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub data: serde_json::Value,
}

/// Where an `ask` request originated; used by automatic routing and voice-only
/// spoken TTS responses.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputSource {
    #[default]
    Cli,
    Voice,
    Typed,
}

impl InputSource {
    fn is_default(&self) -> bool {
        *self == Self::Cli
    }
}

/// Control action for runtime spoken-answer (`present.speak`) state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeakAction {
    On,
    Off,
    Toggle,
    Status,
}

/// Action applied when resuming a persisted notification draft checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftAction {
    Copy,
    Paste,
    Edit,
    Dismiss,
}

impl DraftAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Paste => "paste",
            Self::Edit => "edit",
            Self::Dismiss => "dismiss",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "copy" => Some(Self::Copy),
            "paste" => Some(Self::Paste),
            "edit" => Some(Self::Edit),
            "dismiss" => Some(Self::Dismiss),
            _ => None,
        }
    }
}

impl std::fmt::Display for DraftAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Ask {
        text: String,
        /// Wait for the answer on this connection instead of having the
        /// daemon present it.
        #[serde(default)]
        wait: bool,
        /// Start a new conversation even inside the follow-up window.
        #[serde(default)]
        new: bool,
        /// Optional named profile from `[profiles.<name>]` (or `"default"`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
        /// Run in the background lane (not superseded by newer interactive asks).
        #[serde(default, skip_serializing_if = "is_false")]
        background: bool,
        /// Suppress presentation/stdout when the trimmed answer equals this token.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        suppress_token: Option<String>,
        /// Optional file or image attachments (absolute paths read by the daemon).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
        /// Origin of the request (`cli`, `voice`, or `typed`).
        #[serde(default, skip_serializing_if = "InputSource::is_default")]
        source: InputSource,
        /// Continue the conversation of an earlier turn ID (reusing the live
        /// ACP session when still open, or seeding a fresh session from
        /// `history.db` when closed).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<u64>,
        /// Stream incremental `progress`, `chunk`, `widget`, and `reset` lines
        /// before the final `answer` / `failed` line (implies `wait = true`).
        #[serde(default, skip_serializing_if = "is_false")]
        stream: bool,
        /// Tag the turn with `origin = "distill"` so `hey-computer distill`
        /// never re-distills its own review runs.
        #[serde(default, skip_serializing_if = "is_false")]
        distill: bool,
        /// Optional parent schedule ID linking a CLI-triggered background turn
        /// (such as `hey-computer distill -b` run from a collector script) to
        /// its originating row in `schedules`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schedule_id: Option<u64>,
    },
    Cancel {
        /// When omitted, cancels the active interactive request (or ongoing TTS
        /// speech); when set, cancels the matching request/task id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
    },
    Status,
    /// Query or toggle spoken answers (`present.speak`) in the running daemon.
    Speak {
        action: SpeakAction,
    },
    /// Speak arbitrary Markdown or plain text on demand (bypassing
    /// `speak_enabled` and short-answer cutoffs). Streams `SpeakStarted`, then
    /// `SpeakFinished`; closing the connection stops speech immediately.
    ReadAloud {
        text: String,
    },
    /// Wake the daemon's scheduler after a CLI mutation to `schedules`.
    ScheduleReload,
    /// Trigger an immediate test run of a scheduled job by numeric id or name.
    ScheduleRun {
        target: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    /// The request has a number and will run.
    Accepted {
        id: u64,
    },
    /// Streamed text delta (`stream: true` connections only).
    Chunk {
        id: u64,
        text: String,
    },
    /// Streamed tool/progress summary (`stream: true` connections only).
    Progress {
        id: u64,
        status: String,
    },
    /// Streamed widget payload captured from `mcp-ui` (`stream: true` only).
    Widget {
        id: u64,
        widget: WidgetSpec,
    },
    /// Discard previously streamed chunks/widgets for `id` (e.g. Tier 2 retry).
    Reset {
        id: u64,
    },
    Answer {
        id: u64,
        text: String,
        /// Commands the agent asked for and was refused.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        denied: Vec<String>,
        /// True when the answer matched `suppress_token`.
        #[serde(default, skip_serializing_if = "is_false")]
        suppressed: bool,
        /// Structured widgets captured from `mcp-ui` during this turn.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        widgets: Vec<WidgetSpec>,
        /// Root conversation ID for follow-up linkage.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation_id: Option<u64>,
        /// Effective profile name (when non-default).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
    Failed {
        id: u64,
        reason: FailReason,
        message: String,
    },
    /// The request that was cancelled, if one was running, waiting, or speaking.
    Cancelled {
        id: Option<u64>,
    },
    Status(Status),
    SpeakStatus {
        enabled: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        speaking: bool,
    },
    /// On-demand `ReadAloud` playback started with speech handle `id`.
    SpeakStarted {
        id: u64,
    },
    /// On-demand `ReadAloud` playback finished or was stopped.
    SpeakFinished {
        id: u64,
    },
    /// Generic acknowledgement for control requests (`schedule_reload`, `schedule_run`).
    Ok {
        message: String,
    },
    Error {
        message: String,
    },
}

/// Why a request produced no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailReason {
    /// `cancel`, or the waiting client went away.
    Cancelled,
    /// A newer request replaced it.
    Superseded,
    /// The agent went quiet for longer than the inactivity timeout, or
    /// exceeded the turn wall-clock limit.
    Timeout,
    /// The agent exceeded a per-turn budget (such as `agent.max_tool_calls`).
    LimitReached,
    /// The agent could not be started, or exited mid-request.
    AgentDown,
    /// `agent.wait_for_keyring` is enabled and the login keyring is still locked.
    KeyringLocked,
    /// The agent refused the request; the message is what it said.
    AgentError,
    /// The only thing the agent tried was refused by the deny list.
    Denied,
    /// The agent finished without saying anything.
    Empty,
}

impl FailReason {
    /// A headline for a notification.
    pub fn headline(self) -> &'static str {
        match self {
            Self::Cancelled => "Request cancelled",
            Self::Superseded => "Request replaced",
            Self::Timeout => "Request timed out",
            Self::LimitReached => "Limit reached",
            Self::AgentDown => "The assistant is not running",
            Self::KeyringLocked => "Keyring is locked",
            Self::AgentError => "The assistant refused the request",
            Self::Denied => "Command refused",
            Self::Empty => "No answer",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// `down`, `starting` or `ready`.
    pub agent: AgentState,
    /// The request being worked on.
    pub running: Option<u64>,
    /// Seconds it has been running.
    pub running_secs: Option<f64>,
    /// Profile of the currently running request, if non-default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_profile: Option<String>,
    /// Seconds a follow-up may still continue the current conversation.
    pub follow_up_secs: Option<f64>,
    /// Requests answered or failed since the daemon started.
    pub handled: u64,
    /// Background tasks currently executing.
    #[serde(default)]
    pub background_running: usize,
    /// Background tasks waiting in the FIFO queue.
    #[serde(default)]
    pub background_queued: usize,
    /// Whether spoken voice answers (`present.speak`) are currently enabled.
    #[serde(default, skip_serializing_if = "is_false")]
    pub speak: bool,
    /// Whether TTS audio is actively playing right now.
    #[serde(default, skip_serializing_if = "is_false")]
    pub speaking: bool,
    /// IDs of scheduled jobs currently executing (collector script or agent turn).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub running_schedules: Vec<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Down,
    /// Waiting for `org.freedesktop.secrets` default collection to unlock
    /// before starting the ACP agent (`agent.wait_for_keyring = true`).
    Locked,
    Starting,
    Ready,
}

impl std::fmt::Display for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Down => "down",
            Self::Locked => "locked",
            Self::Starting => "starting",
            Self::Ready => "ready",
        })
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "agent {}", self.agent)?;
        match (self.running, self.running_secs) {
            (Some(id), Some(secs)) => {
                if let Some(profile) = &self.running_profile {
                    write!(f, ", request {id} ({profile}) running for {secs:.1} s")?;
                } else {
                    write!(f, ", request {id} running for {secs:.1} s")?;
                }
            }
            (Some(id), None) => write!(f, ", request {id} waiting")?,
            _ => f.write_str(", idle")?,
        }
        if let Some(secs) = self.follow_up_secs {
            write!(f, ", follow-ups for {secs:.0} s")?;
        }
        if self.background_running > 0 {
            write!(f, ", {} background running", self.background_running)?;
        }
        if self.background_queued > 0 {
            write!(f, ", {} queued", self.background_queued)?;
        }
        if self.speaking {
            f.write_str(", speaking")?;
        } else if self.speak {
            f.write_str(", speak on")?;
        }
        write!(f, ", {} handled", self.handled)
    }
}

/// One line of the protocol, without the newline.
pub fn encode<T: Serialize>(message: &T) -> String {
    // Serialising these types cannot fail: no maps with non-string keys.
    serde_json::to_string(message).unwrap_or_else(|e| {
        format!(
            r#"{{"type":"error","message":"cannot encode a reply: {}"}}"#,
            e.to_string().replace('"', "'")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_defaults_to_not_waiting() {
        let request: Request = serde_json::from_str(r#"{"type":"ask","text":"hi"}"#).unwrap();
        assert_eq!(
            request,
            Request::Ask {
                text: "hi".to_string(),
                wait: false,
                new: false,
                profile: None,
                background: false,
                suppress_token: None,
                attachments: vec![],
                source: InputSource::Cli,
                reply_to: None,
                stream: false,
                distill: false,
                schedule_id: None,
            }
        );
        let cancel: Request = serde_json::from_str(r#"{"type":"cancel"}"#).unwrap();
        assert_eq!(cancel, Request::Cancel { id: None });
        assert_eq!(encode(&cancel), r#"{"type":"cancel"}"#);
        let cancel_id: Request = serde_json::from_str(r#"{"type":"cancel","id":5}"#).unwrap();
        assert_eq!(cancel_id, Request::Cancel { id: Some(5) });
    }

    #[test]
    fn requests_round_trip() {
        for request in [
            Request::Ask {
                text: "Wie spät ist es?".to_string(),
                wait: true,
                new: true,
                profile: Some("deep".to_string()),
                background: true,
                suppress_token: Some("HEARTBEAT_OK".to_string()),
                attachments: vec![Attachment {
                    kind: AttachmentKind::Image,
                    path: PathBuf::from("/tmp/shot.png"),
                }],
                source: InputSource::Voice,
                reply_to: Some(42),
                stream: true,
                distill: false,
                schedule_id: Some(7),
            },
            Request::Cancel { id: None },
            Request::Cancel { id: Some(9) },
            Request::Status,
            Request::Speak {
                action: SpeakAction::Toggle,
            },
            Request::ReadAloud {
                text: "# Morning Brief\n\nClear skies today.".to_string(),
            },
            Request::ScheduleReload,
            Request::ScheduleRun {
                target: "ci-watch".to_string(),
            },
        ] {
            let line = encode(&request);
            assert!(!line.contains('\n'));
            assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), request);
        }
    }

    #[test]
    fn replies_round_trip() {
        for reply in [
            Reply::Accepted { id: 3 },
            Reply::Chunk {
                id: 3,
                text: "delta".to_string(),
            },
            Reply::Progress {
                id: 3,
                status: "Reading file".to_string(),
            },
            Reply::Widget {
                id: 3,
                widget: WidgetSpec {
                    id: "w1".to_string(),
                    kind: "custom_html".to_string(),
                    title: "Demo".to_string(),
                    data: serde_json::json!({"html": "<p>hi</p>"}),
                },
            },
            Reply::Reset { id: 3 },
            Reply::Answer {
                id: 3,
                text: "line one\nline two".to_string(),
                denied: vec![],
                suppressed: false,
                widgets: vec![],
                conversation_id: Some(3),
                profile: None,
            },
            Reply::Answer {
                id: 4,
                text: String::new(),
                denied: vec!["sudo true".to_string()],
                suppressed: true,
                widgets: vec![],
                conversation_id: None,
                profile: Some("deep".to_string()),
            },
            Reply::Failed {
                id: 5,
                reason: FailReason::Timeout,
                message: "no update for 45 s".to_string(),
            },
            Reply::Failed {
                id: 6,
                reason: FailReason::LimitReached,
                message: "too many tool calls".to_string(),
            },
            Reply::Cancelled { id: None },
            Reply::Status(Status {
                agent: AgentState::Ready,
                running: Some(2),
                running_secs: Some(1.5),
                running_profile: Some("deep".to_string()),
                follow_up_secs: None,
                handled: 1,
                background_running: 1,
                background_queued: 2,
                speak: true,
                speaking: false,
                running_schedules: vec![3, 9],
            }),
            Reply::SpeakStatus {
                enabled: true,
                speaking: true,
            },
            Reply::SpeakStarted { id: 1_000_000_001 },
            Reply::SpeakFinished { id: 1_000_000_001 },
            Reply::Error {
                message: "bad".to_string(),
            },
        ] {
            let line = encode(&reply);
            assert!(!line.contains('\n'), "{line}");
            assert_eq!(serde_json::from_str::<Reply>(&line).unwrap(), reply);
        }
    }

    #[test]
    fn the_wire_format_is_stable() {
        assert_eq!(
            encode(&Reply::Failed {
                id: 1,
                reason: FailReason::AgentDown,
                message: "m".to_string()
            }),
            r#"{"type":"failed","id":1,"reason":"agent_down","message":"m"}"#
        );
        assert_eq!(
            encode(&Reply::Cancelled { id: Some(2) }),
            r#"{"type":"cancelled","id":2}"#
        );
        assert_eq!(
            encode(&Reply::Failed {
                id: 3,
                reason: FailReason::LimitReached,
                message: "limit".to_string(),
            }),
            r#"{"type":"failed","id":3,"reason":"limit_reached","message":"limit"}"#
        );
        assert_eq!(
            encode(&Request::ReadAloud {
                text: "Hello".to_string(),
            }),
            r#"{"type":"read_aloud","text":"Hello"}"#
        );
        assert_eq!(
            encode(&Reply::SpeakStarted { id: 11 }),
            r#"{"type":"speak_started","id":11}"#
        );
        assert_eq!(
            encode(&Reply::SpeakFinished { id: 11 }),
            r#"{"type":"speak_finished","id":11}"#
        );
        assert_eq!(
            encode(&Reply::Failed {
                id: 12,
                reason: FailReason::KeyringLocked,
                message: "locked".to_string(),
            }),
            r#"{"type":"failed","id":12,"reason":"keyring_locked","message":"locked"}"#
        );
        assert_eq!(FailReason::KeyringLocked.headline(), "Keyring is locked");
        assert_eq!(AgentState::Locked.to_string(), "locked");
        assert_eq!(encode(&AgentState::Locked), r#""locked""#);
        for action in [
            DraftAction::Copy,
            DraftAction::Paste,
            DraftAction::Edit,
            DraftAction::Dismiss,
        ] {
            let encoded = encode(&action);
            assert_eq!(
                serde_json::from_str::<DraftAction>(&encoded).unwrap(),
                action
            );
            assert_eq!(DraftAction::parse(action.as_str()), Some(action));
        }
    }

    #[test]
    fn unknown_fields_and_types_are_refused() {
        assert!(serde_json::from_str::<Request>(r#"{"type":"ask","txt":"hi"}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"type":"reboot"}"#).is_err());
        assert!(serde_json::from_str::<Request>("ask hi").is_err());
    }
}
