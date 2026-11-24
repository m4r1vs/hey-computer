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
//! - `"new": true` on an ask starts a fresh conversation.
//! - `{"type":"cancel"}` → `{"type":"cancelled","id":1}` (`null` when nothing
//!   was running).
//! - `{"type":"status"}` → `{"type":"status",…}`.
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
    },
    Cancel {
        /// When omitted, cancels the active interactive request; when set,
        /// cancels the matching request/task id (including background tasks).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
    },
    Status,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    /// The request has a number and will run.
    Accepted {
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
    },
    Failed {
        id: u64,
        reason: FailReason,
        message: String,
    },
    /// The request that was cancelled, if one was running or waiting.
    Cancelled {
        id: Option<u64>,
    },
    Status(Status),
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Down,
    Starting,
    Ready,
}

impl std::fmt::Display for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Down => "down",
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
            },
            Request::Cancel { id: None },
            Request::Cancel { id: Some(9) },
            Request::Status,
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
            Reply::Answer {
                id: 3,
                text: "line one\nline two".to_string(),
                denied: vec![],
                suppressed: false,
            },
            Reply::Answer {
                id: 4,
                text: String::new(),
                denied: vec!["sudo true".to_string()],
                suppressed: true,
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
            }),
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
    }

    #[test]
    fn unknown_fields_and_types_are_refused() {
        assert!(serde_json::from_str::<Request>(r#"{"type":"ask","txt":"hi"}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"type":"reboot"}"#).is_err());
        assert!(serde_json::from_str::<Request>("ask hi").is_err());
    }
}
