//! MCP server configuration for `<agent_dir>/config/mcp_config.json`.
//!
//! `agy acp` 1.5.13 enables lazy MCP tool loading (`call_mcp_tool`, keeping
//! full schemas on disk instead of inline in the prompt) only for servers
//! configured in `<agent_dir>/config/mcp_config.json`; servers passed in
//! `session/new` `mcpServers` are declared eagerly and silently truncated when
//! their combined schemas exceed the eager budget.
//!
//! Secret environment variables (`env_files`) and HTTP headers (`header_files`)
//! are read from disk at prepare time and written only to the private `0600`
//! `mcp_config.json`, so credentials never appear in `config.toml` or
//! `/nix/store`. Resolved values are never logged.

use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::io::{BufRead, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::config::{Config, McpServer};
use crate::ipc;
use crate::protocol::{Reply, Request, WidgetSpec};

/// Name of the internal stdio MCP server exposing `show_widget` and
/// `propose_widget_template` to non-restricted lanes.
pub const INTERNAL_UI_MCP_SERVER: &str = "mcp-ui";

/// Name of the internal stdio MCP server exposing synchronous `delegate`
/// subagent execution (`hey-computer mcp-subagent`).
pub const INTERNAL_SUBAGENT_MCP_SERVER: &str = "subagents";

/// Maximum UTF-8 bytes returned in `delegate`'s `answer` field before truncation.
pub const MAX_SUBAGENT_ANSWER_BYTES: usize = 32 * 1024;

/// Supported `kind` values for `show_widget`.
pub const WIDGET_KINDS: &[&str] = &[
    "calendar_agenda",
    "transit_board",
    "tasks_journal",
    "dev_triage",
    "hue_rooms",
    "distill_diff",
    "day_plan",
    "custom_html",
];

/// Maximum framed JSON-RPC body size accepted by `mcp-ui` over stdio.
const MAX_UI_RPC_BYTES: usize = 16 * 1024 * 1024;

/// Validate and normalize arguments to `mcp-ui`'s `show_widget` tool.
pub fn parse_show_widget_args(
    args: &Value,
    fallback_idx: usize,
) -> std::result::Result<WidgetSpec, String> {
    let obj = args
        .as_object()
        .ok_or_else(|| "show_widget arguments must be a JSON object".to_string())?;

    let id = match obj.get("id") {
        None | Some(Value::Null) => format!("widget-{fallback_idx}"),
        Some(Value::String(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                format!("widget-{fallback_idx}")
            } else {
                trimmed.to_string()
            }
        }
        Some(_) => return Err("`id` must be a string when provided".to_string()),
    };

    let kind = obj
        .get("kind")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "`kind` is required".to_string())?;
    if !WIDGET_KINDS.contains(&kind) {
        return Err(format!(
            "unsupported widget `kind` `{kind}` (expected one of: {})",
            WIDGET_KINDS.join(", ")
        ));
    }

    let title = obj
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "`title` must be a non-empty string".to_string())?;

    let data = match obj.get("data") {
        Some(Value::Object(map)) => {
            if kind == "custom_html"
                && !map
                    .get("html")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.trim().is_empty())
            {
                return Err(
                    "`custom_html` widget requires a non-empty string `data.html`".to_string(),
                );
            }
            if kind == "day_plan" {
                validate_day_plan_data(map)?;
            }
            Value::Object(map.clone())
        }
        _ => return Err("`data` must be a JSON object".to_string()),
    };

    Ok(WidgetSpec {
        id,
        kind: kind.to_string(),
        title: title.to_string(),
        data,
    })
}

fn validate_day_plan_data(map: &Map<String, Value>) -> std::result::Result<(), String> {
    for (camel, snake) in [
        ("replyNudges", "reply_nudges"),
        ("tasks", "tasks"),
        ("calendarBlocks", "calendar_blocks"),
        ("checkins", "checkins"),
    ] {
        if let Some(val) = map.get(camel).or_else(|| map.get(snake))
            && !val.is_null()
            && !val.is_array()
        {
            return Err(format!("`day_plan` field `{camel}` must be a JSON array"));
        }
    }
    for (camel, snake) in [
        ("dailyNote", "daily_note"),
        ("preDailySync", "pre_daily_sync"),
    ] {
        if let Some(val) = map.get(camel).or_else(|| map.get(snake))
            && !val.is_null()
            && !val.is_object()
        {
            return Err(format!("`day_plan` field `{camel}` must be a JSON object"));
        }
    }

    let has_roter_faden = map
        .get("roterFaden")
        .or_else(|| map.get("roter_faden"))
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty());
    let has_tasks = map
        .get("tasks")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    let has_blocks = map
        .get("calendarBlocks")
        .or_else(|| map.get("calendar_blocks"))
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    let has_nudges = map
        .get("replyNudges")
        .or_else(|| map.get("reply_nudges"))
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());

    if !(has_roter_faden || has_tasks || has_blocks || has_nudges) {
        return Err(
            "`day_plan` widget requires non-empty `roterFaden`, `tasks`, `calendarBlocks`, or `replyNudges`"
                .to_string(),
        );
    }
    Ok(())
}

/// Validate and normalize arguments to `mcp-ui`'s `propose_widget_template` tool.
pub fn parse_propose_template_args(
    args: &Value,
    fallback_idx: usize,
) -> std::result::Result<WidgetSpec, String> {
    let obj = args
        .as_object()
        .ok_or_else(|| "propose_widget_template arguments must be a JSON object".to_string())?;

    let id = match obj.get("id") {
        None | Some(Value::Null) => format!("template-{fallback_idx}"),
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
        Some(Value::String(_)) => format!("template-{fallback_idx}"),
        Some(_) => return Err("`id` must be a string when provided".to_string()),
    };

    let skill_name = obj
        .get("skill_name")
        .or_else(|| obj.get("skillName"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "`skill_name` must be a non-empty string".to_string())?;
    if skill_name.contains(['/', '\\', '\0']) || skill_name.contains("..") {
        return Err("`skill_name` must not contain path separators or `..`".to_string());
    }

    let description = obj
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "`description` must be a non-empty string".to_string())?;

    let slots_val = obj
        .get("slots")
        .and_then(Value::as_array)
        .ok_or_else(|| "`slots` must be an array of strings".to_string())?;
    let mut slots = Vec::with_capacity(slots_val.len());
    for item in slots_val {
        let slot = item
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "every entry in `slots` must be a non-empty string".to_string())?;
        slots.push(slot.to_string());
    }

    let skill_markdown = obj
        .get("skill_markdown")
        .or_else(|| obj.get("skillMarkdown"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "`skill_markdown` must be a non-empty string".to_string())?;

    let template_html = obj
        .get("template_html")
        .or_else(|| obj.get("templateHtml"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "`template_html` must be a non-empty string".to_string())?;

    let title = obj
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| format!("Skill Template · {skill_name}"));

    Ok(WidgetSpec {
        id,
        kind: "skill_template_draft".to_string(),
        title,
        data: json!({
            "skillName": skill_name,
            "description": description,
            "slots": slots,
            "skillMarkdown": skill_markdown,
            "templateHtml": template_html,
        }),
    })
}

/// Run the internal `hey-computer mcp-ui` JSON-RPC 2.0 server over stdio.
///
/// Never writes logs to stdout; flushes stdout after every JSON-RPC response.
pub fn run_stdio_ui_server() -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve_ui_stdio(stdin.lock(), stdout.lock())
}

fn serve_ui_stdio<R: BufRead, W: Write>(mut reader: R, mut writer: W) -> Result<()> {
    let mut call_idx: usize = 0;
    while let Some((raw, framed)) = read_rpc_message(&mut reader)? {
        let parsed: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                let err_resp = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {
                        "code": -32700,
                        "message": format!("Parse error: {e}"),
                    }
                });
                write_rpc_message(&mut writer, &err_resp, framed)?;
                continue;
            }
        };
        if let Some(resp) = handle_ui_rpc_request(&parsed, &mut call_idx) {
            write_rpc_message(&mut writer, &resp, framed)?;
        }
    }
    Ok(())
}

/// Read either a `Content-Length:` framed JSON-RPC message or a newline-delimited
/// JSON-RPC line. Returns `Ok(None)` at clean EOF.
fn read_rpc_message<R: BufRead>(reader: &mut R) -> Result<Option<(String, bool)>> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed
            .split_once(':')
            .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .map(|(_, v)| v.trim())
        {
            let content_len: usize = rest
                .parse()
                .with_context(|| format!("invalid Content-Length header `{trimmed}`"))?;
            if content_len > MAX_UI_RPC_BYTES {
                bail!("MCP message Content-Length {content_len} exceeds {MAX_UI_RPC_BYTES} bytes");
            }
            // Consume remaining HTTP-style headers up to the blank separator line.
            loop {
                let mut hdr = String::new();
                let hn = reader.read_line(&mut hdr)?;
                if hn == 0 {
                    bail!("unexpected EOF while reading MCP headers");
                }
                if hdr.trim().is_empty() {
                    break;
                }
            }
            let mut body = vec![0u8; content_len];
            reader.read_exact(&mut body)?;
            let text = String::from_utf8(body).context("MCP message body is not UTF-8")?;
            return Ok(Some((text, true)));
        }
        if line.len() > MAX_UI_RPC_BYTES {
            bail!("MCP JSON-RPC line exceeds {MAX_UI_RPC_BYTES} bytes");
        }
        return Ok(Some((trimmed.to_string(), false)));
    }
}

fn write_rpc_message<W: Write>(writer: &mut W, message: &Value, framed: bool) -> Result<()> {
    let serialized = serde_json::to_string(message)?;
    if framed {
        write!(
            writer,
            "Content-Length: {}\r\n\r\n{}",
            serialized.len(),
            serialized
        )?;
    } else {
        writeln!(writer, "{serialized}")?;
    }
    writer.flush()?;
    Ok(())
}

fn handle_ui_rpc_request(msg: &Value, call_idx: &mut usize) -> Option<Value> {
    let Some(obj) = msg.as_object() else {
        return Some(json!({
            "jsonrpc": "2.0",
            "id": Value::Null,
            "error": {"code": -32600, "message": "Invalid Request"}
        }));
    };
    let method = obj.get("method").and_then(Value::as_str);
    // JSON-RPC notifications omit `id` and MUST NOT receive a response.
    let id = obj.get("id").cloned()?;
    let Some(method) = method else {
        return Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32600, "message": "Missing method"}
        }));
    };
    let params = obj.get("params").unwrap_or(&Value::Null);

    match method {
        "initialize" => {
            let protocol_version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("2024-11-05");
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": protocol_version,
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "hey-computer-mcp-ui",
                        "version": "0.1.0"
                    }
                }
            }))
        }
        "ping" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {}
        })),
        "tools/list" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": ui_tool_schemas()
            }
        })),
        "resources/list" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "resources": [] }
        })),
        "prompts/list" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "prompts": [] }
        })),
        "tools/call" => {
            *call_idx += 1;
            let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let default_args = json!({});
            let arguments = params.get("arguments").unwrap_or(&default_args);
            let parsed = match tool_name {
                "show_widget" => parse_show_widget_args(arguments, *call_idx),
                "propose_widget_template" => parse_propose_template_args(arguments, *call_idx),
                other => Err(format!("unknown mcp-ui tool `{other}`")),
            };
            let result = match parsed {
                Ok(spec) => {
                    let ack = json!({
                        "ok": true,
                        "id": spec.id,
                        "kind": spec.kind,
                        "title": spec.title,
                    });
                    json!({
                        "content": [{
                            "type": "text",
                            "text": ack.to_string(),
                        }]
                    })
                }
                Err(err) => {
                    let body = json!({
                        "ok": false,
                        "error": err,
                    });
                    json!({
                        "content": [{
                            "type": "text",
                            "text": body.to_string(),
                        }],
                        "isError": true
                    })
                }
            };
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
            }))
        }
        _ => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("Method not found: {method}"),
            }
        })),
    }
}

fn ui_tool_schemas() -> Value {
    json!([
        {
            "name": "show_widget",
            "description": "Render an interactive structured widget card or custom HTML/SVG surface inside hey-computer-viewer alongside Markdown prose.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Optional stable widget identifier."
                    },
                    "kind": {
                        "type": "string",
                        "enum": WIDGET_KINDS,
                        "description": "Built-in structured card kind or `custom_html` (`data.html`)."
                    },
                    "title": {
                        "type": "string",
                        "description": "Concise human-readable card header title."
                    },
                    "data": {
                        "type": "object",
                        "description": "Structured card payload object (for `custom_html`, include `html: string`)."
                    }
                },
                "required": ["kind", "title", "data"]
            }
        },
        {
            "name": "propose_widget_template",
            "description": "Propose a reusable parameterized widget template (`SKILL.md` + `template.html`) for manual user review before saving.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "skill_name": {
                        "type": "string",
                        "description": "Slug name for the skill directory (e.g. `widget-sprint-pulse`)."
                    },
                    "description": {
                        "type": "string",
                        "description": "Short summary of what the template renders and when to use it."
                    },
                    "slots": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Template placeholder slot names."
                    },
                    "skill_markdown": {
                        "type": "string",
                        "description": "Complete `SKILL.md` content with YAML frontmatter and usage instructions."
                    },
                    "template_html": {
                        "type": "string",
                        "description": "Parameterized HTML/SVG/JS widget template."
                    }
                },
                "required": [
                    "skill_name",
                    "description",
                    "slots",
                    "skill_markdown",
                    "template_html"
                ]
            }
        }
    ])
}

fn resolve_mcp_ui_command() -> String {
    if let Some(bin) = std::env::var_os("CARGO_BIN_EXE_hey-computer").filter(|s| !s.is_empty()) {
        return PathBuf::from(bin).to_string_lossy().into_owned();
    }
    if let Ok(exe) = std::env::current_exe()
        && exe.file_name().and_then(|n| n.to_str()) == Some("hey-computer")
    {
        return exe.to_string_lossy().into_owned();
    }
    "hey-computer".to_string()
}

/// Maximum bytes read from a single secret file (`env_files` / `header_files`).
const MAX_SECRET_BYTES: usize = 16 * 1024;

/// Validated arguments to `subagents`'s `delegate` tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegateToolArgs {
    pub role: String,
    pub prompt: String,
    pub profile: Option<String>,
    pub allowed_mcp_servers: Option<Vec<String>>,
}

/// Validate and normalize arguments to `subagents`'s `delegate` tool.
pub fn parse_delegate_args(args: &Value) -> std::result::Result<DelegateToolArgs, String> {
    let obj = args
        .as_object()
        .ok_or_else(|| "delegate arguments must be a JSON object".to_string())?;

    let role = obj
        .get("role")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "`role` must be a non-empty string".to_string())?
        .to_string();

    let prompt = obj
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "`prompt` must be a non-empty string".to_string())?
        .to_string();

    let profile = match obj.get("profile") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Some(_) => return Err("`profile` must be a string when provided".to_string()),
    };

    let allowed_mcp_servers = match obj.get("allowed_mcp_servers") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let name = item
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        "every entry in `allowed_mcp_servers` must be a non-empty string"
                            .to_string()
                    })?;
                out.push(name.to_string());
            }
            Some(out)
        }
        Some(_) => {
            return Err(
                "`allowed_mcp_servers` must be an array of strings when provided".to_string(),
            );
        }
    };

    Ok(DelegateToolArgs {
        role,
        prompt,
        profile,
        allowed_mcp_servers,
    })
}

/// Cap a subagent answer at [`MAX_SUBAGENT_ANSWER_BYTES`] on a UTF-8 char boundary.
pub fn bound_subagent_answer(text: &str) -> String {
    if text.len() <= MAX_SUBAGENT_ANSWER_BYTES {
        return text.to_string();
    }
    const SUFFIX: &str = "\n\n[Output truncated to 32 KiB]";
    let budget = MAX_SUBAGENT_ANSWER_BYTES.saturating_sub(SUFFIX.len());
    let mut end = budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + SUFFIX.len());
    out.push_str(text[..end].trim_end());
    out.push_str(SUFFIX);
    out
}

/// Run the internal `hey-computer mcp-subagent` async JSON-RPC 2.0 server over stdio.
///
/// Spawns one tokio task per `tools/call` so parallel `delegate` calls run
/// concurrently over separate daemon socket connections, serializes all stdout
/// responses through a single writer task, and cancels in-flight calls on
/// `notifications/cancelled` or stdin EOF.
pub async fn run_stdio_subagent_server(socket: Option<PathBuf>) -> Result<()> {
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    serve_subagent_stdio(stdin, stdout, socket).await
}

async fn serve_subagent_stdio<R, W>(
    mut reader: R,
    mut writer: W,
    socket: Option<PathBuf>,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<(Value, bool)>();
    let writer_task = tokio::spawn(async move {
        while let Some((msg, framed)) = out_rx.recv().await {
            if write_rpc_message_async(&mut writer, &msg, framed)
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let inflight: Arc<Mutex<HashMap<String, tokio::task::AbortHandle>>> =
        Arc::new(Mutex::new(HashMap::new()));

    while let Some((raw, framed)) = read_rpc_message_async(&mut reader).await? {
        let parsed: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                let err_resp = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {
                        "code": -32700,
                        "message": format!("Parse error: {e}"),
                    }
                });
                let _ = out_tx.send((err_resp, framed));
                continue;
            }
        };

        let Some(obj) = parsed.as_object() else {
            let _ = out_tx.send((
                json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {"code": -32600, "message": "Invalid Request"}
                }),
                framed,
            ));
            continue;
        };

        let method = obj.get("method").and_then(Value::as_str);
        let id = obj.get("id").cloned();
        let params = obj.get("params").cloned().unwrap_or(Value::Null);

        // Notifications omit `id` and never receive a JSON-RPC response.
        if id.is_none() {
            if method == Some("notifications/cancelled")
                && let Some(req_id) = params.get("requestId").or_else(|| params.get("id"))
            {
                let key = req_id.to_string();
                if let Ok(mut map) = inflight.lock()
                    && let Some(handle) = map.remove(&key)
                {
                    handle.abort();
                }
            }
            continue;
        }
        let id = id.expect("checked above");
        let Some(method) = method else {
            let _ = out_tx.send((
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32600, "message": "Missing method"}
                }),
                framed,
            ));
            continue;
        };

        match method {
            "initialize" => {
                let protocol_version = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("2024-11-05");
                let _ = out_tx.send((
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": protocol_version,
                            "capabilities": {
                                "tools": {}
                            },
                            "serverInfo": {
                                "name": "hey-computer-mcp-subagent",
                                "version": "0.1.0"
                            }
                        }
                    }),
                    framed,
                ));
            }
            "ping" => {
                let _ = out_tx.send((
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {}
                    }),
                    framed,
                ));
            }
            "tools/list" => {
                let _ = out_tx.send((
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": subagent_tool_schemas()
                        }
                    }),
                    framed,
                ));
            }
            "resources/list" => {
                let _ = out_tx.send((
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "resources": [] }
                    }),
                    framed,
                ));
            }
            "prompts/list" => {
                let _ = out_tx.send((
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "prompts": [] }
                    }),
                    framed,
                ));
            }
            "tools/call" => {
                let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let default_args = json!({});
                let arguments = params.get("arguments").unwrap_or(&default_args);
                if tool_name != "delegate" {
                    let body = json!({
                        "ok": false,
                        "error": format!("unknown subagents tool `{tool_name}`"),
                    });
                    let _ = out_tx.send((
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{ "type": "text", "text": body.to_string() }],
                                "isError": true,
                            }
                        }),
                        framed,
                    ));
                    continue;
                }
                let args = match parse_delegate_args(arguments) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        let body = json!({
                            "ok": false,
                            "error": err,
                        });
                        let _ = out_tx.send((
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "content": [{ "type": "text", "text": body.to_string() }],
                                    "isError": true,
                                }
                            }),
                            framed,
                        ));
                        continue;
                    }
                };

                let id_key = id.to_string();
                let id_key_task = id_key.clone();
                let out = out_tx.clone();
                let inflight_ref = Arc::clone(&inflight);
                let sock_override = socket.clone();
                if let Ok(mut map) = inflight.lock() {
                    let task = tokio::spawn(async move {
                        let result = execute_delegate_call(sock_override.as_deref(), args).await;
                        if let Ok(mut map) = inflight_ref.lock() {
                            map.remove(&id_key_task);
                        }
                        let _ = out.send((
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": result,
                            }),
                            framed,
                        ));
                    });
                    map.insert(id_key, task.abort_handle());
                }
            }
            _ => {
                let _ = out_tx.send((
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": format!("Method not found: {method}"),
                        }
                    }),
                    framed,
                ));
            }
        }
    }

    // Abort any still-running delegate calls when stdin closes so their daemon
    // sockets close immediately and cancel orphaned child sessions.
    if let Ok(mut map) = inflight.lock() {
        for (_, handle) in map.drain() {
            handle.abort();
        }
    }
    drop(out_tx);
    let _ = writer_task.await;
    Ok(())
}

async fn execute_delegate_call(socket: Option<&Path>, args: DelegateToolArgs) -> Value {
    let role = args.role.clone();
    let sock_path = match ipc::socket_path(socket) {
        Ok(path) => path,
        Err(e) => {
            let body = json!({
                "ok": false,
                "role": role,
                "error": format!("{e:#}"),
            });
            return json!({
                "content": [{ "type": "text", "text": body.to_string() }],
                "isError": true,
            });
        }
    };
    let req = Request::Delegate {
        role: args.role,
        prompt: args.prompt,
        profile: args.profile,
        allowed_mcp_servers: args.allowed_mcp_servers,
        parent_session: None,
    };
    let mut client = match ipc::Client::send(&sock_path, &req).await {
        Ok(client) => client,
        Err(e) => {
            let body = json!({
                "ok": false,
                "role": role,
                "error": format!("{e:#}"),
            });
            return json!({
                "content": [{ "type": "text", "text": body.to_string() }],
                "isError": true,
            });
        }
    };
    match client.next(None).await {
        Ok(Reply::Answer { text, denied, .. }) => {
            let mut payload = json!({
                "ok": true,
                "role": role,
                "answer": bound_subagent_answer(&text),
            });
            if !denied.is_empty() {
                payload["denied"] = json!(denied);
            }
            json!({
                "content": [{ "type": "text", "text": payload.to_string() }],
            })
        }
        Ok(Reply::Failed {
            reason, message, ..
        }) => {
            let payload = json!({
                "ok": false,
                "role": role,
                "reason": reason,
                "error": message,
            });
            json!({
                "content": [{ "type": "text", "text": payload.to_string() }],
                "isError": true,
            })
        }
        Ok(Reply::Error { message }) => {
            let payload = json!({
                "ok": false,
                "role": role,
                "error": message,
            });
            json!({
                "content": [{ "type": "text", "text": payload.to_string() }],
                "isError": true,
            })
        }
        Ok(other) => {
            let payload = json!({
                "ok": false,
                "role": role,
                "error": format!("unexpected daemon reply: {other:?}"),
            });
            json!({
                "content": [{ "type": "text", "text": payload.to_string() }],
                "isError": true,
            })
        }
        Err(e) => {
            let payload = json!({
                "ok": false,
                "role": role,
                "error": format!("{e:#}"),
            });
            json!({
                "content": [{ "type": "text", "text": payload.to_string() }],
                "isError": true,
            })
        }
    }
}

async fn read_rpc_message_async<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<(String, bool)>> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed
            .split_once(':')
            .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .map(|(_, v)| v.trim())
        {
            let content_len: usize = rest
                .parse()
                .with_context(|| format!("invalid Content-Length header `{trimmed}`"))?;
            if content_len > MAX_UI_RPC_BYTES {
                bail!("MCP message Content-Length {content_len} exceeds {MAX_UI_RPC_BYTES} bytes");
            }
            loop {
                let mut hdr = String::new();
                let hn = reader.read_line(&mut hdr).await?;
                if hn == 0 {
                    bail!("unexpected EOF while reading MCP headers");
                }
                if hdr.trim().is_empty() {
                    break;
                }
            }
            let mut body = vec![0u8; content_len];
            reader.read_exact(&mut body).await?;
            let text = String::from_utf8(body).context("MCP message body is not UTF-8")?;
            return Ok(Some((text, true)));
        }
        if line.len() > MAX_UI_RPC_BYTES {
            bail!("MCP JSON-RPC line exceeds {MAX_UI_RPC_BYTES} bytes");
        }
        return Ok(Some((trimmed.to_string(), false)));
    }
}

async fn write_rpc_message_async<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &Value,
    framed: bool,
) -> Result<()> {
    let serialized = serde_json::to_string(message)?;
    if framed {
        let frame = format!("Content-Length: {}\r\n\r\n{}", serialized.len(), serialized);
        writer.write_all(frame.as_bytes()).await?;
    } else {
        writer.write_all(serialized.as_bytes()).await?;
        writer.write_all(b"\n").await?;
    }
    writer.flush().await?;
    Ok(())
}

fn subagent_tool_schemas() -> Value {
    json!([
        {
            "name": "delegate",
            "description": "Synchronously run a scoped subagent worker in an isolated ACP session and return its concise result. Batch independent delegate calls in parallel when they can run concurrently.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "role": {
                        "type": "string",
                        "description": "Short label for the worker's role (e.g. 'Personal Comms & Calendar' or 'Work Triage')."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Self-contained instructions for the worker, including expected output structure and whether it must stay read-only."
                    },
                    "profile": {
                        "type": "string",
                        "description": "Optional named profile from [profiles.<name>] (defaults to [subagents].profile, routing.fast_profile, or base [agent])."
                    },
                    "allowed_mcp_servers": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional allowlist of MCP server names this worker may access (intersected with the child profile's mcp_servers; pass [] to disable external MCP servers)."
                    }
                },
                "required": ["role", "prompt"]
            }
        }
    ])
}

/// Build the `{ "mcpServers": { ... } }` document written to
/// `<agent_dir>/config/mcp_config.json`.
///
/// When `mcp_browser` is provided, stdio servers that do not already configure
/// `BROWSER` receive `BROWSER=<mcp_browser>` so OAuth browser launches cannot
/// inherit the MCP server's JSON-RPC `stdout` pipe and crash the connection.
///
/// When `config.subagents.enabled` is true, registers the internal `subagents`
/// stdio MCP server (`hey-computer mcp-subagent`), propagating `socket` via
/// `env.HEY_COMPUTER_SOCKET` when provided.
///
/// Returns `None` when no enabled servers resolve cleanly. Servers whose secret
/// files are missing or unreadable are skipped with a warning (never logging
/// secret values) so a missing optional token does not break the daemon.
pub fn build_mcp_config(
    config: &Config,
    mcp_browser: Option<&Path>,
    socket: Option<&Path>,
) -> Option<Value> {
    let browser_env = mcp_browser.and_then(usable_browser_path);
    let internal_cmd = resolve_mcp_ui_command();
    let mut servers = Map::new();
    servers.insert(
        INTERNAL_UI_MCP_SERVER.to_string(),
        json!({
            "type": "stdio",
            "command": internal_cmd,
            "args": ["mcp-ui"],
            "enabled": true,
        }),
    );
    if config.subagents.enabled {
        let mut sub_entry = Map::new();
        sub_entry.insert("type".to_string(), json!("stdio"));
        sub_entry.insert("command".to_string(), json!(internal_cmd));
        sub_entry.insert("args".to_string(), json!(["mcp-subagent"]));
        if let Some(sock) = socket.filter(|p| !p.as_os_str().is_empty()) {
            sub_entry.insert(
                "env".to_string(),
                json!({ "HEY_COMPUTER_SOCKET": sock.to_string_lossy() }),
            );
        }
        sub_entry.insert("enabled".to_string(), json!(true));
        servers.insert(
            INTERNAL_SUBAGENT_MCP_SERVER.to_string(),
            Value::Object(sub_entry),
        );
    }
    for (name, server) in &config.mcp {
        if !server.enabled {
            continue;
        }
        match build_server_entry(name, server, browser_env) {
            Ok(val) => {
                servers.insert(name.clone(), val);
            }
            Err(e) => {
                tracing::warn!("skipping MCP server `{name}`: {e:#}");
            }
        }
    }
    if servers.is_empty() {
        None
    } else {
        Some(json!({ "mcpServers": servers }))
    }
}

/// Reject paths containing whitespace or `:` because `$BROWSER` consumers
/// (Python `webbrowser`, `xdg-open`) split on `:` and shell-word boundaries.
fn usable_browser_path(path: &Path) -> Option<&str> {
    let text = path.to_str()?;
    if text.is_empty() || text.contains(':') || text.chars().any(char::is_whitespace) {
        return None;
    }
    Some(text)
}

fn build_server_entry(name: &str, server: &McpServer, browser_env: Option<&str>) -> Result<Value> {
    let mut entry = Map::new();
    if !server.command.is_empty() {
        let mut env = resolve_kv_map(name, "env", &server.env, &server.env_files)?;
        if let Some(browser) = browser_env {
            env.entry("BROWSER".to_string())
                .or_insert_with(|| json!(browser));
        }
        entry.insert("type".to_string(), json!("stdio"));
        entry.insert("command".to_string(), json!(server.command));
        if !server.args.is_empty() {
            entry.insert("args".to_string(), json!(server.args));
        }
        if !env.is_empty() {
            entry.insert("env".to_string(), Value::Object(env));
        }
        entry.insert("enabled".to_string(), json!(true));
        return Ok(Value::Object(entry));
    }
    let transport = if server.transport == "sse" {
        "sse"
    } else {
        "http"
    };
    let headers = resolve_kv_map(name, "header", &server.headers, &server.header_files)?;
    entry.insert("type".to_string(), json!(transport));
    // agy's config/mcp_config.json expects `serverUrl` for remote MCP servers.
    entry.insert("serverUrl".to_string(), json!(server.url));
    if !headers.is_empty() {
        entry.insert("headers".to_string(), Value::Object(headers));
    }
    Ok(Value::Object(entry))
}

fn resolve_kv_map(
    server_name: &str,
    kind: &str,
    inline: &BTreeMap<String, String>,
    files: &BTreeMap<String, PathBuf>,
) -> Result<Map<String, Value>> {
    let mut map = Map::new();
    for (k, v) in inline {
        map.insert(k.clone(), json!(v));
    }
    for (k, path) in files {
        let secret = read_secret_file(path).with_context(|| {
            format!("reading {kind} file for `{k}` on MCP server `{server_name}`")
        })?;
        map.insert(k.clone(), json!(secret));
    }
    Ok(map)
}

fn read_secret_file(path: &Path) -> Result<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let meta = file
        .metadata()
        .with_context(|| format!("checking {}", path.display()))?;
    if !meta.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    if meta.permissions().mode() & 0o077 != 0 {
        tracing::warn!(
            "secret file {} is readable by group or others (expected 0600)",
            path.display()
        );
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_SECRET_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() > MAX_SECRET_BYTES {
        bail!(
            "secret file {} exceeds {MAX_SECRET_BYTES} bytes",
            path.display()
        );
    }
    let text = String::from_utf8(bytes)
        .with_context(|| format!("{} is not valid UTF-8", path.display()))?;
    Ok(text.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn resolves_stdio_http_sse_and_respects_allow_mcp_and_missing_secrets() {
        let dir = std::env::temp_dir().join(format!("hc-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let token_path = dir.join("tok");
        std::fs::write(&token_path, "secret-value\n").unwrap();
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let mut config = Config::default();
        config.mcp.insert(
            "nixos".to_string(),
            McpServer {
                enabled: true,
                command: "/bin/mcp-nixos".to_string(),
                args: vec!["--stdio".to_string()],
                env: BTreeMap::from([("PLAIN".to_string(), "1".to_string())]),
                env_files: BTreeMap::from([("TOKEN".to_string(), token_path.clone())]),
                ..McpServer::default()
            },
        );
        config.mcp.insert(
            "remote".to_string(),
            McpServer {
                enabled: true,
                url: "https://mcp.example.test/mcp".to_string(),
                header_files: BTreeMap::from([("Authorization".to_string(), token_path)]),
                ..McpServer::default()
            },
        );
        config.mcp.insert(
            "broken".to_string(),
            McpServer {
                enabled: true,
                command: "/bin/broken".to_string(),
                env_files: BTreeMap::from([("MISSING".to_string(), dir.join("nope"))]),
                ..McpServer::default()
            },
        );
        config.validate().unwrap();

        let mut empty = Config::default();
        let empty_doc = build_mcp_config(&empty, None, Some(Path::new("/tmp/hc.sock")))
            .expect("auto-registers mcp-ui and subagents");
        let empty_servers = empty_doc["mcpServers"].as_object().unwrap();
        assert_eq!(empty_servers.len(), 2, "{empty_servers:?}");
        assert_eq!(empty_servers["mcp-ui"]["type"], "stdio");
        assert_eq!(empty_servers["mcp-ui"]["args"], json!(["mcp-ui"]));
        assert_eq!(empty_servers["mcp-ui"]["enabled"], true);
        assert_eq!(empty_servers["subagents"]["type"], "stdio");
        assert_eq!(empty_servers["subagents"]["args"], json!(["mcp-subagent"]));
        assert_eq!(
            empty_servers["subagents"]["env"]["HEY_COMPUTER_SOCKET"],
            "/tmp/hc.sock"
        );
        assert_eq!(empty_servers["subagents"]["enabled"], true);

        empty.subagents.enabled = false;
        let disabled_sub = build_mcp_config(&empty, None, None).unwrap();
        assert_eq!(disabled_sub["mcpServers"].as_object().unwrap().len(), 1);
        assert!(disabled_sub["mcpServers"].get("subagents").is_none());

        let browser = Path::new("/tmp/hc-mcp-browser");
        let doc = build_mcp_config(&config, Some(browser), None).expect("mcp config built");
        let servers = doc["mcpServers"].as_object().unwrap();
        assert_eq!(servers.len(), 4, "{servers:?}");
        assert_eq!(servers["mcp-ui"]["type"], "stdio");
        assert_eq!(servers["mcp-ui"]["args"], json!(["mcp-ui"]));
        assert_eq!(servers["mcp-ui"]["enabled"], true);
        assert!(servers["mcp-ui"].get("env").is_none());
        assert_eq!(servers["subagents"]["args"], json!(["mcp-subagent"]));
        assert!(servers["subagents"].get("env").is_none());
        assert_eq!(servers["nixos"]["type"], "stdio");
        assert_eq!(servers["nixos"]["command"], "/bin/mcp-nixos");
        assert_eq!(servers["nixos"]["args"], json!(["--stdio"]));
        assert_eq!(
            servers["nixos"]["env"],
            json!({
                "BROWSER": "/tmp/hc-mcp-browser",
                "PLAIN": "1",
                "TOKEN": "secret-value"
            })
        );
        assert_eq!(servers["remote"]["type"], "http");
        assert_eq!(
            servers["remote"]["serverUrl"],
            "https://mcp.example.test/mcp"
        );
        assert_eq!(
            servers["remote"]["headers"],
            json!({"Authorization": "secret-value"})
        );
        assert!(servers["remote"].get("env").is_none());

        // Explicit user BROWSER wins over the default wrapper; paths with spaces
        // or colons are skipped rather than broken by $BROWSER splitting.
        config
            .mcp
            .get_mut("nixos")
            .unwrap()
            .env
            .insert("BROWSER".to_string(), "/custom/browser".to_string());
        let doc = build_mcp_config(&config, Some(browser), None).unwrap();
        assert_eq!(
            doc["mcpServers"]["nixos"]["env"]["BROWSER"],
            "/custom/browser"
        );
        config.mcp.get_mut("nixos").unwrap().env.remove("BROWSER");
        let doc =
            build_mcp_config(&config, Some(Path::new("/tmp/has space/mcp-browser")), None).unwrap();
        assert!(doc["mcpServers"]["nixos"]["env"].get("BROWSER").is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn validates_show_widget_and_propose_template_arguments() {
        let spec = parse_show_widget_args(
            &json!({
                "kind": "transit_board",
                "title": " U3 Borgweg ",
                "data": { "departures": [] }
            }),
            1,
        )
        .unwrap();
        assert_eq!(spec.id, "widget-1");
        assert_eq!(spec.kind, "transit_board");
        assert_eq!(spec.title, "U3 Borgweg");

        let custom = parse_show_widget_args(
            &json!({
                "id": "custom-9",
                "kind": "custom_html",
                "title": "Sparkline",
                "data": { "html": "<svg></svg>" }
            }),
            2,
        )
        .unwrap();
        assert_eq!(custom.id, "custom-9");
        assert_eq!(custom.kind, "custom_html");

        // Rejects unknown kind, empty title, non-object data, and custom_html without data.html.
        for bad in [
            json!({"kind": "unknown_card", "title": "T", "data": {}}),
            json!({"kind": "hue_rooms", "title": "   ", "data": {}}),
            json!({"kind": "hue_rooms", "title": "Rooms", "data": "not-an-object"}),
            json!({"kind": "custom_html", "title": "Empty", "data": {}}),
            json!({"id": 123, "kind": "hue_rooms", "title": "Rooms", "data": {}}),
        ] {
            assert!(
                parse_show_widget_args(&bad, 3).is_err(),
                "{bad} should be rejected"
            );
        }

        let tmpl = parse_propose_template_args(
            &json!({
                "skill_name": "widget-sprint-pulse",
                "description": "Sprint burndown card",
                "slots": ["sprint_name", "open_count"],
                "skill_markdown": "---\nname: widget-sprint-pulse\n---\nBody",
                "template_html": "<div>{{sprint_name}}</div>"
            }),
            4,
        )
        .unwrap();
        assert_eq!(tmpl.id, "template-4");
        assert_eq!(tmpl.kind, "skill_template_draft");
        assert_eq!(tmpl.title, "Skill Template · widget-sprint-pulse");
        assert_eq!(tmpl.data["skillName"], "widget-sprint-pulse");
        assert_eq!(tmpl.data["slots"], json!(["sprint_name", "open_count"]));

        for bad_tmpl in [
            json!({"skill_name": "../escape", "description": "d", "slots": [], "skill_markdown": "m", "template_html": "h"}),
            json!({"skill_name": "ok", "description": "", "slots": [], "skill_markdown": "m", "template_html": "h"}),
            json!({"skill_name": "ok", "description": "d", "slots": [42], "skill_markdown": "m", "template_html": "h"}),
        ] {
            assert!(parse_propose_template_args(&bad_tmpl, 5).is_err());
        }
    }

    #[test]
    fn validates_day_plan_widget_camel_and_snake_case() {
        let camel = parse_show_widget_args(
            &json!({
                "id": "plan-1",
                "kind": "day_plan",
                "title": "Day Plan — Thursday",
                "data": {
                    "date": "2026-11-26",
                    "mode": "workday",
                    "modeDetail": "09:30–18:00 · Office",
                    "status": "proposed",
                    "roterFaden": "Ship the auth fix before lunch, then land webhook retries.",
                    "replyNudges": [{
                        "id": "r1",
                        "channel": "slack",
                        "sender": "Robin",
                        "summary": "Confirm schema compatibility",
                        "selected": true
                    }],
                    "tasks": [{
                        "id": "t1",
                        "title": "Fix integration test",
                        "selected": true,
                        "subtasks": ["Reproduce locally", "Push fix"]
                    }],
                    "calendarBlocks": [{
                        "id": "b1",
                        "start": "09:45",
                        "end": "11:15",
                        "title": "Focus: Auth Fix",
                        "calendar": "work",
                        "transparency": "opaque",
                        "selected": true
                    }],
                    "dailyNote": {
                        "enabled": true,
                        "path": "Journal/2026-11-26.md",
                        "writeDayPlan": true,
                        "syncTodos": true
                    },
                    "preDailySync": {
                        "enabled": true,
                        "label": "Anchor StandUp brief on this Roter Faden"
                    },
                    "checkins": [{
                        "id": "midday",
                        "time": "12:45",
                        "label": "Midday Reassessment",
                        "selected": true
                    }]
                }
            }),
            1,
        )
        .unwrap();
        assert_eq!(camel.id, "plan-1");
        assert_eq!(camel.kind, "day_plan");

        let snake = parse_show_widget_args(
            &json!({
                "kind": "day_plan",
                "title": "Day Plan — Saturday",
                "data": {
                    "date": "2026-11-28",
                    "mode": "day_off",
                    "mode_detail": "Weekend",
                    "roter_faden": "Unhurried morning, clear two errands.",
                    "reply_nudges": [],
                    "calendar_blocks": [],
                    "daily_note": { "enabled": true, "write_day_plan": true, "sync_todos": true },
                    "pre_daily_sync": { "enabled": false }
                }
            }),
            2,
        )
        .unwrap();
        assert_eq!(snake.id, "widget-2");
        assert_eq!(snake.kind, "day_plan");

        // Valid without roterFaden when tasks, calendarBlocks, or replyNudges is non-empty.
        for minimal in [
            json!({"kind": "day_plan", "title": "Tasks Only", "data": {"tasks": [{"title": "Do thing"}]}}),
            json!({"kind": "day_plan", "title": "Blocks Only", "data": {"calendar_blocks": [{"title": "Focus"}]}}),
            json!({"kind": "day_plan", "title": "Nudges Only", "data": {"replyNudges": [{"sender": "Sam"}]}}),
        ] {
            assert!(
                parse_show_widget_args(&minimal, 3).is_ok(),
                "{minimal} should be accepted"
            );
        }

        // Rejects empty data, blank roterFaden with empty lists, and wrong field types.
        for bad in [
            json!({"kind": "day_plan", "title": "Empty", "data": {}}),
            json!({"kind": "day_plan", "title": "Blank", "data": {"roterFaden": "   ", "tasks": [], "calendarBlocks": [], "replyNudges": []}}),
            json!({"kind": "day_plan", "title": "Bad Tasks", "data": {"roterFaden": "Ok", "tasks": "nope"}}),
            json!({"kind": "day_plan", "title": "Bad Blocks", "data": {"roterFaden": "Ok", "calendar_blocks": 123}}),
            json!({"kind": "day_plan", "title": "Bad Nudges", "data": {"roterFaden": "Ok", "replyNudges": {}}}),
            json!({"kind": "day_plan", "title": "Bad Checkins", "data": {"roterFaden": "Ok", "checkins": "12:00"}}),
            json!({"kind": "day_plan", "title": "Bad DailyNote", "data": {"roterFaden": "Ok", "dailyNote": true}}),
            json!({"kind": "day_plan", "title": "Bad PreDaily", "data": {"roterFaden": "Ok", "pre_daily_sync": "yes"}}),
        ] {
            assert!(
                parse_show_widget_args(&bad, 4).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn stdio_ui_server_speaks_newline_and_content_length_jsonrpc() {
        // 1. Newline-delimited JSON-RPC session (initialize -> initialized -> ping -> tools/list -> tools/call).
        let input = [
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}).to_string(),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}).to_string(),
            json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}).to_string(),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}).to_string(),
            json!({
                "jsonrpc": "2.0",
                "id": "call-1",
                "method": "tools/call",
                "params": {
                    "name": "show_widget",
                    "arguments": {
                        "kind": "calendar_agenda",
                        "title": "Today",
                        "data": {"events": []}
                    }
                }
            })
            .to_string(),
            json!({
                "jsonrpc": "2.0",
                "id": "call-bad",
                "method": "tools/call",
                "params": {
                    "name": "show_widget",
                    "arguments": {
                        "kind": "nope",
                        "title": "Bad",
                        "data": {}
                    }
                }
            })
            .to_string(),
        ]
        .join("\n")
            + "\n";

        let mut out = Vec::new();
        serve_ui_stdio(std::io::Cursor::new(input.into_bytes()), &mut out).unwrap();
        let lines: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        // Notification produced no response -> 5 responses for 5 requests.
        assert_eq!(lines.len(), 5, "{lines:?}");
        assert_eq!(lines[0]["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(
            lines[0]["result"]["serverInfo"]["name"],
            "hey-computer-mcp-ui"
        );
        assert_eq!(lines[1]["result"], json!({}));
        assert_eq!(lines[2]["result"]["tools"].as_array().unwrap().len(), 2);
        assert!(lines[3]["result"].get("isError").is_none());
        let ok_text = lines[3]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(ok_text.contains("\"ok\":true"), "{ok_text}");
        assert_eq!(lines[4]["result"]["isError"], true);

        // 2. Content-Length framed JSON-RPC session.
        let req_body = json!({"jsonrpc": "2.0", "id": 99, "method": "initialize"}).to_string();
        let framed_in = format!("Content-Length: {}\r\n\r\n{}", req_body.len(), req_body);
        let mut framed_out = Vec::new();
        serve_ui_stdio(
            std::io::Cursor::new(framed_in.into_bytes()),
            &mut framed_out,
        )
        .unwrap();
        let framed_str = String::from_utf8(framed_out).unwrap();
        assert!(framed_str.starts_with("Content-Length: "), "{framed_str}");
        let (_, body) = framed_str.split_once("\r\n\r\n").unwrap();
        let parsed_resp: Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed_resp["id"], 99);
        assert_eq!(parsed_resp["result"]["protocolVersion"], "2024-11-05");
    }

    #[test]
    fn validates_delegate_arguments_and_bounds_output() {
        let parsed = parse_delegate_args(&json!({
            "role": " Personal Comms ",
            "prompt": " Summarize inbox ",
            "profile": "fast",
            "allowed_mcp_servers": ["google-workspace"]
        }))
        .unwrap();
        assert_eq!(parsed.role, "Personal Comms");
        assert_eq!(parsed.prompt, "Summarize inbox");
        assert_eq!(parsed.profile.as_deref(), Some("fast"));
        assert_eq!(
            parsed.allowed_mcp_servers,
            Some(vec!["google-workspace".to_string()])
        );

        let minimal = parse_delegate_args(&json!({
            "role": "Worker",
            "prompt": "Check logs",
            "profile": "   ",
            "allowed_mcp_servers": []
        }))
        .unwrap();
        assert_eq!(minimal.profile, None);
        assert_eq!(minimal.allowed_mcp_servers, Some(vec![]));

        for bad in [
            json!("not-an-object"),
            json!({"role": "", "prompt": "ok"}),
            json!({"role": "r", "prompt": "   "}),
            json!({"role": "r", "prompt": "p", "profile": 42}),
            json!({"role": "r", "prompt": "p", "allowed_mcp_servers": "slack"}),
            json!({"role": "r", "prompt": "p", "allowed_mcp_servers": [""]}),
        ] {
            assert!(
                parse_delegate_args(&bad).is_err(),
                "{bad} should be rejected"
            );
        }

        let short = "hello world";
        assert_eq!(bound_subagent_answer(short), short);

        // Multi-byte UTF-8 truncation respects char boundaries and stays within 32 KiB.
        let huge = "ä".repeat(MAX_SUBAGENT_ANSWER_BYTES);
        let bounded = bound_subagent_answer(&huge);
        assert!(bounded.len() <= MAX_SUBAGENT_ANSWER_BYTES);
        assert!(bounded.ends_with("[Output truncated to 32 KiB]"));
    }

    #[tokio::test]
    async fn stdio_subagent_server_runs_concurrent_delegates_and_cancels_inflight() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        use tokio::net::UnixListener;

        let dir = std::env::temp_dir().join(format!("hc-mcp-sub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("sub.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        // Fake daemon socket server:
        // - role "slow" sleeps 120ms then answers
        // - role "fast" answers immediately (proving concurrent out-of-order completion!)
        // - role "hang" waits until client disconnects (proving notifications/cancelled closes socket!)
        let (hang_connected_tx, hang_connected_rx) = tokio::sync::oneshot::channel::<()>();
        let (hang_disconnected_tx, hang_disconnected_rx) = tokio::sync::oneshot::channel::<()>();
        let hang_connected_tx = Arc::new(Mutex::new(Some(hang_connected_tx)));
        let hang_disconnected_tx = Arc::new(Mutex::new(Some(hang_disconnected_tx)));

        let server_task = tokio::spawn({
            let hang_connected_tx = Arc::clone(&hang_connected_tx);
            let hang_disconnected_tx = Arc::clone(&hang_disconnected_tx);
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let hang_connected_tx = Arc::clone(&hang_connected_tx);
                    let hang_disconnected_tx = Arc::clone(&hang_disconnected_tx);
                    tokio::spawn(async move {
                        let (r, mut w) = stream.into_split();
                        let mut reader = tokio::io::BufReader::new(r);
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let req: Request = serde_json::from_str(line.trim()).unwrap();
                        let Request::Delegate { role, prompt, .. } = req else {
                            panic!("unexpected request: {req:?}");
                        };
                        match role.as_str() {
                            "slow" => {
                                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                                let mut out = crate::protocol::encode(&Reply::Answer {
                                    id: 1,
                                    text: format!("done:{prompt}"),
                                    denied: vec![],
                                    suppressed: false,
                                    widgets: vec![],
                                    conversation_id: None,
                                    profile: None,
                                    tool_icons: vec![],
                                });
                                out.push('\n');
                                let _ = w.write_all(out.as_bytes()).await;
                            }
                            "fast" => {
                                let mut out = crate::protocol::encode(&Reply::Answer {
                                    id: 2,
                                    text: format!("quick:{prompt}"),
                                    denied: vec!["sudo id".to_string()],
                                    suppressed: false,
                                    widgets: vec![],
                                    conversation_id: None,
                                    profile: None,
                                    tool_icons: vec![],
                                });
                                out.push('\n');
                                let _ = w.write_all(out.as_bytes()).await;
                            }
                            "hang" => {
                                if let Some(tx) = hang_connected_tx.lock().unwrap().take() {
                                    let _ = tx.send(());
                                }
                                let mut buf = [0u8; 32];
                                let _ = reader.read(&mut buf).await;
                                if let Some(tx) = hang_disconnected_tx.lock().unwrap().take() {
                                    let _ = tx.send(());
                                }
                            }
                            other => panic!("unexpected role {other}"),
                        }
                    });
                }
            }
        });

        let (mut client_in_w, server_in_r) = tokio::io::duplex(16 * 1024);
        let (server_out_w, client_out_r) = tokio::io::duplex(16 * 1024);
        let sock_clone = sock_path.clone();
        let mcp_task = tokio::spawn(async move {
            serve_subagent_stdio(
                tokio::io::BufReader::new(server_in_r),
                server_out_w,
                Some(sock_clone),
            )
            .await
        });

        let mut out_lines = tokio::io::BufReader::new(client_out_r).lines();

        // Initialize & tools/list.
        for msg in [
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        ] {
            let line = format!("{msg}\n");
            client_in_w.write_all(line.as_bytes()).await.unwrap();
        }
        let init_resp: Value =
            serde_json::from_str(&out_lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(
            init_resp["result"]["serverInfo"]["name"],
            "hey-computer-mcp-subagent"
        );
        let list_resp: Value =
            serde_json::from_str(&out_lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(list_resp["result"]["tools"][0]["name"], "delegate");

        // Issue slow, fast, and hang concurrently!
        for msg in [
            json!({"jsonrpc": "2.0", "id": "c-slow", "method": "tools/call", "params": {"name": "delegate", "arguments": {"role": "slow", "prompt": "p1"}}}),
            json!({"jsonrpc": "2.0", "id": "c-fast", "method": "tools/call", "params": {"name": "delegate", "arguments": {"role": "fast", "prompt": "p2"}}}),
            json!({"jsonrpc": "2.0", "id": "c-hang", "method": "tools/call", "params": {"name": "delegate", "arguments": {"role": "hang", "prompt": "p3"}}}),
        ] {
            let line = format!("{msg}\n");
            client_in_w.write_all(line.as_bytes()).await.unwrap();
        }

        // Wait until "hang" has actually connected to the daemon socket, then cancel it.
        tokio::time::timeout(std::time::Duration::from_secs(5), hang_connected_rx)
            .await
            .unwrap()
            .unwrap();
        let cancel_msg = format!(
            "{}\n",
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": "c-hang"}})
        );
        client_in_w.write_all(cancel_msg.as_bytes()).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), hang_disconnected_rx)
            .await
            .unwrap()
            .unwrap();

        // Both fast and slow complete; fast finishes first despite being sent after slow!
        let r1: Value =
            serde_json::from_str(&out_lines.next_line().await.unwrap().unwrap()).unwrap();
        let r2: Value =
            serde_json::from_str(&out_lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(r1["id"], "c-fast");
        let r1_body: Value =
            serde_json::from_str(r1["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(r1_body["ok"], true);
        assert_eq!(r1_body["answer"], "quick:p2");
        assert_eq!(r1_body["denied"], json!(["sudo id"]));

        assert_eq!(r2["id"], "c-slow");
        let r2_body: Value =
            serde_json::from_str(r2["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(r2_body["ok"], true);
        assert_eq!(r2_body["answer"], "done:p1");

        drop(client_in_w);
        assert!(out_lines.next_line().await.unwrap().is_none());
        mcp_task.await.unwrap().unwrap();
        server_task.abort();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
