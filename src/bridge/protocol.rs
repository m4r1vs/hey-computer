//! Read-only JSON-lines socket protocol for `hey-computer-bridge`.
//!
//! Only three requests exist (`subscribe`, `status`, `get_context`). Any
//! unknown or mutating request (such as `send`, `react`, `read`, or `typing`)
//! is rejected with `"bridge is strictly read-only"`.

use serde::{Deserialize, Serialize};

/// Wire protocol version reported in `ready` and `status` replies.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum UTF-8 line length on the bridge socket and adapter streams.
pub const MAX_BRIDGE_LINE: usize = 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Whatsapp,
    Signal,
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Whatsapp => f.write_str("whatsapp"),
            Self::Signal => f.write_str("signal"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformState {
    Disabled,
    Starting,
    Connected,
    Reconnecting,
    AuthRequired,
}

impl std::fmt::Display for PlatformState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => f.write_str("disabled"),
            Self::Starting => f.write_str("starting"),
            Self::Connected => f.write_str("connected"),
            Self::Reconnecting => f.write_str("reconnecting"),
            Self::AuthRequired => f.write_str("auth_required"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    DirectMessage,
    SelfChat,
    GroupMessage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAuthor {
    Me,
    Them,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextMessage {
    pub author: ContextAuthor,
    pub sender_name: String,
    pub timestamp_ms: i64,
    pub text: String,
}

/// Strictly read-only requests accepted on `$XDG_RUNTIME_DIR/hey-computer-bridge.sock`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum BridgeRequest {
    Subscribe,
    Status,
    GetContext {
        #[serde(default)]
        platform: Option<Platform>,
        sender_or_chat: String,
        #[serde(default)]
        limit: Option<usize>,
    },
}

/// Events and replies emitted by `hey-computer-bridge`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BridgeEvent {
    Ready {
        version: u32,
        read_only: bool,
        whatsapp: PlatformState,
        signal: PlatformState,
    },
    Status {
        version: u32,
        read_only: bool,
        whatsapp: PlatformState,
        signal: PlatformState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        whatsapp_detail: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal_detail: Option<String>,
        tracked_chats: usize,
    },
    Context {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        platform: Option<Platform>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chat_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sender_name: Option<String>,
        messages: Vec<ContextMessage>,
    },
    Message {
        id: String,
        platform: Platform,
        kind: MessageKind,
        chat_id: String,
        sender_id: String,
        sender_name: String,
        timestamp_ms: i64,
        text: String,
        quoted_text: Option<String>,
        media_placeholder: Option<String>,
        recent_messages: Vec<ContextMessage>,
    },
    UserReplied {
        platform: Platform,
        chat_id: String,
        timestamp_ms: i64,
    },
    PlatformState {
        platform: Platform,
        state: PlatformState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    Error {
        message: String,
    },
}

/// Serialize a request or event to a single JSON line without a trailing newline.
pub fn encode<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("bridge protocol values always serialize to JSON")
}

/// Parse a client request line, returning a read-only refusal for any
/// unsupported request type or unknown field (including extra keys on unit
/// variants, which serde's `deny_unknown_fields` does not catch on internally
/// tagged enums).
pub fn parse_request(line: &str) -> Result<BridgeRequest, String> {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
        return Err("invalid request: bridge is strictly read-only".to_string());
    };
    let Some(obj) = val.as_object() else {
        return Err("invalid request: bridge is strictly read-only".to_string());
    };
    let Some(kind) = obj.get("type").and_then(|v| v.as_str()) else {
        return Err("bridge is strictly read-only".to_string());
    };
    match kind {
        "subscribe" | "status" => {
            if obj.len() != 1 {
                return Err("bridge is strictly read-only".to_string());
            }
        }
        "get_context" => {
            if !obj
                .keys()
                .all(|k| matches!(k.as_str(), "type" | "platform" | "sender_or_chat" | "limit"))
            {
                return Err("bridge is strictly read-only".to_string());
            }
        }
        _ => return Err("bridge is strictly read-only".to_string()),
    }
    serde_json::from_value::<BridgeRequest>(val)
        .map_err(|_| "invalid request: bridge is strictly read-only".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_read_only_requests_and_rejects_outbound_requests() {
        assert_eq!(
            parse_request(r#"{"type":"subscribe"}"#).unwrap(),
            BridgeRequest::Subscribe
        );
        assert_eq!(
            parse_request(r#"{"type":"status"}"#).unwrap(),
            BridgeRequest::Status
        );
        assert_eq!(
            parse_request(
                r#"{"type":"get_context","platform":"signal","sender_or_chat":"Alice Test"}"#
            )
            .unwrap(),
            BridgeRequest::GetContext {
                platform: Some(Platform::Signal),
                sender_or_chat: "Alice Test".to_string(),
                limit: None,
            }
        );

        let err =
            parse_request(r#"{"type":"send","to":"+4915100000001","text":"hi"}"#).unwrap_err();
        assert_eq!(err, "bridge is strictly read-only");

        let err = parse_request(r#"{"type":"subscribe","send_receipts":true}"#).unwrap_err();
        assert!(err.contains("bridge is strictly read-only"));
    }
}
