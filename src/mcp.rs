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
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::config::{Config, McpServer};

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
    file.by_ref()
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
        assert!(build_mcp_config(&empty, None).is_none());

        let browser = Path::new("/tmp/hc-mcp-browser");
        let doc = build_mcp_config(&config, Some(browser)).expect("mcp config built");
        let servers = doc["mcpServers"].as_object().unwrap();
        assert_eq!(servers.len(), 2, "{servers:?}");
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
}
