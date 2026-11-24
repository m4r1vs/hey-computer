//! MCP server resolution for ACP `session/new` `mcpServers`.
//!
//! Secret environment variables (`env_files`) and HTTP headers (`header_files`)
//! are read from disk at session-creation time so credentials never appear in
//! `config.toml` or `/nix/store`. Resolved values are never logged.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::config::{Config, McpServer};

/// Maximum bytes read from a single secret file (`env_files` / `header_files`).
const MAX_SECRET_BYTES: usize = 16 * 1024;

/// Build the ACP `mcpServers` array for `session/new`.
///
/// Servers whose secret files are missing or unreadable are skipped with a
/// warning (never logging secret values) so a missing optional token does not
/// break the daemon.
pub fn resolve_acp_servers(config: &Config, names: &[String], allow_mcp: bool) -> Vec<Value> {
    if !allow_mcp || names.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for name in names {
        let Some(server) = config.mcp.get(name) else {
            continue;
        };
        if !server.enabled {
            continue;
        }
        match build_server_value(name, server) {
            Ok(val) => out.push(val),
            Err(e) => {
                tracing::warn!("skipping MCP server `{name}`: {e:#}");
            }
        }
    }
    out
}

fn build_server_value(name: &str, server: &McpServer) -> Result<Value> {
    if !server.command.is_empty() {
        let env = resolve_kv_entries(name, "env", &server.env, &server.env_files)?;
        return Ok(json!({
            "name": name,
            "command": server.command,
            "args": server.args,
            "env": env,
        }));
    }
    let transport = if server.transport == "sse" {
        "sse"
    } else {
        "http"
    };
    let headers = resolve_kv_entries(name, "header", &server.headers, &server.header_files)?;
    Ok(json!({
        "type": transport,
        "name": name,
        "url": server.url,
        "headers": headers,
    }))
}

fn resolve_kv_entries(
    server_name: &str,
    kind: &str,
    inline: &BTreeMap<String, String>,
    files: &BTreeMap<String, PathBuf>,
) -> Result<Vec<Value>> {
    let mut entries = Vec::with_capacity(inline.len() + files.len());
    for (k, v) in inline {
        entries.push(json!({"name": k, "value": v}));
    }
    for (k, path) in files {
        let secret = read_secret_file(path).with_context(|| {
            format!("reading {kind} file for `{k}` on MCP server `{server_name}`")
        })?;
        entries.push(json!({"name": k, "value": secret}));
    }
    Ok(entries)
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

        let names = vec![
            "nixos".to_string(),
            "remote".to_string(),
            "broken".to_string(),
        ];
        // Restricted lanes get nothing.
        assert!(resolve_acp_servers(&config, &names, false).is_empty());

        let resolved = resolve_acp_servers(&config, &names, true);
        assert_eq!(resolved.len(), 2, "{resolved:?}");
        assert_eq!(resolved[0]["name"], "nixos");
        assert_eq!(resolved[0]["command"], "/bin/mcp-nixos");
        assert_eq!(
            resolved[0]["env"],
            json!([{"name": "PLAIN", "value": "1"}, {"name": "TOKEN", "value": "secret-value"}])
        );
        assert_eq!(resolved[1]["type"], "http");
        assert_eq!(resolved[1]["url"], "https://mcp.example.test/mcp");
        assert_eq!(
            resolved[1]["headers"],
            json!([{"name": "Authorization", "value": "secret-value"}])
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
