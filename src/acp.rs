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

/// Events routed to one subscribed ACP session.
#[derive(Debug)]
pub enum SessionEvent {
    /// `session/update` for this session.
    Update(Value),
    /// `session/request_permission` for this session; answer via [`Client::respond`].
    Permission { id: Value, params: Value },
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
type SessionMap = Arc<Mutex<HashMap<String, (u64, mpsc::UnboundedSender<SessionEvent>)>>>;

/// RAII subscription to one ACP session's `session/update` and
/// `session/request_permission` stream. Dropping it unregisters the session
/// so late updates from a cancelled turn are discarded before the next turn
/// subscribes.
pub struct SessionSubscription {
    session: String,
    token: u64,
    rx: mpsc::UnboundedReceiver<SessionEvent>,
    sessions: SessionMap,
}

impl SessionSubscription {
    pub async fn recv(&mut self) -> Option<SessionEvent> {
        self.rx.recv().await
    }
}

impl Drop for SessionSubscription {
    fn drop(&mut self) {
        if let Ok(mut map) = self.sessions.lock()
            && map
                .get(&self.session)
                .is_some_and(|(tok, _)| *tok == self.token)
        {
            map.remove(&self.session);
        }
    }
}

/// Cloneable JSON-RPC handle shared across concurrent lanes on one agent process.
#[derive(Clone)]
pub struct Client {
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
    sessions: SessionMap,
    next_sub: Arc<AtomicU64>,
}

impl Client {
    /// Subscribe to all updates and permission requests for `session`.
    pub fn subscribe(&self, session: &str) -> SessionSubscription {
        let token = self.next_sub.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        if let Ok(mut map) = self.sessions.lock() {
            map.insert(session.to_string(), (token, tx));
        }
        SessionSubscription {
            session: session.to_string(),
            token,
            rx,
            sessions: Arc::clone(&self.sessions),
        }
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
}

pub struct Connection {
    child: Child,
    /// The child leads its own process group, so the shells and tools it
    /// starts can be killed with it.
    pgid: Option<i32>,
    client: Client,
    stderr: Arc<Mutex<String>>,
}

impl Connection {
    /// Start the agent and the tasks that read and write its stdio.
    /// Returns the connection owner and a one-shot fired when stdout reaches EOF.
    pub fn spawn(
        argv: &[String],
        cwd: &Path,
        state_dir: &Path,
    ) -> Result<(Self, oneshot::Receiver<()>)> {
        let (program, args) = argv.split_first().context("the agent command is empty")?;
        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .env("HEY_COMPUTER_STATE_DIR", state_dir)
            .env("HEY_COMPUTER_ARTIFACTS_DIR", state_dir.join("artifacts"))
            .env("HEY_COMPUTER_AGENT_TURN", "1")
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
        tokio::spawn(read_stderr_lossy(BufReader::new(stderr), stderr_tail));

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let sessions: SessionMap = Arc::new(Mutex::new(HashMap::new()));
        let (exited_tx, exited_rx) = oneshot::channel();
        tokio::spawn(read_loop(
            BufReader::new(stdout),
            Arc::clone(&pending),
            Arc::clone(&sessions),
            exited_tx,
            out.clone(),
        ));

        let client = Client {
            out,
            pending,
            next_id: Arc::new(AtomicU64::new(1)),
            sessions,
            next_sub: Arc::new(AtomicU64::new(1)),
        };

        Ok((
            Self {
                child,
                pgid,
                client,
                stderr: tail,
            },
            exited_rx,
        ))
    }

    pub fn client(&self) -> Client {
        self.client.clone()
    }

    pub fn start_call(
        &self,
        method: &str,
        params: Value,
    ) -> oneshot::Receiver<Result<Value, CallError>> {
        self.client.start_call(method, params)
    }

    pub async fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        self.client.call(method, params, timeout).await
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.client.notify(method, params);
    }

    pub fn respond(&self, id: Value, result: Value) {
        self.client.respond(id, result);
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
        // Clean up any surviving stdio MCP children or backgrounded subprocesses
        // in the agent's process group (while any member lives, the kernel holds
        // the PGID so it cannot be reused).
        self.kill_group(libc::SIGKILL);
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

/// Drain the child's `stderr` until EOF without ever closing the pipe on
/// non-UTF-8 bytes or buffering unbounded lines without `\n`.
async fn read_stderr_lossy<R: tokio::io::AsyncBufRead + Unpin>(
    mut reader: R,
    stderr_tail: Arc<Mutex<String>>,
) {
    const MAX_STDERR_LINE: usize = STDERR_TAIL / 4;
    let mut line = Vec::new();
    let mut discarding = false;
    loop {
        let (consumed, done) = match reader.fill_buf().await {
            Ok([]) => (0, true),
            Ok(buf) => {
                if let Some(nl) = buf.iter().position(|&b| b == b'\n' || b == b'\r') {
                    let slice = &buf[..nl];
                    if !discarding {
                        let room = MAX_STDERR_LINE.saturating_sub(line.len());
                        line.extend_from_slice(&slice[..slice.len().min(room)]);
                    }
                    if !line.is_empty() {
                        let text = String::from_utf8_lossy(&line);
                        tracing::debug!("agent stderr: {text}");
                        if let Ok(mut tail) = stderr_tail.lock() {
                            tail.push_str(&text);
                            tail.push('\n');
                            if tail.len() > STDERR_TAIL {
                                let cut = tail.len() - STDERR_TAIL;
                                let cut = (cut..tail.len())
                                    .find(|&i| tail.is_char_boundary(i))
                                    .unwrap_or(tail.len());
                                tail.drain(..cut);
                            }
                        }
                        line.clear();
                    }
                    discarding = false;
                    (nl + 1, false)
                } else {
                    if !discarding {
                        let room = MAX_STDERR_LINE.saturating_sub(line.len());
                        let take = buf.len().min(room);
                        line.extend_from_slice(&buf[..take]);
                        if take < buf.len() {
                            discarding = true;
                        }
                    }
                    (buf.len(), false)
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if consumed > 0 {
            reader.consume(consumed);
        }
        if done {
            if !line.is_empty() {
                let text = String::from_utf8_lossy(&line);
                tracing::debug!("agent stderr: {text}");
                if let Ok(mut tail) = stderr_tail.lock() {
                    tail.push_str(&text);
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
            break;
        }
    }
}

async fn read_loop<R: tokio::io::AsyncBufRead + Unpin>(
    mut reader: R,
    pending: Pending,
    sessions: SessionMap,
    exited: oneshot::Sender<()>,
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
        dispatch(message, &pending, &sessions, &out);
    }

    // Nobody will answer the calls still waiting, and every active turn wakes
    // up immediately when its session channel closes.
    if let Ok(mut pending) = pending.lock() {
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(CallError::Closed));
        }
    }
    if let Ok(mut map) = sessions.lock() {
        map.clear();
    }
    let _ = exited.send(());
}

fn dispatch(
    message: Value,
    pending: &Pending,
    sessions: &SessionMap,
    out: &mpsc::UnboundedSender<String>,
) {
    let method = message.get("method").and_then(Value::as_str);
    let id = message.get("id").cloned().filter(|id| !id.is_null());

    match (method, id) {
        // A request from the agent.
        (Some(method), Some(id)) => {
            if method == "session/request_permission" {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let session = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let delivered = sessions
                    .lock()
                    .ok()
                    .and_then(|map| map.get(session).map(|(_, tx)| tx.clone()))
                    .is_some_and(|tx| {
                        tx.send(SessionEvent::Permission {
                            id: id.clone(),
                            params,
                        })
                        .is_ok()
                    });
                if !delivered {
                    // Nothing is subscribed to this session; refuse cleanly instead of hanging agy.
                    let line = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"outcome": {"outcome": "cancelled"}},
                    });
                    let _ = out.send(line.to_string());
                }
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
                    .unwrap_or_default();
                let update = params.get("update").cloned().unwrap_or(Value::Null);
                if let Some(tx) = sessions
                    .lock()
                    .ok()
                    .and_then(|map| map.get(session).map(|(_, tx)| tx.clone()))
                {
                    let _ = tx.send(SessionEvent::Update(update));
                }
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

/// Whether `name` is one of `agy`'s lazy MCP dispatch tools.
pub fn is_lazy_mcp_tool(name: Option<&str>) -> bool {
    matches!(
        name,
        Some("call_mcp_tool" | "list_resources" | "read_resource")
    )
}

/// Extract the target MCP server name from a lazy MCP call (`call_mcp_tool`,
/// `list_resources`, or `read_resource`), if any.
pub fn mcp_server_name(call: &Value) -> Option<String> {
    if !is_lazy_mcp_tool(call.get("name").and_then(Value::as_str)) {
        return None;
    }
    ["rawInput", "args"].into_iter().find_map(|key| {
        call.get(key)
            .and_then(|input| input.get("ServerName").or_else(|| input.get("serverName")))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
    })
}

/// Extract the target MCP tool name from a lazy `call_mcp_tool` call, if any.
pub fn mcp_tool_name(call: &Value) -> Option<String> {
    if call.get("name").and_then(Value::as_str) != Some("call_mcp_tool") {
        return None;
    }
    ["rawInput", "args"].into_iter().find_map(|key| {
        call.get(key)
            .and_then(|input| input.get("ToolName").or_else(|| input.get("toolName")))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
    })
}

/// Resolve an ACP MCP tool call (lazy `call_mcp_tool`/`list_resources`/`read_resource`
/// or eager `mcp_<server>_<tool>`) into `(server_name, optional_tool_name)`.
pub fn resolve_mcp_call(
    call: &Value,
    known_servers: &[String],
) -> Option<(String, Option<String>)> {
    let name = call.get("name").and_then(Value::as_str)?;
    if is_lazy_mcp_tool(Some(name)) {
        let server = mcp_server_name(call)?;
        let tool =
            mcp_tool_name(call).or_else(|| (name != "call_mcp_tool").then(|| name.to_string()));
        return Some((server, tool));
    }
    if let Some(rest) = name.strip_prefix("mcp_") {
        let ui_prefix = format!("{}_", crate::mcp::INTERNAL_UI_MCP_SERVER);
        if let Some(tool) = rest.strip_prefix(&ui_prefix) {
            return Some((
                crate::mcp::INTERNAL_UI_MCP_SERVER.to_string(),
                (!tool.is_empty()).then(|| tool.to_string()),
            ));
        }
        let subagent_prefix = format!("{}_", crate::mcp::INTERNAL_SUBAGENT_MCP_SERVER);
        if let Some(tool) = rest.strip_prefix(&subagent_prefix) {
            return Some((
                crate::mcp::INTERNAL_SUBAGENT_MCP_SERVER.to_string(),
                (!tool.is_empty()).then(|| tool.to_string()),
            ));
        }
        if let Some(server) = known_servers
            .iter()
            .filter(|s| rest.starts_with(&format!("{s}_")))
            .max_by_key(|s| s.len())
        {
            let suffix = &rest[server.len() + 1..];
            return Some((
                server.clone(),
                (!suffix.is_empty()).then(|| suffix.to_string()),
            ));
        }
        if let Some((server, tool)) = rest.split_once('_')
            && !server.is_empty()
            && !tool.is_empty()
        {
            return Some((server.to_string(), Some(tool.to_string())));
        }
    }
    None
}

/// Extracted `mcp-ui` tool invocation (`show_widget` or `propose_widget_template`),
/// supporting both lazy `call_mcp_tool` and eager `mcp_mcp-ui_*` forms.
#[derive(Debug, Clone, PartialEq)]
pub struct McpUiCall {
    pub tool_call_id: Option<String>,
    pub tool_name: String,
    pub arguments: Option<Value>,
}

/// Extract `(tool_call_id, tool_name, arguments)` when `call` targets the
/// internal `mcp-ui` server (`show_widget` or `propose_widget_template`).
pub fn extract_mcp_ui_call(call: &Value) -> Option<McpUiCall> {
    let tc_id = tool_call_id(call).map(String::from);
    let name = call.get("name").and_then(Value::as_str);
    if name == Some("call_mcp_tool") {
        if mcp_server_name(call).as_deref() != Some(crate::mcp::INTERNAL_UI_MCP_SERVER) {
            return None;
        }
        let tool_name = mcp_tool_name(call)?;
        if !matches!(
            tool_name.as_str(),
            "show_widget" | "propose_widget_template"
        ) {
            return None;
        }
        let arguments = extract_mcp_ui_arguments(call, true);
        return Some(McpUiCall {
            tool_call_id: tc_id,
            tool_name,
            arguments,
        });
    }
    let prefix = format!("mcp_{}_", crate::mcp::INTERNAL_UI_MCP_SERVER);
    let tool_name = name?.strip_prefix(&prefix)?;
    if !matches!(tool_name, "show_widget" | "propose_widget_template") {
        return None;
    }
    let arguments = extract_mcp_ui_arguments(call, false);
    Some(McpUiCall {
        tool_call_id: tc_id,
        tool_name: tool_name.to_string(),
        arguments,
    })
}

/// Extract JSON object arguments from a lazy or eager `mcp-ui` call/update.
pub fn extract_mcp_ui_arguments(call: &Value, is_lazy: bool) -> Option<Value> {
    for key in ["rawInput", "args"] {
        let Some(input) = call.get(key) else {
            continue;
        };
        if is_lazy {
            if let Some(raw_args) = input.get("Arguments").or_else(|| input.get("arguments"))
                && let Some(parsed) = normalize_json_object_arg(raw_args)
            {
                return Some(parsed);
            }
        } else {
            if let Some(obj) = input.as_object()
                && !obj.is_empty()
            {
                if (obj.contains_key("Arguments") || obj.contains_key("arguments"))
                    && !obj.contains_key("kind")
                    && !obj.contains_key("skill_name")
                    && !obj.contains_key("skillName")
                    && let Some(inner) = obj
                        .get("Arguments")
                        .or_else(|| obj.get("arguments"))
                        .and_then(normalize_json_object_arg)
                {
                    return Some(inner);
                }
                return Some(Value::Object(obj.clone()));
            }
            if let Some(parsed) = normalize_json_object_arg(input) {
                return Some(parsed);
            }
        }
    }
    None
}

fn normalize_json_object_arg(val: &Value) -> Option<Value> {
    match val {
        Value::Object(map) if !map.is_empty() => Some(Value::Object(map.clone())),
        Value::String(s) => serde_json::from_str::<Value>(s)
            .ok()
            .filter(|v| v.is_object() && !v.as_object().is_none_or(serde_json::Map::is_empty)),
        _ => None,
    }
}

fn mcp_ui_progress_label(tool_name: &str, args: Option<&Value>) -> String {
    let title = args.and_then(|a| {
        a.get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| {
                if tool_name == "propose_widget_template" {
                    a.get("skill_name")
                        .or_else(|| a.get("skillName"))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(|name| format!("Skill Template · {name}"))
                } else {
                    None
                }
            })
    });
    match title {
        Some(t) => format!("Render widget: {t}"),
        None => "Render widget…".to_string(),
    }
}

/// Extracted `subagents` tool invocation (`delegate`), supporting both lazy
/// `call_mcp_tool` and eager `mcp_subagents_*` forms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSubagentCall {
    pub tool_call_id: Option<String>,
    pub tool_name: String,
    pub role: Option<String>,
}

/// Extract `(tool_call_id, tool_name, role)` when `call` targets the internal
/// `subagents` server (`delegate`).
pub fn extract_mcp_subagent_call(call: &Value) -> Option<McpSubagentCall> {
    let tc_id = tool_call_id(call).map(String::from);
    let name = call.get("name").and_then(Value::as_str);
    if name == Some("call_mcp_tool") {
        if mcp_server_name(call).as_deref() != Some(crate::mcp::INTERNAL_SUBAGENT_MCP_SERVER) {
            return None;
        }
        let tool_name = mcp_tool_name(call)?;
        let args = extract_mcp_ui_arguments(call, true);
        let role = args
            .as_ref()
            .and_then(|a| a.get("role"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        return Some(McpSubagentCall {
            tool_call_id: tc_id,
            tool_name,
            role,
        });
    }
    let prefix = format!("mcp_{}_", crate::mcp::INTERNAL_SUBAGENT_MCP_SERVER);
    let tool_name = name?.strip_prefix(&prefix)?;
    let args = extract_mcp_ui_arguments(call, false);
    let role = args
        .as_ref()
        .and_then(|a| a.get("role"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    Some(McpSubagentCall {
        tool_call_id: tc_id,
        tool_name: tool_name.to_string(),
        role,
    })
}

/// The shell command on a `toolCall` object, if it is one.
///
/// Scoped to shell-execution tool calls (`kind == "execute"` or
/// `name == "run_command"`) and explicitly excludes lazy MCP dispatch tools so
/// MCP calls are never misparsed as shell commands.
pub fn tool_command(call: &Value) -> Option<String> {
    let name = call.get("name").and_then(Value::as_str);
    if is_lazy_mcp_tool(name) {
        return None;
    }
    let is_execute =
        call.get("kind").and_then(Value::as_str) == Some("execute") || name == Some("run_command");
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

    if let Some(ui) = extract_mcp_ui_call(tool_call) {
        if is_update && ui.arguments.is_none() {
            return None;
        }
        let label = mcp_ui_progress_label(&ui.tool_name, ui.arguments.as_ref());
        return sanitize_status(&label, PROGRESS_STATUS_CHARS);
    }

    if let Some(sub) = extract_mcp_subagent_call(tool_call) {
        if is_update && sub.role.is_none() {
            return None;
        }
        let label = match sub.role {
            Some(role) => format!("Delegating: {role}…"),
            None => "Delegating…".to_string(),
        };
        return sanitize_status(&label, PROGRESS_STATUS_CHARS);
    }

    let raw_title = tool_call.get("title").and_then(Value::as_str);
    if is_update && raw_title.is_none() {
        return None;
    }

    if tool_call.get("name").and_then(Value::as_str) == Some("call_mcp_tool") {
        let label = match (mcp_server_name(tool_call), mcp_tool_name(tool_call)) {
            (Some(server), Some(tool)) => format!("{server}: {tool}"),
            (Some(server), None) => format!("MCP: {server}"),
            (None, Some(tool)) => format!("MCP: {tool}"),
            (None, None) => raw_title.unwrap_or("Calling MCP tool…").to_string(),
        };
        return sanitize_status(&label, PROGRESS_STATUS_CHARS);
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
        let sessions: SessionMap = Arc::new(Mutex::new(HashMap::new()));
        let (s1_tx, mut s1_rx) = mpsc::unbounded_channel();
        let (s2_tx, mut s2_rx) = mpsc::unbounded_channel();
        {
            let mut map = sessions.lock().unwrap();
            map.insert(
                "00000000-0000-4000-8000-000000000001".to_string(),
                (1, s1_tx),
            );
            map.insert("s2".to_string(), (2, s2_tx));
        }
        let (out, mut written) = mpsc::unbounded_channel();

        dispatch(
            json!({"jsonrpc": "2.0", "id": 7, "result": {"stopReason": "end_turn"}}),
            &pending,
            &sessions,
            &out,
        );
        assert_eq!(rx.await.unwrap().unwrap()["stopReason"], "end_turn");

        dispatch(
            json!({"jsonrpc": "2.0", "id": "1", "method": "session/request_permission", "params": agy_permission()}),
            &pending,
            &sessions,
            &out,
        );
        match s1_rx.recv().await.unwrap() {
            SessionEvent::Permission { id, .. } => assert_eq!(id, json!("1")),
            other => panic!("{other:?}"),
        }

        // Unsubscribed permission requests receive an immediate cancelled outcome.
        dispatch(
            json!({"jsonrpc": "2.0", "id": "9", "method": "session/request_permission", "params": {"sessionId": "orphan"}}),
            &pending,
            &sessions,
            &out,
        );
        let cancelled: Value = serde_json::from_str(&written.recv().await.unwrap()).unwrap();
        assert_eq!(cancelled["id"], "9");
        assert_eq!(cancelled["result"]["outcome"]["outcome"], "cancelled");

        dispatch(
            json!({"jsonrpc": "2.0", "id": 3, "method": "fs/read_text_file", "params": {}}),
            &pending,
            &sessions,
            &out,
        );
        let refusal: Value = serde_json::from_str(&written.recv().await.unwrap()).unwrap();
        assert_eq!(refusal["id"], 3);
        assert_eq!(refusal["error"]["code"], -32601);

        dispatch(
            json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s2", "update": {"sessionUpdate": "usage_update"}}}),
            &pending,
            &sessions,
            &out,
        );
        match s2_rx.recv().await.unwrap() {
            SessionEvent::Update(u) => assert_eq!(u["sessionUpdate"], "usage_update"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn lazy_call_mcp_tool_is_not_parsed_as_shell_command_and_formats_clean_progress() {
        let call = json!({
            "sessionUpdate": "tool_call",
            "name": "call_mcp_tool",
            "kind": "execute",
            "title": "MCP Tool conversations_history",
            "rawInput": {
                "ServerName": "slack",
                "ToolName": "conversations_history",
                "Arguments": {"channel_id": "C123", "command": "sudo rm -rf /"}
            }
        });
        assert_eq!(tool_command(&call), None);
        assert_eq!(mcp_server_name(&call).as_deref(), Some("slack"));
        assert_eq!(
            mcp_tool_name(&call).as_deref(),
            Some("conversations_history")
        );
        assert_eq!(
            tool_progress(&call).as_deref(),
            Some("slack: conversations_history")
        );

        let res_call = json!({
            "name": "read_resource",
            "kind": "read",
            "rawInput": {"ServerName": "gitlab", "Uri": "gitlab://proj"}
        });
        assert!(is_lazy_mcp_tool(Some("list_resources")));
        assert!(is_lazy_mcp_tool(Some("read_resource")));
        assert_eq!(mcp_server_name(&res_call).as_deref(), Some("gitlab"));

        // Both lazy `call_mcp_tool` and eager `mcp_mcp-ui_*` extract cleanly and
        // format `Render widget: <title>` progress labels.
        let lazy_ui = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "tc_ui_1",
            "name": "call_mcp_tool",
            "kind": "execute",
            "rawInput": {
                "ServerName": "mcp-ui",
                "ToolName": "show_widget",
                "Arguments": {
                    "kind": "transit_board",
                    "title": "U3 Borgweg",
                    "data": {"departures": []}
                }
            }
        });
        let extracted_lazy = extract_mcp_ui_call(&lazy_ui).unwrap();
        assert_eq!(extracted_lazy.tool_call_id.as_deref(), Some("tc_ui_1"));
        assert_eq!(extracted_lazy.tool_name, "show_widget");
        assert_eq!(
            extracted_lazy.arguments.as_ref().unwrap()["title"],
            "U3 Borgweg"
        );
        assert_eq!(
            tool_progress(&lazy_ui).as_deref(),
            Some("Render widget: U3 Borgweg")
        );

        let eager_ui = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "tc_ui_2",
            "name": "mcp_mcp-ui_propose_widget_template",
            "kind": "other",
            "args": {
                "skill_name": "widget-ci-status",
                "description": "CI card",
                "slots": ["branch"],
                "skill_markdown": "# CI",
                "template_html": "<div>{{branch}}</div>"
            }
        });
        let extracted_eager = extract_mcp_ui_call(&eager_ui).unwrap();
        assert_eq!(extracted_eager.tool_call_id.as_deref(), Some("tc_ui_2"));
        assert_eq!(extracted_eager.tool_name, "propose_widget_template");
        assert_eq!(
            tool_progress(&eager_ui).as_deref(),
            Some("Render widget: Skill Template · widget-ci-status")
        );

        let lazy_sub = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "tc_sub_1",
            "name": "call_mcp_tool",
            "kind": "execute",
            "rawInput": {
                "ServerName": "subagents",
                "ToolName": "delegate",
                "Arguments": {
                    "role": "Personal Comms",
                    "prompt": "Check personal inbox"
                }
            }
        });
        let extracted_sub = extract_mcp_subagent_call(&lazy_sub).unwrap();
        assert_eq!(extracted_sub.tool_call_id.as_deref(), Some("tc_sub_1"));
        assert_eq!(extracted_sub.tool_name, "delegate");
        assert_eq!(extracted_sub.role.as_deref(), Some("Personal Comms"));
        assert_eq!(
            tool_progress(&lazy_sub).as_deref(),
            Some("Delegating: Personal Comms…")
        );

        let eager_sub = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "tc_sub_2",
            "name": "mcp_subagents_delegate",
            "kind": "other",
            "args": {"role": "Work Triage", "prompt": "Check work"}
        });
        assert_eq!(
            tool_progress(&eager_sub).as_deref(),
            Some("Delegating: Work Triage…")
        );
    }

    #[tokio::test]
    async fn stderr_reader_survives_non_utf8_and_oversized_lines() {
        let mut payload = Vec::new();
        payload.extend_from_slice(b"first line\n");
        payload.extend_from_slice(b"bad utf8: \xff\xfe ok\n");
        payload.extend_from_slice(&vec![b'x'; STDERR_TAIL * 4]);
        payload.push(b'\r');
        payload.extend_from_slice(b"final tail line\n");

        let tail = Arc::new(Mutex::new(String::new()));
        read_stderr_lossy(BufReader::new(&payload[..]), Arc::clone(&tail)).await;
        let got = tail.lock().unwrap().clone();
        assert!(got.contains("bad utf8:"), "{got:?}");
        assert!(got.contains("final tail line"), "{got:?}");
        assert!(got.len() <= STDERR_TAIL);
    }

    #[test]
    fn resolves_lazy_and_eager_mcp_calls_with_longest_server_prefix() {
        let known = vec![
            "my".to_string(),
            "my_server".to_string(),
            "google-workspace".to_string(),
        ];

        assert_eq!(
            resolve_mcp_call(
                &json!({
                    "name": "call_mcp_tool",
                    "rawInput": {"ServerName": "slack", "ToolName": "post_message"}
                }),
                &known
            ),
            Some(("slack".to_string(), Some("post_message".to_string())))
        );
        assert_eq!(
            resolve_mcp_call(
                &json!({
                    "name": "read_resource",
                    "rawInput": {"ServerName": "gitlab", "Uri": "gitlab://proj"}
                }),
                &known
            ),
            Some(("gitlab".to_string(), Some("read_resource".to_string())))
        );
        // Argument-less lazy stub returns None.
        assert_eq!(
            resolve_mcp_call(&json!({"name": "call_mcp_tool"}), &known),
            None
        );
        // Eager internal mcp-ui.
        assert_eq!(
            resolve_mcp_call(&json!({"name": "mcp_mcp-ui_show_widget"}), &known),
            Some(("mcp-ui".to_string(), Some("show_widget".to_string())))
        );
        // Longest server name wins for underscored server names.
        assert_eq!(
            resolve_mcp_call(&json!({"name": "mcp_my_server_list_items"}), &known),
            Some(("my_server".to_string(), Some("list_items".to_string())))
        );
        // Non-MCP tool returns None.
        assert_eq!(
            resolve_mcp_call(&json!({"name": "run_command"}), &known),
            None
        );
    }
}
