//! Signal adapter: supervises `signal-cli daemon` over a private `0700` UNIX
//! socket in strictly read-only mode.
//!
//! Spawns:
//! `signal-cli --config <state>/signal -a <e164> daemon --socket <0700_runtime>/signal.sock --no-receive-stdout --receive-mode manual --ignore-stories --ignore-attachments`
//!
//! Calls ONLY read-only JSON-RPC methods: `version`, `listContacts`,
//! `listGroups`, and `subscribeReceive`. Never passes `--send-read-receipts` and
//! never implements or invokes `send`, `sendReceipt`, `sendTyping`, or `sendReaction`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::{mpsc, watch};

use crate::bridge::config::{
    BridgeConfig, PairingLockGuard, ensure_private_dir, is_pairing_in_progress, normalize_e164,
    normalize_identifier, resolve_signal_account,
};
use crate::bridge::context::{InboundRawMessage, RawAdapterEvent};
use crate::bridge::protocol::{MAX_BRIDGE_LINE, Platform, PlatformState};
use crate::bridge::store::ChatMetaRecord;
use crate::lines::{self, Line};

/// Safe environment variables retained for the scrubbed JVM environment.
const ALLOWED_JVM_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
    "JAVA_HOME",
    "JAVA_OPTS",
];

fn signal_program(config: &BridgeConfig) -> (String, Vec<String>) {
    if let Some((first, rest)) = config.signal.command.split_first() {
        (first.clone(), rest.to_vec())
    } else {
        ("signal-cli".to_string(), Vec::new())
    }
}

fn signal_config_dir(config: &BridgeConfig) -> Result<PathBuf> {
    let dir = config.resolved_state_dir()?.join("signal");
    ensure_private_dir(&dir)?;
    Ok(dir)
}

fn internal_socket_path(config: &BridgeConfig) -> Result<PathBuf> {
    // Keep the internal signal-cli socket inside a private 0700 directory so
    // no other local user can connect, and scope it per state_dir hash so
    // parallel test instances never collide.
    let base = crate::ipc::runtime_dir()?.join("hey-computer-bridge-internal");
    ensure_private_dir(&base)?;
    let state_key = config.resolved_state_dir()?;
    let hash = fnv1a64(state_key.to_string_lossy().as_bytes());
    let dir = base.join(format!("{hash:016x}"));
    ensure_private_dir(&dir)?;
    Ok(dir.join("signal.sock"))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Local contact and group directory populated from read-only `listContacts` and
/// `listGroups` RPCs.
#[derive(Debug, Default)]
pub struct ContactDirectory {
    own_e164: String,
    own_uuid: Option<String>,
    uuid_to_e164: HashMap<String, String>,
    e164_to_uuid: HashMap<String, String>,
    names: HashMap<String, String>,
    groups: HashMap<String, String>,
}

impl ContactDirectory {
    pub fn new(own_e164: String) -> Self {
        Self {
            own_e164,
            own_uuid: None,
            uuid_to_e164: HashMap::new(),
            e164_to_uuid: HashMap::new(),
            names: HashMap::new(),
            groups: HashMap::new(),
        }
    }

    pub fn ingest_contacts(&mut self, value: &Value) -> Vec<RawAdapterEvent> {
        let mut events = Vec::new();
        let Some(items) = value.as_array() else {
            return events;
        };
        for item in items {
            let number = item
                .get("number")
                .and_then(|v| v.as_str())
                .and_then(normalize_e164);
            let uuid = item
                .get("uuid")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(normalize_identifier);
            let name = contact_display_name(item);

            if let (Some(num), Some(u)) = (&number, &uuid) {
                if *num == self.own_e164 {
                    self.own_uuid = Some(u.clone());
                }
                self.uuid_to_e164.insert(u.clone(), num.clone());
                self.e164_to_uuid.insert(num.clone(), u.clone());
            }
            if let Some(n) = &name {
                if let Some(num) = &number {
                    self.names.insert(num.clone(), n.clone());
                }
                if let Some(u) = &uuid {
                    self.names.insert(u.clone(), n.clone());
                }
            }
            if let Some(chat_id) = number.clone().or_else(|| uuid.clone()) {
                let mut aliases = Vec::new();
                if let Some(num) = number {
                    aliases.push(num);
                }
                if let Some(u) = uuid {
                    aliases.push(u);
                }
                events.push(RawAdapterEvent::ContactAlias {
                    platform: Platform::Signal,
                    chat_id,
                    aliases,
                    display_name: name,
                });
            }
        }
        events
    }

    pub fn ingest_groups(&mut self, value: &Value) -> Vec<ChatMetaRecord> {
        let mut records = Vec::new();
        let Some(items) = value.as_array() else {
            return records;
        };
        for item in items {
            let Some(raw_id) = item
                .get("id")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let chat_id = normalize_identifier(&format!("group:{raw_id}"));
            let title = item
                .get("name")
                .or_else(|| item.get("title"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string);

            if let Some(t) = &title {
                self.groups.insert(chat_id.clone(), t.clone());
            }

            records.push(ChatMetaRecord {
                platform: Platform::Signal,
                chat_id: chat_id.clone(),
                aliases: vec![chat_id],
                is_group: true,
                is_self_chat: false,
                title,
                raw_jid: None,
            });
        }
        records
    }

    fn matches_own(&self, e164: Option<&str>, uuid: Option<&str>) -> bool {
        if let Some(num) = e164
            && num != self.own_e164
        {
            return false;
        }
        if let (Some(u), Some(own_u)) = (uuid, self.own_uuid.as_deref())
            && u != own_u
        {
            return false;
        }
        let e164_hit = e164.is_some_and(|n| n == self.own_e164);
        let uuid_hit = uuid.is_some_and(|u| self.own_uuid.as_deref() == Some(u));
        e164_hit || uuid_hit
    }

    fn resolve_peer(
        &self,
        raw_e164: Option<&str>,
        raw_uuid: Option<&str>,
        fallback_name: Option<&str>,
    ) -> (String, Vec<String>, String) {
        let mut e164 = raw_e164.and_then(normalize_e164);
        let mut uuid = raw_uuid
            .filter(|s| !s.trim().is_empty())
            .map(normalize_identifier);

        if e164.is_none()
            && let Some(u) = &uuid
        {
            e164 = self.uuid_to_e164.get(u).cloned();
        }
        if uuid.is_none()
            && let Some(n) = &e164
        {
            uuid = self.e164_to_uuid.get(n).cloned();
        }

        let canonical = e164
            .clone()
            .or_else(|| uuid.clone())
            .unwrap_or_else(|| "unknown".to_string());

        let mut aliases = Vec::new();
        if let Some(n) = &e164 {
            aliases.push(n.clone());
        }
        if let Some(u) = &uuid {
            aliases.push(u.clone());
        }

        let name = e164
            .as_ref()
            .and_then(|n| self.names.get(n))
            .or_else(|| uuid.as_ref().and_then(|u| self.names.get(u)))
            .cloned()
            .or_else(|| {
                fallback_name
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string)
            })
            .unwrap_or_default();

        (canonical, aliases, name)
    }
}

fn contact_display_name(item: &Value) -> Option<String> {
    for key in ["name", "nickName", "givenName"] {
        if let Some(s) = item.get(key).and_then(|v| v.as_str()) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    if let Some(profile) = item.get("profile") {
        let given = profile
            .get("givenName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let family = profile
            .get("familyName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let combined = format!("{given} {family}").trim().to_string();
        if !combined.is_empty() {
            return Some(combined);
        }
    }
    None
}

/// Supervise `signal-cli daemon` over its private UNIX socket until `stop` is set.
pub async fn run(
    config: BridgeConfig,
    tx: mpsc::Sender<RawAdapterEvent>,
    mut stop: watch::Receiver<bool>,
) {
    let cfg_dir = match signal_config_dir(&config) {
        Ok(dir) => dir,
        Err(e) => {
            let _ = tx
                .send(RawAdapterEvent::PlatformStateChanged {
                    platform: Platform::Signal,
                    state: PlatformState::Reconnecting,
                    detail: Some(format!("failed to prepare signal state dir: {e:#}")),
                })
                .await;
            return;
        }
    };

    let sock_path = match internal_socket_path(&config) {
        Ok(p) => p,
        Err(e) => {
            let _ = tx
                .send(RawAdapterEvent::PlatformStateChanged {
                    platform: Platform::Signal,
                    state: PlatformState::Reconnecting,
                    detail: Some(format!("failed to prepare signal socket dir: {e:#}")),
                })
                .await;
            return;
        }
    };

    let (program, base_args) = signal_program(&config);
    let uses_default_cli = config.signal.command.is_empty();
    let accounts_json = cfg_dir.join("data").join("accounts.json");
    let mut backoff_secs = 2u64;

    loop {
        if *stop.borrow() {
            break;
        }

        let mut resolved_account = match resolve_signal_account(&config.signal, Some(&cfg_dir)) {
            Ok(acct) => acct,
            Err(e) => {
                let _ = tx
                    .send(RawAdapterEvent::PlatformStateChanged {
                        platform: Platform::Signal,
                        state: PlatformState::AuthRequired,
                        detail: Some(format!("Signal account configuration error: {e:#}")),
                    })
                    .await;
                return;
            }
        };

        if is_pairing_in_progress(&cfg_dir)
            || resolved_account.is_none()
            || (uses_default_cli && !accounts_json.is_file())
        {
            let _ = tx
                .send(RawAdapterEvent::PlatformStateChanged {
                    platform: Platform::Signal,
                    state: PlatformState::AuthRequired,
                    detail: Some(
                        "Signal device not linked yet — run `hey-computer-bridge pair signal --name hey-computer`."
                            .to_string(),
                    ),
                })
                .await;
            while !*stop.borrow() {
                if wait_or_stop(Duration::from_secs(2), &mut stop).await {
                    return;
                }
                if is_pairing_in_progress(&cfg_dir) {
                    continue;
                }
                if uses_default_cli && !accounts_json.is_file() {
                    continue;
                }
                if let Ok(Some(acct)) = resolve_signal_account(&config.signal, Some(&cfg_dir)) {
                    resolved_account = Some(acct);
                    break;
                }
            }
            if *stop.borrow() {
                break;
            }
        }

        let Some(own_e164) = resolved_account else {
            continue;
        };
        let accounts_mtime_before = std::fs::metadata(&accounts_json)
            .and_then(|m| m.modified())
            .ok();

        let _ = std::fs::remove_file(&sock_path);
        let _ = tx
            .send(RawAdapterEvent::PlatformStateChanged {
                platform: Platform::Signal,
                state: PlatformState::Starting,
                detail: None,
            })
            .await;

        let mut cmd = Command::new(&program);
        cmd.env_clear();
        for key in ALLOWED_JVM_ENV {
            if let Some(val) = std::env::var_os(key) {
                cmd.env(key, val);
            }
        }
        cmd.args(&base_args)
            .arg("--config")
            .arg(&cfg_dir)
            .arg("-a")
            .arg(&own_e164)
            .arg("daemon")
            .arg("--socket")
            .arg(&sock_path)
            .arg("--no-receive-stdout")
            .arg("--receive-mode")
            .arg("manual")
            .arg("--ignore-stories")
            .arg("--ignore-attachments")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        unsafe {
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("failed to spawn signal-cli ({program}): {e}");
                let _ = tx
                    .send(RawAdapterEvent::PlatformStateChanged {
                        platform: Platform::Signal,
                        state: PlatformState::Reconnecting,
                        detail: Some(format!("failed to spawn {program}: {e}")),
                    })
                    .await;
                if wait_or_stop(Duration::from_secs(backoff_secs), &mut stop).await {
                    break;
                }
                backoff_secs = (backoff_secs * 2).min(60);
                continue;
            }
        };

        let child_pid = child.id();
        let auth_flag = Arc::new(AtomicBool::new(false));
        let auth_flag_stderr = Arc::clone(&auth_flag);
        let stderr = child.stderr.take().expect("stderr piped");
        let stderr_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            while let Ok(Line::Text(line)) = lines::read(&mut reader, 16 * 1024).await {
                if line.contains("User is not registered")
                    || line.contains("Not registered")
                    || line.contains("Authorization failed")
                    || line.contains("401")
                {
                    auth_flag_stderr.store(true, Ordering::Relaxed);
                }
                tracing::debug!(platform = "signal", stderr = %line);
            }
        });

        let stream = match wait_for_signal_socket(&sock_path, &mut child, &mut stop).await {
            Ok(Some(stream)) => stream,
            Ok(None) => {
                terminate_process_group(child_pid);
                let _ = child.kill().await;
                let _ = child.wait().await;
                stderr_task.abort();
                let _ = std::fs::remove_file(&sock_path);
                return;
            }
            Err(e) => {
                terminate_process_group(child_pid);
                let _ = child.kill().await;
                let _ = child.wait().await;
                stderr_task.abort();
                let _ = std::fs::remove_file(&sock_path);

                if auth_flag.load(Ordering::Relaxed) {
                    let _ = tx
                        .send(RawAdapterEvent::PlatformStateChanged {
                            platform: Platform::Signal,
                            state: PlatformState::AuthRequired,
                            detail: Some(
                                "Signal account is not linked; run `hey-computer-bridge pair signal --name hey-computer`."
                                    .to_string(),
                            ),
                        })
                        .await;
                    while !*stop.borrow() {
                        if wait_or_stop(Duration::from_secs(2), &mut stop).await {
                            return;
                        }
                        if is_pairing_in_progress(&cfg_dir) {
                            continue;
                        }
                        let mtime_now = std::fs::metadata(&accounts_json)
                            .and_then(|m| m.modified())
                            .ok();
                        if mtime_now.is_some() && mtime_now != accounts_mtime_before {
                            backoff_secs = 2;
                            break;
                        }
                    }
                    continue;
                }

                tracing::warn!("signal-cli socket failed to become ready: {e:#}");
                let _ = tx
                    .send(RawAdapterEvent::PlatformStateChanged {
                        platform: Platform::Signal,
                        state: PlatformState::Reconnecting,
                        detail: Some(format!(
                            "signal-cli startup failed; retrying in {backoff_secs}s"
                        )),
                    })
                    .await;
                if wait_or_stop(Duration::from_secs(backoff_secs), &mut stop).await {
                    break;
                }
                backoff_secs = (backoff_secs * 2).min(60);
                continue;
            }
        };

        let run_result = drive_signal_session(
            stream,
            &own_e164,
            config.signal.groups_enabled,
            &tx,
            &mut stop,
            &mut backoff_secs,
        )
        .await;

        terminate_process_group(child_pid);
        let _ = child.kill().await;
        let _ = child.wait().await;
        stderr_task.abort();
        let _ = std::fs::remove_file(&sock_path);

        if *stop.borrow() {
            break;
        }
        if auth_flag.load(Ordering::Relaxed) {
            let _ = tx
                .send(RawAdapterEvent::PlatformStateChanged {
                    platform: Platform::Signal,
                    state: PlatformState::AuthRequired,
                    detail: Some(
                        "Signal session unauthorized (401); run `hey-computer-bridge pair signal`."
                            .to_string(),
                    ),
                })
                .await;
            while !*stop.borrow() {
                if stop.changed().await.is_err() {
                    break;
                }
            }
            break;
        }

        if let Err(e) = run_result {
            tracing::warn!(retry_secs = backoff_secs, "signal session ended: {e:#}");
        }
        let _ = tx
            .send(RawAdapterEvent::PlatformStateChanged {
                platform: Platform::Signal,
                state: PlatformState::Reconnecting,
                detail: Some(format!(
                    "signal-cli disconnected; retrying in {backoff_secs}s"
                )),
            })
            .await;
        if wait_or_stop(Duration::from_secs(backoff_secs), &mut stop).await {
            break;
        }
        backoff_secs = (backoff_secs * 2).min(60);
    }
}

async fn wait_for_signal_socket(
    sock_path: &Path,
    child: &mut tokio::process::Child,
    stop: &mut watch::Receiver<bool>,
) -> Result<Option<UnixStream>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if *stop.borrow() {
            return Ok(None);
        }
        if let Ok(stream) = UnixStream::connect(sock_path).await {
            return Ok(Some(stream));
        }
        if let Some(status) = child.try_wait()? {
            bail!("signal-cli exited before opening socket ({status})");
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out waiting for signal-cli socket");
        }
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
            _ = stop.changed() => {
                if *stop.borrow() {
                    return Ok(None);
                }
            }
        }
    }
}

async fn drive_signal_session(
    stream: UnixStream,
    own_e164: &str,
    groups_enabled: bool,
    tx: &mpsc::Sender<RawAdapterEvent>,
    stop: &mut watch::Receiver<bool>,
    backoff_secs: &mut u64,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);

    // Strictly read-only JSON-RPC calls: version, listContacts, listGroups, subscribeReceive.
    send_rpc(&mut write, "1", "version", json!({})).await?;
    let _version = read_rpc_response(&mut reader, "1", stop).await?;

    let mut contacts = ContactDirectory::new(own_e164.to_string());
    send_rpc(
        &mut write,
        "2",
        "listContacts",
        json!({ "account": own_e164 }),
    )
    .await?;
    if let Ok(Some(result)) = read_rpc_response(&mut reader, "2", stop).await {
        for ev in contacts.ingest_contacts(&result) {
            let _ = tx.send(ev).await;
        }
    }

    if groups_enabled {
        send_rpc(
            &mut write,
            "2g",
            "listGroups",
            json!({ "account": own_e164 }),
        )
        .await?;
        match read_rpc_response(&mut reader, "2g", stop).await {
            Ok(Some(result)) => {
                let group_records = contacts.ingest_groups(&result);
                if !group_records.is_empty() {
                    let _ = tx.send(RawAdapterEvent::ChatMetaBatch(group_records)).await;
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!("signal-cli listGroups returned non-fatal error: {e:#}");
            }
        }
    }

    send_rpc(
        &mut write,
        "3",
        "subscribeReceive",
        json!({ "account": own_e164 }),
    )
    .await?;
    let _sub = read_rpc_response(&mut reader, "3", stop).await?;

    *backoff_secs = 2;
    let _ = tx
        .send(RawAdapterEvent::PlatformStateChanged {
            platform: Platform::Signal,
            state: PlatformState::Connected,
            detail: None,
        })
        .await;

    loop {
        tokio::select! {
            _ = stop.changed() => {
                if *stop.borrow() {
                    return Ok(());
                }
            }
            line = lines::read(&mut reader, MAX_BRIDGE_LINE) => match line {
                Ok(Line::Text(raw)) => {
                    if raw.trim().is_empty() {
                        continue;
                    }
                    let Ok(val) = serde_json::from_str::<Value>(&raw) else {
                        tracing::debug!("ignoring non-JSON line from signal-cli");
                        continue;
                    };
                    if val.get("method").and_then(|v| v.as_str()) != Some("receive") {
                        continue;
                    }
                    let Some(envelope) = val
                        .get("params")
                        .and_then(|p| p.get("result").or(Some(p)))
                        .and_then(|p| p.get("envelope"))
                    else {
                        continue;
                    };
                    if let Some(msg) = parse_signal_envelope(envelope, &mut contacts) {
                        let _ = tx.send(RawAdapterEvent::Message(Box::new(msg))).await;
                    }
                }
                Ok(Line::TooLong(n)) => {
                    tracing::warn!(bytes = n, "dropping oversized signal-cli line");
                }
                Ok(Line::NotUtf8) => {
                    tracing::warn!("dropping non-UTF-8 signal-cli line");
                }
                Ok(Line::Eof) => bail!("signal-cli socket closed"),
                Err(e) => return Err(e).context("reading signal-cli socket"),
            }
        }
    }
}

async fn send_rpc(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    id: &str,
    method: &str,
    params: Value,
) -> Result<()> {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let mut line = serde_json::to_string(&payload)?;
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    write.flush().await?;
    Ok(())
}

async fn read_rpc_response(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    expected_id: &str,
    stop: &mut watch::Receiver<bool>,
) -> Result<Option<Value>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for signal-cli RPC response {expected_id}");
        }
        tokio::select! {
            _ = stop.changed() => {
                if *stop.borrow() {
                    return Ok(None);
                }
            }
            res = tokio::time::timeout(remaining, lines::read(reader, MAX_BRIDGE_LINE)) => {
                let line = res.context("signal-cli RPC timeout")??;
                match line {
                    Line::Text(raw) => {
                        let Ok(val) = serde_json::from_str::<Value>(&raw) else {
                            continue;
                        };
                        let id_matches = val.get("id").is_some_and(|v| {
                            v.as_str() == Some(expected_id)
                                || v.as_i64().map(|n| n.to_string()).as_deref() == Some(expected_id)
                        });
                        if !id_matches {
                            continue;
                        }
                        if let Some(err) = val.get("error") {
                            bail!("signal-cli RPC {expected_id} failed: {err}");
                        }
                        return Ok(val.get("result").cloned());
                    }
                    Line::Eof => bail!("signal-cli closed socket during RPC {expected_id}"),
                    Line::TooLong(_) | Line::NotUtf8 => continue,
                }
            }
        }
    }
}

/// Parse one Signal `envelope` JSON object into an `InboundRawMessage`.
///
/// - Verified Note-to-Self: `syncMessage.sentMessage` where `source` == own AND
///   `destination` == own AND `groupInfo` is absent.
/// - Outbound to group: `syncMessage.sentMessage` where `source` == own AND
///   `groupInfo` is present (`from_me = true, is_group = true, is_self_chat = false`).
/// - Outbound to another contact: `syncMessage.sentMessage` where `source` ==
///   own AND `destination` != own AND `groupInfo` is absent (`from_me = true`,
///   `is_self_chat = false`).
/// - Inbound DM/group: `dataMessage`.
pub fn parse_signal_envelope(
    envelope: &Value,
    contacts: &mut ContactDirectory,
) -> Option<InboundRawMessage> {
    let source_e164 = envelope
        .get("sourceNumber")
        .or_else(|| envelope.get("source"))
        .and_then(|v| v.as_str())
        .and_then(normalize_e164);
    let source_uuid = envelope
        .get("sourceUuid")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(normalize_identifier);
    let source_name = envelope.get("sourceName").and_then(|v| v.as_str());
    let env_ts = envelope
        .get("timestamp")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(crate::bridge::context::unix_timestamp_ms);

    // Opportunistically learn own_uuid when we see our own sourceNumber + sourceUuid.
    if source_e164.as_deref() == Some(contacts.own_e164.as_str())
        && let Some(u) = &source_uuid
        && contacts.own_uuid.is_none()
    {
        contacts.own_uuid = Some(u.clone());
    }

    // Case 1: syncMessage.sentMessage (Note to Self, outbound group reply, OR outbound DM reply)
    if let Some(sent) = envelope
        .get("syncMessage")
        .and_then(|s| s.get("sentMessage"))
    {
        // Only accept sync messages cryptographically sourced from own account.
        if !contacts.matches_own(source_e164.as_deref(), source_uuid.as_deref()) {
            return None;
        }

        let text = sent
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let quoted_text = extract_quote_text(sent);
        let media_placeholder = extract_attachment_placeholder(sent);
        if text.is_empty() && media_placeholder.is_none() {
            return None;
        }
        let ts = sent
            .get("timestamp")
            .and_then(|v| v.as_i64())
            .unwrap_or(env_ts);

        // Outbound message sent to a Signal group from the phone / linked Desktop.
        if let Some(gi) = sent.get("groupInfo").filter(|v| !v.is_null()) {
            let gid = gi
                .get("groupId")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())?;
            let chat_id = normalize_identifier(&format!("group:{gid}"));
            let group_title =
                extract_group_title(gi).or_else(|| contacts.groups.get(&chat_id).cloned());
            if let Some(t) = &group_title {
                contacts.groups.insert(chat_id.clone(), t.clone());
            }
            return Some(InboundRawMessage {
                id: format!("sync-grp-{ts}-{gid}"),
                platform: Platform::Signal,
                chat_id: chat_id.clone(),
                chat_aliases: vec![chat_id],
                raw_jid: None,
                chat_title: group_title,
                sender_id: contacts.own_e164.clone(),
                sender_aliases: vec![contacts.own_e164.clone()],
                sender_name: "Me".to_string(),
                from_me: true,
                is_group: true,
                is_self_chat: false,
                timestamp_ms: ts,
                text,
                quoted_text,
                media_placeholder,
            });
        }

        let dest_e164 = sent
            .get("destinationNumber")
            .or_else(|| sent.get("destination"))
            .and_then(|v| v.as_str())
            .and_then(normalize_e164);
        let dest_uuid = sent
            .get("destinationUuid")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(normalize_identifier);

        let is_self_chat = contacts.matches_own(dest_e164.as_deref(), dest_uuid.as_deref());
        if is_self_chat {
            let mut aliases = vec![contacts.own_e164.clone()];
            if let Some(u) = &contacts.own_uuid {
                aliases.push(u.clone());
            }
            return Some(InboundRawMessage {
                id: format!("sync-{ts}"),
                platform: Platform::Signal,
                chat_id: contacts.own_e164.clone(),
                chat_aliases: aliases.clone(),
                raw_jid: None,
                chat_title: Some("Note to Self".to_string()),
                sender_id: contacts.own_e164.clone(),
                sender_aliases: aliases,
                sender_name: "Me".to_string(),
                from_me: true,
                is_group: false,
                is_self_chat: true,
                timestamp_ms: ts,
                text,
                quoted_text,
                media_placeholder,
            });
        }

        // Outbound message sent to another contact -> record as author: "me" + user_replied.
        let (chat_id, chat_aliases, display_name) =
            contacts.resolve_peer(dest_e164.as_deref(), dest_uuid.as_deref(), None);
        if chat_id == "unknown" {
            return None;
        }
        let chat_title = (!display_name.is_empty()).then(|| display_name.clone());
        return Some(InboundRawMessage {
            id: format!("sync-out-{ts}-{chat_id}"),
            platform: Platform::Signal,
            chat_id,
            chat_aliases,
            raw_jid: None,
            chat_title,
            sender_id: contacts.own_e164.clone(),
            sender_aliases: vec![contacts.own_e164.clone()],
            sender_name: display_name,
            from_me: true,
            is_group: false,
            is_self_chat: false,
            timestamp_ms: ts,
            text,
            quoted_text,
            media_placeholder,
        });
    }

    // Case 2: inbound dataMessage
    if let Some(data) = envelope.get("dataMessage") {
        let text = data
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let quoted_text = extract_quote_text(data);
        let media_placeholder = extract_attachment_placeholder(data);
        if text.is_empty() && media_placeholder.is_none() {
            return None;
        }
        let ts = data
            .get("timestamp")
            .and_then(|v| v.as_i64())
            .unwrap_or(env_ts);

        let (sender_id, sender_aliases, sender_name) =
            contacts.resolve_peer(source_e164.as_deref(), source_uuid.as_deref(), source_name);

        let group_info = data.get("groupInfo").filter(|v| !v.is_null());
        let (is_group, chat_id, chat_aliases, chat_title) = if let Some(gi) = group_info {
            let gid = gi
                .get("groupId")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("unknown");
            let cid = normalize_identifier(&format!("group:{gid}"));
            let gtitle = extract_group_title(gi).or_else(|| contacts.groups.get(&cid).cloned());
            if let Some(t) = &gtitle {
                contacts.groups.insert(cid.clone(), t.clone());
            }
            (true, cid.clone(), vec![cid], gtitle)
        } else {
            let dm_title = (!sender_name.is_empty()).then(|| sender_name.clone());
            (false, sender_id.clone(), sender_aliases.clone(), dm_title)
        };

        return Some(InboundRawMessage {
            id: format!("data-{ts}-{sender_id}"),
            platform: Platform::Signal,
            chat_id,
            chat_aliases,
            raw_jid: None,
            chat_title,
            sender_id,
            sender_aliases,
            sender_name,
            from_me: false,
            is_group,
            is_self_chat: false,
            timestamp_ms: ts,
            text,
            quoted_text,
            media_placeholder,
        });
    }

    None
}

fn extract_group_title(gi: &Value) -> Option<String> {
    for key in ["groupName", "name", "title"] {
        if let Some(s) = gi.get(key).and_then(|v| v.as_str()) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn extract_quote_text(msg: &Value) -> Option<String> {
    msg.get("quote")
        .and_then(|q| q.get("text"))
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn extract_attachment_placeholder(msg: &Value) -> Option<String> {
    let first = msg.get("attachments")?.as_array()?.first()?;
    let ct = first
        .get("contentType")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_voice = first
        .get("voiceNote")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if is_voice || ct.starts_with("audio/") {
        Some("[voice note]".to_string())
    } else if ct.starts_with("image/") {
        Some("[image]".to_string())
    } else if ct.starts_with("video/") {
        Some("[video]".to_string())
    } else {
        Some("[attachment]".to_string())
    }
}

/// Interactive one-time linked-device pairing for Signal
/// (`hey-computer-bridge pair signal --name hey-computer`).
pub async fn pair_signal(config: &BridgeConfig, device_name: &str) -> Result<()> {
    let cfg_dir = signal_config_dir(config)?;
    let _pair_lock = PairingLockGuard::acquire(&cfg_dir);
    let (program, base_args) = signal_program(config);

    let mut child = Command::new(&program)
        .args(&base_args)
        .arg("--config")
        .arg(&cfg_dir)
        .arg("link")
        .arg("-n")
        .arg(device_name)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawning {program} link"))?;

    let stdout = child.stdout.take().expect("stdout piped");
    let mut reader = BufReader::new(stdout);
    if let Ok(Line::Text(uri)) = lines::read(&mut reader, 16 * 1024).await {
        let uri = uri.trim();
        if !uri.is_empty() {
            eprintln!(
                "Scan this Signal linked-device QR code (Signal -> Settings -> Linked Devices):"
            );
            // Try `qrencode -t ANSIUTF8` when available, otherwise print the provisioning URI.
            let rendered = Command::new("qrencode")
                .args(["-t", "ANSIUTF8", "-o", "-"])
                .arg(uri)
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::null())
                .status()
                .await
                .is_ok_and(|s| s.success());
            if !rendered {
                eprintln!("{uri}");
            }
        }
    }

    let status = child.wait().await.context("waiting for signal-cli link")?;
    if !status.success() {
        bail!("Signal pairing failed with status {status}");
    }

    let verify = Command::new(&program)
        .args(&base_args)
        .arg("--config")
        .arg(&cfg_dir)
        .arg("listAccounts")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("running {program} listAccounts"))?;
    if !verify.success() {
        bail!("Signal pairing completed, but listAccounts failed with status {verify}");
    }
    Ok(())
}

async fn wait_or_stop(delay: Duration, stop: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        () = tokio::time::sleep(delay) => false,
        _ = stop.changed() => *stop.borrow(),
    }
}

fn terminate_process_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // SAFETY: sending SIGTERM to our own child's process group.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_note_to_self_group_sync_and_distinguishes_outbound_to_other() {
        let mut dir = ContactDirectory::new("+4915100000000".to_string());
        dir.ingest_contacts(&json!([
            { "number": "+4915100000000", "uuid": "11111111-1111-1111-1111-111111111111" },
            { "number": "+4915100000001", "uuid": "22222222-2222-2222-2222-222222222222", "name": "Alice Test" }
        ]));
        let groups = dir.ingest_groups(&json!([
            { "id": "grp-flatmates-1", "name": "Flatmates Group" }
        ]));
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].chat_id, "group:grp-flatmates-1");
        assert_eq!(groups[0].title.as_deref(), Some("Flatmates Group"));

        // Verified Note to Self
        let self_env = json!({
            "sourceNumber": "+4915100000000",
            "sourceUuid": "11111111-1111-1111-1111-111111111111",
            "timestamp": 1764109200000i64,
            "syncMessage": {
                "sentMessage": {
                    "destinationNumber": "+4915100000000",
                    "timestamp": 1764109200000i64,
                    "message": "Remind me to buy milk"
                }
            }
        });
        let parsed = parse_signal_envelope(&self_env, &mut dir).unwrap();
        assert!(parsed.from_me);
        assert!(parsed.is_self_chat);
        assert!(!parsed.is_group);
        assert_eq!(parsed.text, "Remind me to buy milk");

        // Spoofed syncMessage from another source must be rejected.
        let spoof_env = json!({
            "sourceNumber": "+4915100000001",
            "timestamp": 1764109201000i64,
            "syncMessage": {
                "sentMessage": {
                    "destinationNumber": "+4915100000000",
                    "timestamp": 1764109201000i64,
                    "message": "spoof"
                }
            }
        });
        assert!(parse_signal_envelope(&spoof_env, &mut dir).is_none());

        // Outbound message sent to Alice Test (not self-chat).
        let outbound_env = json!({
            "sourceNumber": "+4915100000000",
            "timestamp": 1764109202000i64,
            "syncMessage": {
                "sentMessage": {
                    "destinationUuid": "22222222-2222-2222-2222-222222222222",
                    "timestamp": 1764109202000i64,
                    "message": "On my way!"
                }
            }
        });
        let outbound = parse_signal_envelope(&outbound_env, &mut dir).unwrap();
        assert!(outbound.from_me);
        assert!(!outbound.is_self_chat);
        assert_eq!(outbound.chat_id, "+4915100000001");
        assert_eq!(outbound.sender_name, "Alice Test");
        assert_eq!(outbound.chat_title.as_deref(), Some("Alice Test"));

        // Outbound syncMessage sent to a Signal group resolves group title from listGroups!
        let grp_out_env = json!({
            "sourceNumber": "+4915100000000",
            "timestamp": 1764109203000i64,
            "syncMessage": {
                "sentMessage": {
                    "groupInfo": { "groupId": "grp-flatmates-1" },
                    "timestamp": 1764109203000i64,
                    "message": "Bringing pizza!"
                }
            }
        });
        let grp_out = parse_signal_envelope(&grp_out_env, &mut dir).unwrap();
        assert!(grp_out.from_me);
        assert!(grp_out.is_group);
        assert!(!grp_out.is_self_chat);
        assert_eq!(grp_out.chat_id, "group:grp-flatmates-1");
        assert_eq!(grp_out.chat_title.as_deref(), Some("Flatmates Group"));
        assert_eq!(grp_out.sender_name, "Me");

        // Inbound dataMessage in Signal group keeps speaker name separate from group title!
        let grp_in_env = json!({
            "sourceUuid": "22222222-2222-2222-2222-222222222222",
            "timestamp": 1764109204000i64,
            "dataMessage": {
                "groupInfo": { "groupId": "grp-flatmates-1" },
                "timestamp": 1764109204000i64,
                "message": "Awesome!"
            }
        });
        let grp_in = parse_signal_envelope(&grp_in_env, &mut dir).unwrap();
        assert!(!grp_in.from_me);
        assert!(grp_in.is_group);
        assert_eq!(grp_in.chat_id, "group:grp-flatmates-1");
        assert_eq!(grp_in.chat_title.as_deref(), Some("Flatmates Group"));
        assert_eq!(grp_in.sender_name, "Alice Test");
    }
}
