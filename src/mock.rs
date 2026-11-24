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
    /// Answer `initialize` only after this long, like an agent slow to start.
    pub init_delay_ms: u64,
    /// Fail `initialize` with this message.
    pub init_error: Option<String>,
    /// Fail every `session/new` with this message, as an expired login does.
    pub session_error: Option<String>,
    /// Reject `session/set_config_option` once a session has completed a turn.
    pub reject_mid_session_config: bool,
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

fn default_execute_kind() -> String {
    "execute".to_string()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// An `agent_message_chunk`.
    Chunk(String),
    SleepMs(u64),
    /// A tool call that asks for permission. Rejected or cancelled, it ends
    /// the turn with no text, as agy does.
    Permission {
        command: String,
        #[serde(default)]
        summary: Option<String>,
        #[serde(default = "default_execute_kind")]
        kind: String,
    },
    /// A non-permission tool call (`tool_call` followed by completed `tool_call_update`).
    ToolCall {
        kind: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        summary: Option<String>,
    },
    /// "turn N in SESSION", for follow-up tests.
    TurnInfo,
    /// "model=M thought=T turn=N in SESSION", for profile tests.
    ConfigInfo,
    /// Echo a summary of non-text attachment blocks received on this prompt.
    AttachmentInfo,
    /// "you said: TEXT".
    Echo,
    /// Wait until another concurrent turn emits `Signal(name)`.
    WaitFor(String),
    /// Release any turn waiting on `WaitFor(name)` (latched).
    Signal(String),
    /// Exit at once with this status, mid-turn.
    Exit(i32),
    /// Write this line as it is (not JSON, for example).
    Raw(String),
    /// Write a line of this many bytes.
    Huge(usize),
    /// Send nothing more until cancelled.
    Hang,
    /// Ignore `session/cancel` completely (for wedged-session recovery tests).
    HangUninterruptible,
    /// End the turn with this stop reason.
    Stop(String),
    /// Answer the prompt with a JSON-RPC error.
    Error {
        code: i64,
        message: String,
    },
    /// Send a raw `session/update` payload.
    Update(Value),
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
    session_config: HashMap<String, HashMap<String, String>>,
    cancels: HashMap<String, Arc<Notify>>,
    waiting: HashMap<String, oneshot::Sender<Value>>,
    barriers: HashMap<String, (bool, Arc<Notify>)>,
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
        let line = match lines::read(&mut stdin, crate::acp::MAX_AGENT_LINE).await? {
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
                let delay = Duration::from_millis(self.script.init_delay_ms);
                let id = id.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    self.respond(
                        &id,
                        json!({
                            "protocolVersion": 1,
                            "agentCapabilities": {"loadSession": false, "sessionCapabilities": {"close": {}}},
                            "agentInfo": {"name": "mock-agent", "version": "0.0.0"},
                            "authMethods": [],
                        }),
                    );
                });
            }
            "session/new" => {
                if let Some(message) = &self.script.session_error {
                    self.log.line("new refused");
                    self.error(id, -32000, message);
                    return;
                }
                let session = format!("mock-{}", self.next_session.fetch_add(1, Ordering::Relaxed));
                {
                    let mut state = self.state.lock().unwrap();
                    state.sessions.insert(session.clone(), 0);
                    state.session_config.insert(
                        session.clone(),
                        HashMap::from([
                            ("model".to_string(), "mock-large".to_string()),
                            ("thought_level".to_string(), "high".to_string()),
                            ("mode".to_string(), "auto".to_string()),
                        ]),
                    );
                }
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
                let session = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let key = params.get("configId").and_then(Value::as_str).unwrap_or("");
                let value = params.get("value").and_then(Value::as_str).unwrap_or("");
                let rejected = {
                    let mut state = self.state.lock().unwrap();
                    let turns = state.sessions.get(session).copied().unwrap_or(0);
                    if self.script.reject_mid_session_config && turns > 0 {
                        true
                    } else {
                        if let Some(cfg) = state.session_config.get_mut(session) {
                            cfg.insert(key.to_string(), value.to_string());
                        }
                        false
                    }
                };
                if rejected {
                    self.log
                        .line(&format!("set-rejected {key}={value} ({session})"));
                    self.error(id, -32602, "mid-session config rejected");
                } else {
                    self.log.line(&format!("set {key}={value} ({session})"));
                    self.respond(id, json!({"configOptions": []}));
                }
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
        let blocks = params
            .get("prompt")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let text: String = blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect();
        let attach_summary: Vec<String> = blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("image") => {
                    let mime = b.get("mimeType").and_then(Value::as_str).unwrap_or("?");
                    let len = b
                        .get("data")
                        .and_then(Value::as_str)
                        .map(str::len)
                        .unwrap_or(0);
                    Some(format!("image:{mime}:b64={len}"))
                }
                Some("resource") => {
                    let res = b.get("resource")?;
                    let uri = res.get("uri").and_then(Value::as_str).unwrap_or("?");
                    let mime = res.get("mimeType").and_then(Value::as_str).unwrap_or("?");
                    let len = res
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::len)
                        .unwrap_or(0);
                    Some(format!("resource:{uri}:{mime}:len={len}"))
                }
                _ => None,
            })
            .collect();
        let attach_tag = if attach_summary.is_empty() {
            String::new()
        } else {
            format!(" [{}]", attach_summary.join(" "))
        };
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
        self.log
            .line(&format!("prompt {session} {text}{attach_tag}"));

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
                Step::ConfigInfo => {
                    let (model, thought) = {
                        let state = self.state.lock().unwrap();
                        let cfg = state.session_config.get(&session);
                        (
                            cfg.and_then(|c| c.get("model"))
                                .cloned()
                                .unwrap_or_default(),
                            cfg.and_then(|c| c.get("thought_level"))
                                .cloned()
                                .unwrap_or_default(),
                        )
                    };
                    self.chunk(
                        &session,
                        &format!("model={model} thought={thought} turn={turn} in {session}"),
                    );
                }
                Step::AttachmentInfo => {
                    self.chunk(&session, &format!("{text}{attach_tag}"));
                }
                Step::Echo => self.chunk(&session, &format!("you said: {text}")),
                Step::Signal(name) => {
                    let notify = {
                        let mut state = self.state.lock().unwrap();
                        let entry = state
                            .barriers
                            .entry(name)
                            .or_insert_with(|| (false, Arc::new(Notify::new())));
                        entry.0 = true;
                        Arc::clone(&entry.1)
                    };
                    notify.notify_waiters();
                }
                Step::WaitFor(name) => loop {
                    let (signaled, notify) = {
                        let mut state = self.state.lock().unwrap();
                        let entry = state
                            .barriers
                            .entry(name.clone())
                            .or_insert_with(|| (false, Arc::new(Notify::new())));
                        (entry.0, Arc::clone(&entry.1))
                    };
                    if signaled {
                        break;
                    }
                    tokio::select! {
                        () = notify.notified() => {}
                        () = cancel.notified() => return cancelled(),
                    }
                },
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
                Step::HangUninterruptible => {
                    std::future::pending::<()>().await;
                }
                Step::Permission {
                    command,
                    summary,
                    kind,
                } => {
                    let call = format!("call_{}", self.next_request.load(Ordering::Relaxed));
                    let mut input = if kind == "execute" {
                        json!({"CommandLine": command, "WaitMsBeforeAsync": 5000})
                    } else {
                        json!({"target": command})
                    };
                    if let Some(summary) = &summary {
                        input["toolSummary"] = json!(summary);
                    }
                    let tool_name = if kind == "execute" {
                        "run_command"
                    } else {
                        "tool_action"
                    };
                    let tool = json!({
                        "sessionUpdate": "tool_call",
                        "toolCallId": call,
                        "name": tool_name,
                        "args": input,
                        "status": "pending",
                        "kind": kind,
                        "title": command,
                        "rawInput": input,
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
                Step::ToolCall {
                    kind,
                    title,
                    summary,
                } => {
                    let call =
                        format!("call_{}", self.next_request.fetch_add(1, Ordering::Relaxed));
                    let mut tool = json!({
                        "sessionUpdate": "tool_call",
                        "toolCallId": call,
                        "status": "in_progress",
                        "kind": kind,
                    });
                    if let Some(title) = title {
                        tool["title"] = json!(title);
                    }
                    if let Some(summary) = summary {
                        tool["rawInput"] = json!({"toolSummary": summary});
                        tool["args"] = json!({"toolSummary": summary});
                    }
                    self.update(&session, tool);
                    self.update(
                        &session,
                        json!({"sessionUpdate": "tool_call_update", "toolCallId": call, "status": "completed"}),
                    );
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
                Step::Update(update) => self.update(&session, update),
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
