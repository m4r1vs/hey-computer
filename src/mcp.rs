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

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufRead, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::config::{Config, McpServer};
use crate::protocol::WidgetSpec;

/// Name of the internal stdio MCP server exposing `show_widget` and
/// `propose_widget_template` to non-restricted lanes.
pub const INTERNAL_UI_MCP_SERVER: &str = "mcp-ui";

/// Supported `kind` values for `show_widget`.
pub const WIDGET_KINDS: &[&str] = &[
    "calendar_agenda",
    "transit_board",
    "tasks_journal",
    "dev_triage",
    "hue_rooms",
    "distill_diff",
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

/// Build the `{ "mcpServers": { ... } }` document written to
/// `<agent_dir>/config/mcp_config.json`.
///
/// When `mcp_browser` is provided, stdio servers that do not already configure
/// `BROWSER` receive `BROWSER=<mcp_browser>` so OAuth browser launches cannot
/// inherit the MCP server's JSON-RPC `stdout` pipe and crash the connection.
///
/// Returns `None` when no enabled servers resolve cleanly. Servers whose secret
/// files are missing or unreadable are skipped with a warning (never logging
/// secret values) so a missing optional token does not break the daemon.
pub fn build_mcp_config(config: &Config, mcp_browser: Option<&Path>) -> Option<Value> {
    let browser_env = mcp_browser.and_then(usable_browser_path);
    let mut servers = Map::new();
    servers.insert(
        INTERNAL_UI_MCP_SERVER.to_string(),
        json!({
            "type": "stdio",
            "command": resolve_mcp_ui_command(),
            "args": ["mcp-ui"],
            "enabled": true,
        }),
    );
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

        let empty = Config::default();
        let empty_doc = build_mcp_config(&empty, None).expect("auto-registers mcp-ui");
        let empty_servers = empty_doc["mcpServers"].as_object().unwrap();
        assert_eq!(empty_servers.len(), 1, "{empty_servers:?}");
        assert_eq!(empty_servers["mcp-ui"]["type"], "stdio");
        assert_eq!(empty_servers["mcp-ui"]["args"], json!(["mcp-ui"]));
        assert_eq!(empty_servers["mcp-ui"]["enabled"], true);

        let browser = Path::new("/tmp/hc-mcp-browser");
        let doc = build_mcp_config(&config, Some(browser)).expect("mcp config built");
        let servers = doc["mcpServers"].as_object().unwrap();
        assert_eq!(servers.len(), 3, "{servers:?}");
        assert_eq!(servers["mcp-ui"]["type"], "stdio");
        assert_eq!(servers["mcp-ui"]["args"], json!(["mcp-ui"]));
        assert_eq!(servers["mcp-ui"]["enabled"], true);
        assert!(servers["mcp-ui"].get("env").is_none());
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
        let doc = build_mcp_config(&config, Some(browser)).unwrap();
        assert_eq!(
            doc["mcpServers"]["nixos"]["env"]["BROWSER"],
            "/custom/browser"
        );
        config.mcp.get_mut("nixos").unwrap().env.remove("BROWSER");
        let doc = build_mcp_config(&config, Some(Path::new("/tmp/has space/mcp-browser"))).unwrap();
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
}
