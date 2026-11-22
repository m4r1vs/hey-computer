//! A scripted stand-in for an ACP agent, for the integration tests.
//!
//! `hey-computer debug mock-agent SCRIPT [--log FILE]` speaks enough of the
//! protocol, in the shape agy 1.5 uses (string ids on its own requests,
//! `run_command` tool calls with a `CommandLine`, a turn that ends with no text
//! when a tool call is rejected), to drive the daemon through every path
//! without a model. Each prompt runs the steps of the first turn script whose
//! `match` occurs in the prompt text, or the `default` steps.
//!
//! ```json
//! {
//!   "turns": [
//!     {"match": "plain", "steps": [{"chunk": "Hello "}, {"chunk": "there."}]},
//!     {"match": "shell", "steps": [{"permission": {"command": "hyprctl version"}},
//!                                  {"chunk": "Hyprland 0.55.0"}]},
//!     {"match": "slow", "steps": ["hang"]}
//!   ],
//!   "default": ["echo"]
//! }
//! ```
//!
//! The log file records what the mock saw (`start`, `new`, `prompt`,
//! `permission allow_once`, `cancel`, …), one event per line, so a test can
//! check what the daemon did.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::lines::{self, Line};

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Script {
    /// Fail `initialize` with this message.
    pub init_error: Option<String>,
    /// Fail every `session/new` with this message, as an expired login does.
    pub session_error: Option<String>,
    pub turns: Vec<TurnScript>,
    pub default: Vec<Step>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnScript {
    #[serde(rename = "match")]
    pub pattern: String,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// An `agent_message_chunk`.
    Chunk(String),
    SleepMs(u64),
    /// A `run_command` tool call that asks for permission. Rejected or
    /// cancelled, it ends the turn with no text, as agy does.
    Permission {
        command: String,
    },
    /// "turn N in SESSION", for follow-up tests.
    TurnInfo,
    /// "you said: TEXT".
    Echo,
    /// Exit at once with this status, mid-turn.
    Exit(i32),
    /// Write this line as it is (not JSON, for example).
    Raw(String),
    /// Write a line of this many bytes.
    Huge(usize),
    /// Send nothing more until cancelled.
    Hang,
    /// End the turn with this stop reason.
    Stop(String),
    /// Answer the prompt with a JSON-RPC error.
    Error {
        code: i64,
        message: String,
    },
}

struct Logger(Option<PathBuf>);

impl Logger {
    fn line(&self, text: &str) {
        let Some(path) = &self.0 else {
            return;
        };
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{text}");
        }
    }
}

#[derive(Default)]
struct State {
    sessions: HashMap<String, u32>,
    cancels: HashMap<String, Arc<Notify>>,
    waiting: HashMap<String, oneshot::Sender<Value>>,
}

struct Mock {
    script: Script,
    log: Logger,
    out: mpsc::UnboundedSender<String>,
    state: Mutex<State>,
    next_session: AtomicU64,
    next_request: AtomicU64,
}

pub async fn run(script: &Path, log: Option<PathBuf>) -> Result<()> {
    let text =
        std::fs::read_to_string(script).with_context(|| format!("reading {}", script.display()))?;
    let script: Script =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", script.display()))?;

    let (out, mut lines_out) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = lines_out.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err()
                || stdout.write_all(b"\n").await.is_err()
                || stdout.flush().await.is_err()
            {
                return;
            }
        }
    });

    let mock = Arc::new(Mock {
        script,
        log: Logger(log),
        out,
        state: Mutex::new(State::default()),
        next_session: AtomicU64::new(1),
        next_request: AtomicU64::new(1),
    });
    mock.log.line("start");

    let mut stdin = BufReader::new(tokio::io::stdin());
    loop {
        let line = match lines::read(&mut stdin, 1 << 20).await? {
            Line::Text(line) => line,
            Line::Eof => break,
            Line::TooLong(_) | Line::NotUtf8 => continue,
        };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            mock.log.line("malformed input");
            continue;
        };
        Arc::clone(&mock).handle(message);
    }
    mock.log.line("stdin closed");
    drop(mock);
    let _ = writer.await;
    Ok(())
}

impl Mock {
    fn send(&self, message: Value) {
        let _ = self.out.send(message.to_string());
    }

    fn respond(&self, id: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn error(&self, id: &Value, code: i64, message: &str) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}));
    }

    fn update(&self, session: &str, update: Value) {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": session, "update": update},
        }));
    }

    fn handle(self: Arc<Self>, message: Value) {
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            (Some(method), Some(id)) => self.request(method, &id, params),
            (Some("session/cancel"), None) => {
                let session = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.log.line(&format!("cancel {session}"));
                if let Some(cancel) = self.state.lock().unwrap().cancels.get(session) {
                    cancel.notify_one();
                }
            }
            (Some(other), None) => self.log.line(&format!("notification {other}")),
            (None, Some(id)) => {
                let key = id
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| id.to_string());
                let waiter = self.state.lock().unwrap().waiting.remove(&key);
                if let Some(waiter) = waiter {
                    let _ = waiter.send(message.get("result").cloned().unwrap_or(Value::Null));
                }
            }
            (None, None) => {}
        }
    }

    fn request(self: Arc<Self>, method: &str, id: &Value, params: Value) {
        match method {
            "initialize" => {
                if let Some(message) = &self.script.init_error {
                    self.error(id, -32000, message);
                    return;
                }
                self.respond(
                    id,
                    json!({
                        "protocolVersion": 1,
                        "agentCapabilities": {"loadSession": false, "sessionCapabilities": {"close": {}}},
                        "agentInfo": {"name": "mock-agent", "version": "0.0.0"},
                        "authMethods": [],
                    }),
                );
            }
            "session/new" => {
                if let Some(message) = &self.script.session_error {
                    self.log.line("new refused");
                    self.error(id, -32000, message);
                    return;
                }
                let session = format!("mock-{}", self.next_session.fetch_add(1, Ordering::Relaxed));
                self.state
                    .lock()
                    .unwrap()
                    .sessions
                    .insert(session.clone(), 0);
                let extra = params
                    .get("additionalDirectories")
                    .map(|d| format!(" dirs={d}"))
                    .unwrap_or_default();
                self.log.line(&format!("new {session}{extra}"));
                self.respond(
                    id,
                    json!({
                        "sessionId": session,
                        "configOptions": [
                            {"id": "model", "currentValue": "mock-large"},
                            {"id": "thought_level", "currentValue": "high"},
                            {"id": "mode", "currentValue": "auto",
                             "options": [{"value": "default"}, {"value": "auto"}]},
                        ],
                    }),
                );
            }
            "session/set_config_option" => {
                let key = params.get("configId").and_then(Value::as_str).unwrap_or("");
                let value = params.get("value").and_then(Value::as_str).unwrap_or("");
                self.log.line(&format!("set {key}={value}"));
                self.respond(id, json!({"configOptions": []}));
            }
            "session/close" => {
                let session = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                self.log.line(&format!("close {session}"));
                self.respond(id, json!({}));
            }
            "session/prompt" => {
                let id = id.clone();
                tokio::spawn(async move { self.prompt(id, params).await });
            }
            other => self.error(id, -32601, &format!("{other} is not supported")),
        }
    }

    async fn prompt(self: Arc<Self>, id: Value, params: Value) {
        let session = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let text: String = params
            .get("prompt")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect()
            })
            .unwrap_or_default();
        let cancel = Arc::new(Notify::new());
        let turn = {
            let mut state = self.state.lock().unwrap();
            state.cancels.insert(session.clone(), Arc::clone(&cancel));
            match state.sessions.get_mut(&session) {
                Some(turns) => {
                    *turns += 1;
                    *turns
                }
                None => {
                    drop(state);
                    self.error(&id, -32602, "unknown session");
                    return;
                }
            }
        };
        self.log.line(&format!("prompt {session} {text}"));

        let steps = self
            .script
            .turns
            .iter()
            .find(|t| text.contains(&t.pattern))
            .map(|t| t.steps.clone())
            .unwrap_or_else(|| self.script.default.clone());

        // Updates can still arrive between the cancel and the response (agy
        // sent one); a late chunk makes sure the daemon does not carry it
        // into the next answer.
        let cancelled = || {
            self.log.line("turn cancelled");
            self.chunk(&session, " (late chunk of a cancelled turn)");
            self.respond(&id, json!({"stopReason": "cancelled"}));
        };

        for step in steps {
            match step {
                Step::Chunk(chunk) => self.chunk(&session, &chunk),
                Step::TurnInfo => self.chunk(&session, &format!("turn {turn} in {session}")),
                Step::Echo => self.chunk(&session, &format!("you said: {text}")),
                Step::SleepMs(ms) => {
                    tokio::select! {
                        () = tokio::time::sleep(Duration::from_millis(ms)) => {}
                        () = cancel.notified() => return cancelled(),
                    }
                }
                Step::Hang => {
                    cancel.notified().await;
                    return cancelled();
                }
                Step::Permission { command } => {
                    let call = format!("call_{}", self.next_request.load(Ordering::Relaxed));
                    let tool = json!({
                        "sessionUpdate": "tool_call",
                        "toolCallId": call,
                        "name": "run_command",
                        "args": {"CommandLine": command, "WaitMsBeforeAsync": 5000},
                        "status": "pending",
                        "kind": "execute",
                        "title": command,
                        "rawInput": {"CommandLine": command, "WaitMsBeforeAsync": 5000},
                    });
                    self.update(&session, tool.clone());
                    let request = self
                        .next_request
                        .fetch_add(1, Ordering::Relaxed)
                        .to_string();
                    let (tx, rx) = oneshot::channel();
                    self.state
                        .lock()
                        .unwrap()
                        .waiting
                        .insert(request.clone(), tx);
                    self.send(json!({
                        "jsonrpc": "2.0",
                        "id": request,
                        "method": "session/request_permission",
                        "params": {
                            "sessionId": session,
                            "toolCall": tool,
                            "options": [
                                {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                                {"optionId": "deny", "name": "Deny", "kind": "reject_once"},
                            ],
                        },
                    }));
                    let answer = tokio::select! {
                        answer = rx => answer.unwrap_or(Value::Null),
                        () = cancel.notified() => return cancelled(),
                    };
                    let choice = match answer.pointer("/outcome/optionId").and_then(Value::as_str) {
                        Some("allow") => "allow_once",
                        Some("deny") => "reject_once",
                        _ => "cancelled",
                    };
                    self.log.line(&format!("permission {choice} {command}"));
                    let status = if choice == "allow_once" {
                        "completed"
                    } else {
                        "failed"
                    };
                    self.update(
                        &session,
                        json!({"sessionUpdate": "tool_call_update", "toolCallId": call, "status": status}),
                    );
                    if choice != "allow_once" {
                        self.respond(&id, json!({"stopReason": "end_turn"}));
                        return;
                    }
                }
                Step::Exit(code) => {
                    self.log.line(&format!("exit {code}"));
                    std::process::exit(code);
                }
                Step::Raw(line) => {
                    let _ = self.out.send(line);
                }
                Step::Huge(size) => {
                    let _ = self.out.send("x".repeat(size));
                }
                Step::Stop(reason) => {
                    self.respond(&id, json!({"stopReason": reason}));
                    return;
                }
                Step::Error { code, message } => {
                    self.error(&id, code, &message);
                    return;
                }
            }
        }
        self.update(
            &session,
            json!({"sessionUpdate": "usage_update", "used": 1, "size": 2}),
        );
        self.respond(&id, json!({"stopReason": "end_turn"}));
    }

    fn chunk(&self, session: &str, text: &str) {
        self.update(
            session,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}),
        );
    }
}
