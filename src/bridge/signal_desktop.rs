//! Strictly read-only Signal Desktop SQLCipher importer (`src/bridge/signal_desktop.rs`).
//!
//! Snapshots `<desktop_dir>/sql/db.sqlite` (+ `-wal` / `-shm`) into a private
//! `0700` scratch directory with an RAII `Drop` guard, passes `PRAGMA key` over
//! `sqlcipher -readonly -json` stdin (never in `argv` or logs), and imports
//! 1:1 + group conversations and messages incrementally into `BridgeStore`.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::bridge::config::{
    BridgeConfig, ensure_private_dir, normalize_e164, normalize_identifier, resolve_signal_account,
};
use crate::bridge::protocol::{ChatSummary, Platform};
use crate::bridge::store::{BridgeStore, ChatMetaRecord, StoredMessageInput};

const MAX_DESKTOP_CONFIG_BYTES: u64 = 256 * 1024;
const WATERMARK_KEY: &str = "signal_desktop_watermark_ms";
const WATERMARK_OVERLAP_MS: i64 = 60_000;
const BATCH_CHUNK_SIZE: usize = 500;

static SNAP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalDesktopSyncSummary {
    pub conversations: usize,
    pub messages_imported: usize,
}

struct SnapshotGuard {
    dir: PathBuf,
}

impl SnapshotGuard {
    fn create(base_parent: &Path, prefix: &str) -> Result<(Self, PathBuf)> {
        ensure_private_dir(base_parent)?;
        let seq = SNAP_SEQ.fetch_add(1, Ordering::Relaxed);
        let snap_dir = base_parent.join(format!("{prefix}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&snap_dir);
        ensure_private_dir(&snap_dir)?;
        let guard = Self {
            dir: snap_dir.clone(),
        };
        Ok((guard, snap_dir))
    }

    fn copy_db_files(&self, src_db: &Path) -> Result<PathBuf> {
        let dst_db = self.dir.join("db.sqlite");
        copy_private_file(src_db, &dst_db)?;
        for suffix in ["-wal", "-shm"] {
            let src_sidecar = PathBuf::from(format!("{}{suffix}", src_db.display()));
            if src_sidecar.is_file() {
                let dst_sidecar = self.dir.join(format!("db.sqlite{suffix}"));
                let _ = copy_private_file(&src_sidecar, &dst_sidecar);
            }
        }
        Ok(dst_db)
    }
}

impl Drop for SnapshotGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn copy_private_file(src: &Path, dst: &Path) -> Result<()> {
    std::fs::copy(src, dst)
        .with_context(|| format!("copying {} to private snapshot", src.display()))?;
    std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0600 {}", dst.display()))?;
    Ok(())
}

/// Read and strictly validate the 64-character hex SQLCipher key from
/// `<desktop_dir>/config.json` without logging secret material.
fn read_validated_hex_key(config_json_path: &Path) -> Result<String> {
    let meta = std::fs::metadata(config_json_path)
        .with_context(|| format!("checking {}", config_json_path.display()))?;
    if !meta.is_file() {
        bail!("{} is not a regular file", config_json_path.display());
    }
    if meta.len() > MAX_DESKTOP_CONFIG_BYTES {
        bail!(
            "{} exceeds {} bytes",
            config_json_path.display(),
            MAX_DESKTOP_CONFIG_BYTES
        );
    }
    let raw = std::fs::read_to_string(config_json_path)
        .with_context(|| format!("reading {}", config_json_path.display()))?;
    let parsed: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", config_json_path.display()))?;
    let Some(key) = parsed.get("key").and_then(|v| v.as_str()) else {
        bail!(
            "{} has no plaintext 'key' field (SafeStorage encryptedKey is unsupported)",
            config_json_path.display()
        );
    };
    let trimmed = key.trim();
    if trimmed.len() != 64 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("Signal Desktop 'key' must be exactly 64 ASCII hexadecimal characters");
    }
    Ok(trimmed.to_ascii_lowercase())
}

fn sqlcipher_program(override_cmd: &[String]) -> (&str, &[String]) {
    if let Some((first, rest)) = override_cmd.split_first() {
        (first.as_str(), rest)
    } else {
        ("sqlcipher", &[])
    }
}

/// Execute a read-only SQL query against the snapshotted SQLCipher database,
/// feeding `PRAGMA key` on stdin (never via `argv`).
fn run_sqlcipher_json(
    snap_db: &Path,
    hex_key: &str,
    sqlcipher_cmd: &[String],
    sql: &str,
) -> Result<Vec<Value>> {
    // Defense-in-depth re-check before interpolating into `PRAGMA key`.
    if hex_key.len() != 64 || !hex_key.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("refusing to execute sqlcipher with non-hex key");
    }
    let (program, base_args) = sqlcipher_program(sqlcipher_cmd);
    let mut child = Command::new(program)
        .args(base_args)
        .arg("-readonly")
        .arg("-json")
        .arg(snap_db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {program} -readonly -json"))?;

    {
        let mut stdin = child.stdin.take().context("sqlcipher stdin unavailable")?;
        let script = format!("PRAGMA key = \"x'{hex_key}'\";\n{sql}\n");
        stdin
            .write_all(script.as_bytes())
            .context("writing query to sqlcipher stdin")?;
        stdin.flush().context("flushing sqlcipher stdin")?;
    }

    let out = child
        .wait_with_output()
        .context("waiting for sqlcipher process")?;
    if !out.status.success() {
        let stderr_summary = String::from_utf8_lossy(&out.stderr);
        let first_line = stderr_summary.lines().next().unwrap_or("unknown error");
        bail!(
            "{program} exited with {}: {}",
            out.status,
            first_line.trim()
        );
    }

    let stdout = String::from_utf8(out.stdout).context("sqlcipher output was not valid UTF-8")?;
    let mut rows = Vec::new();
    for arr_res in serde_json::Deserializer::from_str(&stdout).into_iter::<Vec<Value>>() {
        let arr = arr_res.context("parsing sqlcipher -json output")?;
        // SQLCipher 4 prints `[{"ok":"ok"}]` in response to `PRAGMA key`; skip it.
        if arr.len() == 1
            && arr[0]
                .as_object()
                .is_some_and(|o| o.len() == 1 && o.get("ok").and_then(|v| v.as_str()) == Some("ok"))
        {
            continue;
        }
        rows.extend(arr);
    }
    Ok(rows)
}

#[derive(Debug, Clone)]
struct ConvInfo {
    chat_id: String,
    aliases: Vec<String>,
    is_group: bool,
    is_self_chat: bool,
    title: String,
    active_at_ms: i64,
}

fn val_str<'a>(obj: &'a Value, key: &str) -> Option<&'a str> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn val_i64(obj: &Value, key: &str) -> Option<i64> {
    let v = obj.get(key)?;
    v.as_i64()
        .or_else(|| v.as_f64().map(|f| f as i64))
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
}

fn build_conv_directory(
    conv_rows: &[Value],
    own_e164: Option<&str>,
) -> (HashMap<String, ConvInfo>, HashMap<String, String>) {
    let mut convs = HashMap::new();
    let mut peer_names: HashMap<String, String> = HashMap::new();

    for row in conv_rows {
        let Some(conv_id) = val_str(row, "id") else {
            continue;
        };
        let conv_type = val_str(row, "type").unwrap_or("");
        let active_at_ms = val_i64(row, "active_at").unwrap_or(0);

        if conv_type == "group" {
            let gid = val_str(row, "groupId").unwrap_or(conv_id);
            let chat_id = normalize_identifier(&format!("group:{gid}"));
            let title = val_str(row, "name").unwrap_or("").to_string();
            convs.insert(
                conv_id.to_string(),
                ConvInfo {
                    chat_id: chat_id.clone(),
                    aliases: vec![chat_id],
                    is_group: true,
                    is_self_chat: false,
                    title,
                    active_at_ms,
                },
            );
            continue;
        }

        if conv_type == "private" {
            let e164 = val_str(row, "e164").and_then(normalize_e164);
            let uuid = val_str(row, "serviceId")
                .or_else(|| val_str(row, "uuid"))
                .map(normalize_identifier);

            let Some(chat_id) = e164.clone().or_else(|| uuid.clone()) else {
                continue;
            };
            let mut aliases = Vec::new();
            if let Some(n) = &e164 {
                aliases.push(n.clone());
            }
            if let Some(u) = &uuid {
                aliases.push(u.clone());
            }

            let raw_name = val_str(row, "name")
                .map(ToString::to_string)
                .or_else(|| val_str(row, "profileFullName").map(ToString::to_string))
                .or_else(|| {
                    let first = val_str(row, "profileName").unwrap_or("");
                    let last = val_str(row, "profileFamilyName").unwrap_or("");
                    let combined = format!("{first} {last}").trim().to_string();
                    (!combined.is_empty()).then_some(combined)
                })
                .unwrap_or_default();

            let is_self_chat = own_e164.is_some_and(|own| e164.as_deref() == Some(own))
                || raw_name.eq_ignore_ascii_case("Note to Self");

            let title = if is_self_chat && raw_name.is_empty() {
                "Note to Self".to_string()
            } else {
                raw_name
            };

            if !title.is_empty() && !is_self_chat {
                if let Some(n) = &e164 {
                    peer_names.insert(n.clone(), title.clone());
                }
                if let Some(u) = &uuid {
                    peer_names.insert(u.clone(), title.clone());
                }
            }

            convs.insert(
                conv_id.to_string(),
                ConvInfo {
                    chat_id,
                    aliases,
                    is_group: false,
                    is_self_chat,
                    title,
                    active_at_ms,
                },
            );
        }
    }

    (convs, peer_names)
}

fn discover_own_e164_from_desktop(
    snap_db: &Path,
    hex_key: &str,
    sqlcipher_cmd: &[String],
) -> Option<String> {
    let rows = run_sqlcipher_json(
        snap_db,
        hex_key,
        sqlcipher_cmd,
        "SELECT json FROM items WHERE id = 'number_id' LIMIT 1;",
    )
    .ok()?;
    let json_s = rows.first().and_then(|r| val_str(r, "json"))?;
    let parsed: Value = serde_json::from_str(json_s).ok()?;
    let val = parsed.get("value")?.as_str()?;
    let bare = val.split('.').next().unwrap_or(val);
    normalize_e164(bare)
}

fn query_conversations(
    snap_db: &Path,
    hex_key: &str,
    sqlcipher_cmd: &[String],
) -> Result<Vec<Value>> {
    let primary = "SELECT id, type, name, profileFullName, profileName, profileFamilyName, e164, serviceId, groupId, active_at FROM conversations;";
    match run_sqlcipher_json(snap_db, hex_key, sqlcipher_cmd, primary) {
        Ok(rows) => Ok(rows),
        Err(_) => run_sqlcipher_json(
            snap_db,
            hex_key,
            sqlcipher_cmd,
            "SELECT id, type, name, profileFullName, profileName, profileFamilyName, e164, uuid AS serviceId, groupId, active_at FROM conversations;",
        ),
    }
}

/// Standalone read-only Signal Desktop group inspector so `hey-computer-bridge groups`
/// can list all Signal groups directly from `~/.config/Signal` even before a
/// daemon rebuild or initial `history.db` sync.
pub fn list_signal_desktop_groups(
    desktop_dir: &Path,
    sqlcipher_cmd: &[String],
) -> Result<Vec<ChatSummary>> {
    let config_json = desktop_dir.join("config.json");
    let db_path = desktop_dir.join("sql").join("db.sqlite");
    if !config_json.is_file() || !db_path.is_file() {
        return Ok(Vec::new());
    }

    let hex_key = read_validated_hex_key(&config_json)?;
    let snap_parent = crate::ipc::runtime_dir().unwrap_or_else(|_| std::env::temp_dir());
    let (guard, _) = SnapshotGuard::create(&snap_parent, "hey-computer-sig-snap")?;
    let snap_db = guard.copy_db_files(&db_path)?;

    let conv_rows = query_conversations(&snap_db, &hex_key, sqlcipher_cmd)?;
    let (convs, _) = build_conv_directory(&conv_rows, None);

    let mut groups: Vec<ChatSummary> = convs
        .into_values()
        .filter(|c| c.is_group)
        .map(|c| ChatSummary {
            platform: Platform::Signal,
            chat_id: c.chat_id,
            is_group: true,
            is_self_chat: false,
            title: c.title,
            last_message_ms: c.active_at_ms,
            message_count: 0,
            raw_jid: None,
        })
        .collect();
    groups.sort_by(|a, b| {
        b.last_message_ms
            .cmp(&a.last_message_ms)
            .then_with(|| a.title.cmp(&b.title))
    });
    Ok(groups)
}

/// Incrementally import Signal Desktop conversations and messages into `BridgeStore`.
/// Returns `Ok(default)` quietly if Signal Desktop is not present on disk.
pub fn sync_signal_desktop(
    config: &BridgeConfig,
    store: &mut BridgeStore,
) -> Result<SignalDesktopSyncSummary> {
    let Some(desktop_dir) = config.resolved_signal_desktop_dir() else {
        return Ok(SignalDesktopSyncSummary::default());
    };
    let config_json = desktop_dir.join("config.json");
    let db_path = desktop_dir.join("sql").join("db.sqlite");
    if !config_json.is_file() || !db_path.is_file() {
        return Ok(SignalDesktopSyncSummary::default());
    }

    let hex_key = read_validated_hex_key(&config_json)?;
    let state_dir = config.resolved_state_dir()?;
    let signal_state_dir = state_dir.join("signal");
    let (guard, _) = SnapshotGuard::create(&signal_state_dir, ".import-snapshot")?;
    let snap_db = guard.copy_db_files(&db_path)?;

    let configured_own = resolve_signal_account(&config.signal, Some(&signal_state_dir))
        .ok()
        .flatten()
        .or_else(|| {
            discover_own_e164_from_desktop(&snap_db, &hex_key, &config.signal.sqlcipher_command)
        });

    let conv_rows = query_conversations(&snap_db, &hex_key, &config.signal.sqlcipher_command)?;
    let (convs, peer_names) = build_conv_directory(&conv_rows, configured_own.as_deref());

    let meta_batch: Vec<ChatMetaRecord> = convs
        .values()
        .filter(|c| !c.is_group || config.signal.groups_enabled)
        .map(|c| ChatMetaRecord {
            platform: Platform::Signal,
            chat_id: c.chat_id.clone(),
            aliases: c.aliases.clone(),
            is_group: c.is_group,
            is_self_chat: c.is_self_chat,
            title: (!c.title.is_empty()).then(|| c.title.clone()),
            raw_jid: None,
        })
        .collect();
    let conversations_count = meta_batch.len();
    store.upsert_chat_meta_batch(&meta_batch)?;

    let prev_watermark: i64 = store
        .get_sync_state(WATERMARK_KEY)?
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    let cutoff = prev_watermark.saturating_sub(WATERMARK_OVERLAP_MS).max(0);

    let msg_sql = format!(
        "SELECT id, conversationId, type, sent_at, received_at, source, sourceServiceId, body, hasAttachments, json_extract(json, '$.quote.text') AS quote_text \
         FROM messages \
         WHERE COALESCE(isErased, 0) = 0 \
           AND storyId IS NULL \
           AND COALESCE(sent_at, received_at, 0) >= {cutoff} \
         ORDER BY COALESCE(sent_at, received_at, 0) ASC;"
    );
    let msg_rows = match run_sqlcipher_json(
        &snap_db,
        &hex_key,
        &config.signal.sqlcipher_command,
        &msg_sql,
    ) {
        Ok(rows) => rows,
        Err(_) => {
            let fallback_sql = format!(
                "SELECT id, conversationId, type, sent_at, received_at, source, sourceUuid AS sourceServiceId, body, hasAttachments, json_extract(json, '$.quote.text') AS quote_text \
                 FROM messages \
                 WHERE COALESCE(sent_at, received_at, 0) >= {cutoff} \
                 ORDER BY COALESCE(sent_at, received_at, 0) ASC;"
            );
            run_sqlcipher_json(
                &snap_db,
                &hex_key,
                &config.signal.sqlcipher_command,
                &fallback_sql,
            )?
        }
    };

    let mut max_ts = prev_watermark;
    let mut batch: Vec<StoredMessageInput> =
        Vec::with_capacity(msg_rows.len().min(BATCH_CHUNK_SIZE));
    let mut messages_imported = 0usize;

    for row in &msg_rows {
        let Some(msg_id) = val_str(row, "id") else {
            continue;
        };
        let Some(conv_id) = val_str(row, "conversationId") else {
            continue;
        };
        let Some(conv) = convs.get(conv_id) else {
            continue;
        };
        if conv.is_group && !config.signal.groups_enabled {
            continue;
        }

        let msg_type = val_str(row, "type").unwrap_or("");
        let from_me = match msg_type {
            "outgoing" | "sent" => true,
            "incoming" => false,
            _ => continue,
        };

        let ts_ms = val_i64(row, "sent_at")
            .filter(|&t| t > 0)
            .or_else(|| val_i64(row, "received_at").filter(|&t| t > 0))
            .unwrap_or(0);
        if ts_ms <= 0 {
            continue;
        }
        if ts_ms > max_ts {
            max_ts = ts_ms;
        }

        let body = val_str(row, "body").unwrap_or("").to_string();
        let has_attachments = val_i64(row, "hasAttachments").unwrap_or(0) != 0;
        if body.is_empty() && !has_attachments {
            continue;
        }
        let media_placeholder =
            (body.is_empty() && has_attachments).then(|| "[attachment]".to_string());
        let quoted_text = val_str(row, "quote_text").map(ToString::to_string);

        let (sender_id, sender_name) = if from_me {
            let sid = configured_own.clone().unwrap_or_default();
            (sid, "Me".to_string())
        } else {
            let src_e164 = val_str(row, "source").and_then(normalize_e164);
            let src_uuid = val_str(row, "sourceServiceId").map(normalize_identifier);
            let sname = src_e164
                .as_ref()
                .and_then(|n| peer_names.get(n))
                .or_else(|| src_uuid.as_ref().and_then(|u| peer_names.get(u)))
                .cloned()
                .or_else(|| (!conv.is_group && !conv.title.is_empty()).then(|| conv.title.clone()))
                .unwrap_or_default();
            let sid = src_e164
                .or(src_uuid)
                .unwrap_or_else(|| conv.chat_id.clone());
            (sid, sname)
        };

        batch.push(StoredMessageInput {
            platform: Platform::Signal,
            chat_id: conv.chat_id.clone(),
            chat_aliases: conv.aliases.clone(),
            raw_jid: None,
            msg_id: msg_id.to_string(),
            timestamp_ms: ts_ms,
            from_me,
            is_group: conv.is_group,
            is_self_chat: from_me && conv.is_self_chat,
            chat_title: (!conv.title.is_empty()).then(|| conv.title.clone()),
            sender_id,
            sender_name,
            text: body,
            quoted_text,
            media_placeholder,
        });

        if batch.len() >= BATCH_CHUNK_SIZE {
            messages_imported += store.upsert_messages_batch(&batch, config.max_stored_messages)?;
            batch.clear();
        }
    }

    if !batch.is_empty() {
        messages_imported += store.upsert_messages_batch(&batch, config.max_stored_messages)?;
    }

    if max_ts > prev_watermark {
        store.set_sync_state(WATERMARK_KEY, &max_ts.to_string())?;
    }

    Ok(SignalDesktopSyncSummary {
        conversations: conversations_count,
        messages_imported,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hc-sigdesk-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rejects_non_hex_keys_before_spawning_sqlcipher() {
        let dir = scratch("bad-key");
        let cfg_path = dir.join("config.json");
        std::fs::write(&cfg_path, r#"{"key":"0123\"; DROP TABLE x; --"}"#).unwrap();
        assert!(read_validated_hex_key(&cfg_path).is_err());

        std::fs::write(&cfg_path, r#"{"key":"0123456789abcdef"}"#).unwrap();
        assert!(read_validated_hex_key(&cfg_path).is_err());

        let valid = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        std::fs::write(&cfg_path, format!(r#"{{"key":"{valid}"}}"#)).unwrap();
        assert_eq!(read_validated_hex_key(&cfg_path).unwrap(), valid);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn imports_encrypted_signal_desktop_db_and_cleans_up_snapshot() {
        let sqlcipher_bin = if Command::new("sqlcipher")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            "sqlcipher".to_string()
        } else {
            // Skip only if sqlcipher is absent outside nix develop; integration test covers mock fallback too.
            return;
        };

        let dir = scratch("encrypted");
        let desktop_dir = dir.join("Signal");
        let sql_dir = desktop_dir.join("sql");
        std::fs::create_dir_all(&sql_dir).unwrap();

        let key = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
        std::fs::write(
            desktop_dir.join("config.json"),
            format!(r#"{{"key":"{key}"}}"#),
        )
        .unwrap();

        let db_file = sql_dir.join("db.sqlite");
        let mut init = Command::new(&sqlcipher_bin)
            .arg(&db_file)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let schema_and_seed = format!(
            "PRAGMA key = \"x'{key}'\";
             CREATE TABLE items (id TEXT PRIMARY KEY, json TEXT);
             INSERT INTO items VALUES ('number_id', '{{\"value\":\"+4915100000000.1\"}}');
             CREATE TABLE conversations (
                 id TEXT PRIMARY KEY, type TEXT, name TEXT, profileFullName TEXT,
                 profileName TEXT, profileFamilyName TEXT, e164 TEXT,
                 serviceId TEXT, groupId TEXT, active_at INTEGER
             );
             INSERT INTO conversations VALUES
                 ('c-alice', 'private', 'Alice Test', NULL, NULL, NULL, '+4915100000001', '22222222-2222-2222-2222-222222222222', NULL, 1764100002000),
                 ('c-bob', 'private', NULL, 'Bob Member', NULL, NULL, '+4915100000002', '33333333-3333-3333-3333-333333333333', NULL, 1764100001000),
                 ('c-grp', 'group', 'Flatmates Group', NULL, NULL, NULL, NULL, NULL, 'grp-flatmates-v2', 1764100005000);
             CREATE TABLE messages (
                 id TEXT PRIMARY KEY, conversationId TEXT, type TEXT,
                 sent_at INTEGER, received_at INTEGER, source TEXT,
                 sourceServiceId TEXT, body TEXT, hasAttachments INTEGER,
                 isErased INTEGER, storyId TEXT, json TEXT
             );
             INSERT INTO messages VALUES
                 ('m1', 'c-alice', 'incoming', 1764100001000, 1764100001100, '+4915100000001', '22222222-2222-2222-2222-222222222222', 'Lunch tomorrow?', 0, 0, NULL, '{{}}'),
                 ('m2', 'c-alice', 'outgoing', 1764100002000, 1764100002000, NULL, NULL, 'Yes at 12:30!', 0, 0, NULL, '{{\"quote\":{{\"text\":\"Lunch tomorrow?\"}}}}'),
                 ('m3', 'c-grp', 'incoming', 1764100005000, 1764100005100, NULL, '33333333-3333-3333-3333-333333333333', 'Rent sent!', 0, 0, NULL, '{{}}'),
                 ('m4-empty', 'c-grp', 'keychange', 1764100006000, 1764100006000, NULL, NULL, '', 0, 0, NULL, '{{}}');\n"
        );
        init.stdin
            .take()
            .unwrap()
            .write_all(schema_and_seed.as_bytes())
            .unwrap();
        assert!(init.wait().unwrap().success());

        // Standalone pre-rebuild group listing works without BridgeStore.
        let groups =
            list_signal_desktop_groups(&desktop_dir, std::slice::from_ref(&sqlcipher_bin)).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].title, "Flatmates Group");
        assert_eq!(groups[0].chat_id, "group:grp-flatmates-v2");

        // Full incremental import into BridgeStore.
        let state_dir = dir.join("state");
        let mut cfg = BridgeConfig {
            state_dir: state_dir.clone(),
            ..BridgeConfig::default()
        };
        cfg.signal.desktop_dir = Some(desktop_dir);
        cfg.signal.sqlcipher_command = vec![sqlcipher_bin];

        let mut store = BridgeStore::open(&state_dir.join("history.db")).unwrap();
        let summary1 = sync_signal_desktop(&cfg, &mut store).unwrap();
        assert_eq!(summary1.conversations, 3);
        assert_eq!(summary1.messages_imported, 3);

        // Second sync is idempotent (0 newly imported messages).
        let summary2 = sync_signal_desktop(&cfg, &mut store).unwrap();
        assert_eq!(summary2.messages_imported, 0);

        // Group title stayed "Flatmates Group" while speaker resolved to "Bob Member".
        let (grp, grp_msgs) = store
            .get_messages(Some(Platform::Signal), "Flatmates Group", None, None, 10)
            .unwrap();
        assert_eq!(grp.unwrap().title, "Flatmates Group");
        assert_eq!(grp_msgs.len(), 1);
        assert_eq!(grp_msgs[0].sender_name, "Bob Member");
        assert_eq!(grp_msgs[0].text, "Rent sent!");

        // Snapshot directory was removed by Drop guard.
        let leftovers: Vec<_> = std::fs::read_dir(state_dir.join("signal"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("import-snapshot"))
            .collect();
        assert!(leftovers.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
