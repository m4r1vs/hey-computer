//! A small client for the Agent Client Protocol over a child's stdio.
//!
//! Newline-delimited JSON-RPC 2.0, protocol version 1. Hand-written on
//! purpose: agy sends fields outside the specification (`name`, `args` on
//! tool calls, snake_case twins of capability keys, string request ids), and
//! this only needs the handful of messages a one-shot assistant uses.
//!
//! The connection owns the child. Its stdout is read by one task that hands
//! responses to whoever is waiting for them and everything else (session
//! updates, permission requests, the end of the stream) to one channel, which
//! the conductor reads.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

use crate::lines::{self, Line};

/// The longest line read from the agent. A tool call can carry a whole file;
/// anything larger is skipped, not fatal.
pub const MAX_AGENT_LINE: usize = 32 * 1024 * 1024;

/// How much of the agent's stderr is kept to explain its death.
const STDERR_TAIL: usize = 4096;

/// Something the agent sent that is not the answer to one of our calls.
#[derive(Debug)]
pub enum Incoming {
    /// `session/update`.
    Update { session: String, update: Value },
    /// `session/request_permission`; answer it with [`Connection::respond`].
    Permission { id: Value, params: Value },
    /// The agent's stdout ended: it exited or is about to.
    Exited,
}

/// A JSON-RPC error object, as the agent sent it.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        match &self.data {
            Some(Value::String(detail)) if !detail.is_empty() => write!(f, ": {detail}"),
            Some(Value::Object(map)) => match map.get("message").or_else(|| map.get("details")) {
                Some(Value::String(detail)) if !detail.is_empty() => write!(f, ": {detail}"),
                _ => Ok(()),
            },
            _ => Ok(()),
        }
    }
}

#[derive(Debug)]
pub enum CallError {
    /// The agent answered with an error.
    Rpc(RpcError),
    /// The agent went away before answering.
    Closed,
    Timeout(Duration),
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc(e) => write!(f, "{e}"),
            Self::Closed => f.write_str("the agent exited"),
            Self::Timeout(t) => write!(f, "no answer within {t:?}"),
        }
    }
}

impl std::error::Error for CallError {}

/// How the agent ended: its exit status and the last lines of its stderr,
/// which are shown to the user but logged at debug only (they are agy's,
/// and nothing says what they may contain).
#[derive(Debug, Clone)]
pub struct Ended {
    pub status: String,
    pub tail: String,
}

impl fmt::Display for Ended {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.status)?;
        if !self.tail.is_empty() {
            write!(f, ": {}", self.tail)?;
        }
        Ok(())
    }
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, CallError>>>>>;

pub struct Connection {
    child: Child,
    /// The child leads its own process group, so the shells and tools it
    /// starts can be killed with it.
    pgid: Option<i32>,
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: AtomicU64,
    stderr: Arc<Mutex<String>>,
}

impl Connection {
    /// Start the agent and the tasks that read and write its stdio.
    pub fn spawn(argv: &[String], cwd: &Path) -> Result<(Self, mpsc::UnboundedReceiver<Incoming>)> {
        let (program, args) = argv.split_first().context("the agent command is empty")?;
        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {program}"))?;
        let pgid = child.id().and_then(|id| i32::try_from(id).ok());

        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take().context("no stderr")?;

        let (out, mut lines_out) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(line) = lines_out.recv().await {
                if tracing::enabled!(tracing::Level::TRACE) {
                    tracing::trace!("acp ← {}", redact_outbound_for_trace(&line));
                }
                let written = async {
                    stdin.write_all(line.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await
                };
                if let Err(e) = written.await {
                    tracing::debug!("cannot write to the agent: {e}");
                    return;
                }
            }
        });

        let tail = Arc::new(Mutex::new(String::new()));
        let stderr_tail = Arc::clone(&tail);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                tracing::debug!("agent stderr: {line}");
                if let Ok(mut tail) = stderr_tail.lock() {
                    tail.push_str(&line);
                    tail.push('\n');
                    if tail.len() > STDERR_TAIL {
                        let cut = tail.len() - STDERR_TAIL;
                        let cut = (cut..tail.len())
                            .find(|&i| tail.is_char_boundary(i))
                            .unwrap_or(tail.len());
                        tail.drain(..cut);
                    }
                }
            }
        });

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (events, incoming) = mpsc::unbounded_channel();
        tokio::spawn(read_loop(
            BufReader::new(stdout),
            Arc::clone(&pending),
            events,
            out.clone(),
        ));

        Ok((
            Self {
                child,
                pgid,
                out,
                pending,
                next_id: AtomicU64::new(1),
                stderr: tail,
            },
            incoming,
        ))
    }

    /// Send a request and get a receiver for its answer, so the caller can
    /// wait for it alongside the agent's other messages.
    pub fn start_call(
        &self,
        method: &str,
        params: Value,
    ) -> oneshot::Receiver<Result<Value, CallError>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(id, tx);
        }
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if self.out.send(line.to_string()).is_err()
            && let Ok(mut pending) = self.pending.lock()
            && let Some(tx) = pending.remove(&id)
        {
            let _ = tx.send(Err(CallError::Closed));
        }
        rx
    }

    /// Send a request and wait for its answer.
    pub async fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        let rx = self.start_call(method, params);
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(CallError::Closed),
            Err(_) => Err(CallError::Timeout(timeout)),
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        let line = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let _ = self.out.send(line.to_string());
    }

    /// Answer a request the agent made. The id goes back exactly as it came:
    /// agy numbers its requests with strings.
    pub fn respond(&self, id: Value, result: Value) {
        let line = json!({"jsonrpc": "2.0", "id": id, "result": result});
        let _ = self.out.send(line.to_string());
    }

    /// The last lines the agent wrote to stderr.
    pub fn stderr_tail(&self) -> String {
        self.stderr
            .lock()
            .map(|t| t.trim().to_string())
            .unwrap_or_default()
    }

    /// Why the agent is gone.
    pub async fn reap(mut self) -> Ended {
        let status = match tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await {
            Ok(Ok(status)) => status.to_string(),
            Ok(Err(e)) => format!("unknown exit ({e})"),
            Err(_) => {
                self.kill_group(libc::SIGKILL);
                "still running, killed".to_string()
            }
        };
        // The stderr reader may still be draining the last lines.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let tail = last_lines(&self.stderr_tail(), 3);
        Ended { status, tail }
    }

    /// Stop the agent: SIGTERM, then SIGKILL, each to the whole process
    /// group.
    pub async fn shutdown(mut self) {
        self.kill_group(libc::SIGTERM);
        if tokio::time::timeout(Duration::from_secs(3), self.child.wait())
            .await
            .is_err()
        {
            self.kill_group(libc::SIGKILL);
            let _ = self.child.wait().await;
        }
    }

    fn kill_group(&self, signal: i32) {
        if let Some(pgid) = self.pgid {
            // SAFETY: kill(2) with a negative pid signals a process group; it
            // touches no memory of ours.
            unsafe {
                libc::kill(-pgid, signal);
            }
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Whatever the agent started (a shell, a viewer it was asked to open
        // with `&`) is in its group; take the group down with it unless it
        // already exited cleanly.
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            self.kill_group(libc::SIGKILL);
        }
    }
}

fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join(" / ")
}

async fn read_loop<R: tokio::io::AsyncBufRead + Unpin>(
    mut reader: R,
    pending: Pending,
    events: mpsc::UnboundedSender<Incoming>,
    out: mpsc::UnboundedSender<String>,
) {
    loop {
        let line = match lines::read(&mut reader, MAX_AGENT_LINE).await {
            Ok(Line::Text(line)) => line,
            Ok(Line::Eof) => break,
            Ok(Line::TooLong(n)) => {
                tracing::warn!("skipped a {n}-byte line from the agent");
                continue;
            }
            Ok(Line::NotUtf8) => {
                tracing::warn!("skipped a line from the agent that is not UTF-8");
                continue;
            }
            Err(e) => {
                tracing::debug!("reading from the agent: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        tracing::trace!("acp → {line}");
        let message: Value = match serde_json::from_str(&line) {
            Ok(Value::Object(map)) => Value::Object(map),
            Ok(_) | Err(_) => {
                tracing::warn!(
                    "skipped a malformed line from the agent ({} bytes)",
                    line.len()
                );
                continue;
            }
        };
        dispatch(message, &pending, &events, &out);
    }

    // Nobody will answer the calls still waiting.
    if let Ok(mut pending) = pending.lock() {
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(CallError::Closed));
        }
    }
    let _ = events.send(Incoming::Exited);
}

fn dispatch(
    message: Value,
    pending: &Pending,
    events: &mpsc::UnboundedSender<Incoming>,
    out: &mpsc::UnboundedSender<String>,
) {
    let method = message.get("method").and_then(Value::as_str);
    let id = message.get("id").cloned().filter(|id| !id.is_null());

    match (method, id) {
        // A request from the agent.
        (Some(method), Some(id)) => {
            if method == "session/request_permission" {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let _ = events.send(Incoming::Permission { id, params });
            } else {
                // The client advertised no file system and no terminal, so
                // anything else is unexpected; say so instead of leaving the
                // agent waiting.
                tracing::debug!("refusing agent request {method}");
                let line = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": format!("{method} is not supported")},
                });
                let _ = out.send(line.to_string());
            }
        }
        // A notification.
        (Some(method), None) => {
            if method == "session/update"
                && let Some(params) = message.get("params")
            {
                let session = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let update = params.get("update").cloned().unwrap_or(Value::Null);
                let _ = events.send(Incoming::Update { session, update });
            }
        }
        // A response to one of our calls.
        (None, Some(id)) => {
            let Some(id) = id
                .as_u64()
                .or_else(|| id.as_str().and_then(|s| s.parse().ok()))
            else {
                tracing::debug!("a response with an unknown id: {id}");
                return;
            };
            let Some(tx) = pending.lock().ok().and_then(|mut p| p.remove(&id)) else {
                tracing::debug!("a response nobody waits for: {id}");
                return;
            };
            let result = match message.get("error") {
                Some(error) => Err(CallError::Rpc(RpcError {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_string(),
                    data: error.get("data").cloned(),
                })),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = tx.send(result);
        }
        (None, None) => tracing::debug!("skipped a message that is neither request nor response"),
    }
}

/// The text of an `agent_message_chunk` update.
pub fn chunk_text(update: &Value) -> Option<&str> {
    if update.get("sessionUpdate")?.as_str()? != "agent_message_chunk" {
        return None;
    }
    let content = update.get("content")?;
    if content.get("type").and_then(Value::as_str) != Some("text") {
        return None;
    }
    content.get("text")?.as_str()
}

/// The `toolCallId` on a `tool_call` update or `toolCall` object, if non-empty.
pub fn tool_call_id(value: &Value) -> Option<&str> {
    value
        .get("toolCallId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// Maximum characters in a live progress status line shown in the notification.
const PROGRESS_STATUS_CHARS: usize = 80;

/// Redact bulky attachment payloads and MCP secret values before logging an
/// outbound JSON-RPC line at `trace`.
fn redact_outbound_for_trace(line: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(line) else {
        return format!("<non-json {} bytes>", line.len());
    };
    if let Some(params) = value.get_mut("params").and_then(Value::as_object_mut) {
        if let Some(blocks) = params.get_mut("prompt").and_then(Value::as_array_mut) {
            for block in blocks {
                if let Some(obj) = block.as_object_mut() {
                    if let Some(Value::String(data)) = obj.get("data") {
                        let len = data.len();
                        obj.insert("data".to_string(), json!(format!("<redacted {len} bytes>")));
                    }
                    if let Some(res) = obj.get_mut("resource").and_then(Value::as_object_mut) {
                        for key in ["text", "blob"] {
                            if let Some(Value::String(s)) = res.get(key) {
                                let len = s.len();
                                res.insert(
                                    key.to_string(),
                                    json!(format!("<redacted {len} bytes>")),
                                );
                            }
                        }
                    }
                }
            }
        }
        if let Some(servers) = params.get_mut("mcpServers").and_then(Value::as_array_mut) {
            for server in servers {
                if let Some(obj) = server.as_object_mut() {
                    for list_key in ["env", "headers"] {
                        if let Some(items) = obj.get_mut(list_key).and_then(Value::as_array_mut) {
                            for item in items {
                                if let Some(entry) = item.as_object_mut()
                                    && entry.contains_key("value")
                                {
                                    entry.insert("value".to_string(), json!("<redacted>"));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    value.to_string()
}

/// The shell command on a `toolCall` object, if it is one.
///
/// Scoped to shell-execution tool calls (`kind == "execute"` or
/// `name == "run_command"`) so non-shell tools such as MCP tools that happen
/// to take a `command` parameter are never misparsed as shell commands.
pub fn tool_command(call: &Value) -> Option<String> {
    let is_execute = call.get("kind").and_then(Value::as_str) == Some("execute")
        || call.get("name").and_then(Value::as_str) == Some("run_command");
    if !is_execute {
        return None;
    }
    for key in ["rawInput", "args"] {
        if let Some(line) = call
            .get(key)
            .and_then(|input| input.get("CommandLine").or_else(|| input.get("command")))
        {
            match line {
                Value::String(s) => return Some(s.clone()),
                Value::Array(words) => {
                    let words: Vec<&str> = words.iter().filter_map(Value::as_str).collect();
                    return Some(words.join(" "));
                }
                _ => {}
            }
        }
    }
    // Without the raw input, an execute call's title is its command line.
    if call.get("kind").and_then(Value::as_str) == Some("execute") {
        return call.get("title").and_then(Value::as_str).map(String::from);
    }
    None
}

/// The shell command a permission request is about, if it is one.
pub fn permission_command(params: &Value) -> Option<String> {
    tool_command(params.get("toolCall")?)
}

/// A short, sanitized progress summary for a `tool_call`, `tool_call_update`,
/// or `request_permission` `toolCall` object. Never exposes raw shell arguments.
pub fn tool_progress(tool_call: &Value) -> Option<String> {
    let is_update =
        tool_call.get("sessionUpdate").and_then(Value::as_str) == Some("tool_call_update");
    if is_update
        && matches!(
            tool_call.get("status").and_then(Value::as_str),
            Some("completed" | "failed" | "cancelled")
        )
    {
        return None;
    }

    let summary = ["rawInput", "args"]
        .into_iter()
        .find_map(|key| {
            tool_call
                .get(key)
                .and_then(|input| input.get("toolSummary"))
                .and_then(Value::as_str)
        })
        .and_then(|s| sanitize_status(s, PROGRESS_STATUS_CHARS));
    if summary.is_some() {
        return summary;
    }

    let raw_title = tool_call.get("title").and_then(Value::as_str);
    if is_update && raw_title.is_none() {
        return None;
    }

    let kind = tool_call
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("other");
    if kind == "execute" {
        let label = tool_command(tool_call)
            .as_deref()
            .and_then(command_program)
            .map(|prog| format!("Running {prog}…"))
            .unwrap_or_else(|| "Running command…".to_string());
        return sanitize_status(&label, PROGRESS_STATUS_CHARS);
    }

    if let Some(title) = raw_title
        && let Some(clean) = sanitize_status(title, PROGRESS_STATUS_CHARS)
        && !clean.eq_ignore_ascii_case(kind)
    {
        return Some(clean);
    }

    if is_update {
        return None;
    }

    match kind {
        "read" => Some("Reading file…".to_string()),
        "edit" => Some("Editing file…".to_string()),
        "delete" => Some("Deleting file…".to_string()),
        "move" => Some("Moving file…".to_string()),
        "search" => Some("Searching…".to_string()),
        "fetch" => Some("Fetching page…".to_string()),
        "think" => None,
        _ => Some("Working…".to_string()),
    }
}

/// Extract a clean program basename from a command line, skipping leading
/// `KEY=value` assignments and `env` wrappers, and ignoring generic shells.
fn command_program(line: &str) -> Option<&str> {
    let mut words = line.split_whitespace();
    let mut skip_next = false;
    for raw in &mut words {
        if skip_next {
            skip_next = false;
            continue;
        }
        let word = raw.trim_matches(|c| matches!(c, '\'' | '"'));
        if word.is_empty() {
            continue;
        }
        if let Some((name, _)) = word.split_once('=')
            && !name.is_empty()
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            continue;
        }
        let base = word.rsplit('/').next().unwrap_or(word);
        if base == "env" {
            continue;
        }
        if base.starts_with('-') {
            if matches!(base, "-u" | "-C" | "-S") {
                skip_next = true;
            }
            continue;
        }
        if matches!(base, "sh" | "bash" | "zsh" | "dash" | "fish") {
            return None;
        }
        if base
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Some(base);
        }
        return None;
    }
    None
}

fn sanitize_status(text: &str, max_chars: usize) -> Option<String> {
    let mut out = String::with_capacity(text.len().min(max_chars));
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !out.is_empty() {
                pending_space = true;
            }
            continue;
        }
        if c.is_control() {
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
    }
    if out.is_empty() {
        return None;
    }
    if out.chars().count() <= max_chars {
        Some(out)
    } else {
        let mut cut: String = out.chars().take(max_chars.saturating_sub(1)).collect();
        cut.push('…');
        Some(cut)
    }
}

/// The answer to a permission request: the first option of `kind`, or a
/// cancelled outcome when the agent offered none.
pub fn permission_outcome(params: &Value, kinds: &[&str]) -> Value {
    let options = params
        .get("options")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for kind in kinds {
        if let Some(option) = options
            .iter()
            .find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))
            && let Some(id) = option.get("optionId")
        {
            return json!({"outcome": {"outcome": "selected", "optionId": id}});
        }
    }
    json!({"outcome": {"outcome": "cancelled"}})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A permission request as agy 1.5 sends it (values invented).
    fn agy_permission() -> Value {
        serde_json::from_str(
            r#"{"sessionId":"00000000-0000-4000-8000-000000000001","toolCall":{"sessionUpdate":"tool_call","toolCallId":"call_1","name":"run_command","args":{"CommandLine":"hyprctl version","WaitMsBeforeAsync":5000,"toolSummary":"Check the version"},"status":"pending","kind":"execute","title":"hyprctl version","rawInput":{"CommandLine":"hyprctl version","WaitMsBeforeAsync":5000,"toolSummary":"Check the version"}},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]}"#,
        )
        .unwrap()
    }

    #[test]
    fn finds_the_command_line_of_a_permission_request() {
        assert_eq!(
            permission_command(&agy_permission()).as_deref(),
            Some("hyprctl version")
        );
        let title_only = json!({"toolCall": {"kind": "execute", "title": "ls -l"}});
        assert_eq!(permission_command(&title_only).as_deref(), Some("ls -l"));
        let edit = json!({"toolCall": {"kind": "edit", "title": "Edit notes.md"}});
        assert_eq!(permission_command(&edit), None);
        // Non-execute tools (e.g. MCP) with a `command` parameter are not shell commands.
        let mcp = json!({
            "toolCall": {
                "kind": "other",
                "name": "mcp_demo_tool",
                "rawInput": {"command": "sudo something"}
            }
        });
        assert_eq!(permission_command(&mcp), None);
    }

    #[test]
    fn outbound_trace_redacts_attachments_and_mcp_secrets() {
        let prompt = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/prompt",
            "params": {
                "sessionId": "s1",
                "prompt": [
                    {"type": "text", "text": "describe"},
                    {"type": "image", "mimeType": "image/png", "data": "QUJDREVGRw=="},
                    {"type": "resource", "resource": {"uri": "file:///a.txt", "mimeType": "text/plain", "text": "secret file body"}},
                ]
            }
        })
        .to_string();
        let redacted = redact_outbound_for_trace(&prompt);
        assert!(!redacted.contains("QUJDREVGRw=="), "{redacted}");
        assert!(!redacted.contains("secret file body"), "{redacted}");
        assert!(redacted.contains("<redacted 12 bytes>"), "{redacted}");

        let session = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {
                "cwd": "/tmp",
                "mcpServers": [
                    {"name": "s", "command": "mcp", "env": [{"name": "TOKEN", "value": "top-secret"}]},
                    {"type": "http", "name": "h", "url": "https://example.test", "headers": [{"name": "Authorization", "value": "Bearer xyz"}]}
                ]
            }
        })
        .to_string();
        let redacted = redact_outbound_for_trace(&session);
        assert!(!redacted.contains("top-secret"), "{redacted}");
        assert!(!redacted.contains("Bearer xyz"), "{redacted}");
        assert!(redacted.contains("<redacted>"), "{redacted}");
    }

    #[test]
    fn picks_options_by_kind_not_by_id() {
        let params = agy_permission();
        assert_eq!(
            permission_outcome(&params, &["allow_once", "allow_always"]),
            json!({"outcome": {"outcome": "selected", "optionId": "allow"}})
        );
        assert_eq!(
            permission_outcome(&params, &["reject_once", "reject_always"]),
            json!({"outcome": {"outcome": "selected", "optionId": "deny"}})
        );
        assert_eq!(
            permission_outcome(&json!({"options": []}), &["allow_once"]),
            json!({"outcome": {"outcome": "cancelled"}})
        );
    }

    #[test]
    fn reads_message_chunks_only() {
        let chunk = json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "ok"}});
        assert_eq!(chunk_text(&chunk), Some("ok"));
        let thought = json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hm"}});
        assert_eq!(chunk_text(&thought), None);
    }

    #[test]
    fn extracts_non_empty_tool_call_ids() {
        let params = agy_permission();
        assert_eq!(tool_call_id(&params["toolCall"]), Some("call_1"));
        assert_eq!(tool_call_id(&json!({"toolCallId": ""})), None);
        assert_eq!(tool_call_id(&json!({"kind": "read"})), None);
    }

    #[test]
    fn formats_clean_tool_progress_summaries() {
        // Prefers toolSummary over raw CommandLine/title.
        let params = agy_permission();
        assert_eq!(
            tool_progress(&params["toolCall"]).as_deref(),
            Some("Check the version")
        );

        // Execute without toolSummary extracts only the program basename.
        assert_eq!(
            tool_progress(&json!({
                "sessionUpdate": "tool_call",
                "kind": "execute",
                "title": "hyprctl version"
            }))
            .as_deref(),
            Some("Running hyprctl…")
        );
        assert_eq!(
            tool_progress(&json!({
                "sessionUpdate": "tool_call",
                "kind": "execute",
                "rawInput": {"CommandLine": "FOO=1 /nix/store/abc/bin/curl -H 'Auth: secret' https://example.org"}
            }))
            .as_deref(),
            Some("Running curl…")
        );
        for shell in ["bash -lc 'echo hi'", "env sh -c 'date'"] {
            assert_eq!(
                tool_progress(&json!({
                    "sessionUpdate": "tool_call",
                    "kind": "execute",
                    "rawInput": {"CommandLine": shell}
                }))
                .as_deref(),
                Some("Running command…"),
                "{shell}"
            );
        }

        // Non-execute kinds use title or fall back to friendly labels.
        assert_eq!(
            tool_progress(
                &json!({"sessionUpdate": "tool_call", "kind": "edit", "title": "Edit notes.md"})
            )
            .as_deref(),
            Some("Edit notes.md")
        );
        assert_eq!(
            tool_progress(&json!({"sessionUpdate": "tool_call", "kind": "read"})).as_deref(),
            Some("Reading file…")
        );
        assert_eq!(
            tool_progress(&json!({"sessionUpdate": "tool_call", "kind": "search"})).as_deref(),
            Some("Searching…")
        );
        assert_eq!(
            tool_progress(&json!({"sessionUpdate": "tool_call", "kind": "think"})),
            None
        );

        // Collapses whitespace/control chars and truncates at 80 chars.
        let noisy = format!("Check\t\n\u{7}{}", "ä".repeat(90));
        let cleaned = tool_progress(&json!({
            "sessionUpdate": "tool_call",
            "kind": "read",
            "rawInput": {"toolSummary": noisy}
        }))
        .unwrap();
        assert_eq!(cleaned.chars().count(), PROGRESS_STATUS_CHARS);
        assert!(cleaned.starts_with("Check ä"), "{cleaned}");
        assert!(cleaned.ends_with('…'), "{cleaned}");

        // tool_call_update ignores terminal statuses and bare in_progress pings.
        assert_eq!(
            tool_progress(&json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_1",
                "status": "completed",
                "title": "Done"
            })),
            None
        );
        assert_eq!(
            tool_progress(&json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_1",
                "status": "in_progress"
            })),
            None
        );
        assert_eq!(
            tool_progress(&json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_1",
                "status": "in_progress",
                "rawInput": {"toolSummary": "Fetch forecast"}
            }))
            .as_deref(),
            Some("Fetch forecast")
        );
    }

    #[test]
    fn rpc_errors_carry_their_detail() {
        let error = RpcError {
            code: -32000,
            message: "Authentication required".to_string(),
            data: Some(json!({"message": "run agy to log in again"})),
        };
        assert_eq!(
            error.to_string(),
            "Authentication required: run agy to log in again"
        );
    }

    #[tokio::test]
    async fn dispatch_routes_responses_requests_and_updates() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending.lock().unwrap().insert(7, tx);
        let (events, mut incoming) = mpsc::unbounded_channel();
        let (out, mut written) = mpsc::unbounded_channel();

        dispatch(
            json!({"jsonrpc": "2.0", "id": 7, "result": {"stopReason": "end_turn"}}),
            &pending,
            &events,
            &out,
        );
        assert_eq!(rx.await.unwrap().unwrap()["stopReason"], "end_turn");

        dispatch(
            json!({"jsonrpc": "2.0", "id": "1", "method": "session/request_permission", "params": agy_permission()}),
            &pending,
            &events,
            &out,
        );
        match incoming.recv().await.unwrap() {
            Incoming::Permission { id, .. } => assert_eq!(id, json!("1")),
            other => panic!("{other:?}"),
        }

        dispatch(
            json!({"jsonrpc": "2.0", "id": 3, "method": "fs/read_text_file", "params": {}}),
            &pending,
            &events,
            &out,
        );
        let refusal: Value = serde_json::from_str(&written.recv().await.unwrap()).unwrap();
        assert_eq!(refusal["id"], 3);
        assert_eq!(refusal["error"]["code"], -32601);

        dispatch(
            json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s", "update": {"sessionUpdate": "usage_update"}}}),
            &pending,
            &events,
            &out,
        );
        match incoming.recv().await.unwrap() {
            Incoming::Update { session, .. } => assert_eq!(session, "s"),
            other => panic!("{other:?}"),
        }
    }
}
