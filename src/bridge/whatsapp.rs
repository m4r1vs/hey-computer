//! WhatsApp adapter: supervises the strictly read-only `baileys-reader` helper.
//!
//! Spawns `baileys-reader` with piped stdout (for live messages, contact/group
//! metadata, and full history sync batches) and piped stdin accepting ONLY
//! `{"type":"fetch_history",...}` control requests for read-only paging.
//! On `401` (`auth_required`), transitions WhatsApp to `PlatformState::AuthRequired`
//! and stops respawning until `creds.json` is re-linked.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};

use crate::bridge::config::{
    BridgeConfig, PairingLockGuard, ensure_private_dir, is_pairing_in_progress, normalize_e164,
    normalize_identifier, normalize_jid,
};
use crate::bridge::context::{InboundRawMessage, RawAdapterEvent};
use crate::bridge::protocol::{MAX_BRIDGE_LINE, Platform, PlatformState};
use crate::bridge::store::ChatMetaRecord;
use crate::lines::{self, Line};

/// Strictly read-only control commands sent to `baileys-reader` over stdin.
#[derive(Debug, Clone)]
pub enum WhatsappControl {
    FetchHistory {
        chat_jid: String,
        oldest_msg_id: String,
        oldest_from_me: bool,
        oldest_ts_ms: i64,
        count: usize,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StdinControlPayload<'a> {
    FetchHistory {
        chat_jid: &'a str,
        oldest_msg_id: &'a str,
        oldest_from_me: bool,
        oldest_ts_ms: i64,
        count: usize,
    },
}

#[derive(Debug, Deserialize)]
struct BaileysChatMeta {
    raw_jid: String,
    #[serde(default)]
    resolved_e164: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    is_group: bool,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BaileysMessage {
    #[serde(default)]
    upsert_type: String,
    id: String,
    remote_jid: String,
    #[serde(default)]
    remote_jid_alt: Option<String>,
    #[serde(default)]
    participant: Option<String>,
    #[serde(default)]
    participant_alt: Option<String>,
    #[serde(default)]
    resolved_chat_e164: Option<String>,
    #[serde(default)]
    resolved_sender_e164: Option<String>,
    #[serde(default)]
    from_me: bool,
    #[serde(default)]
    is_group: bool,
    #[serde(default)]
    is_self_chat: bool,
    #[serde(default)]
    push_name: Option<String>,
    #[serde(default)]
    chat_title: Option<String>,
    timestamp_ms: i64,
    #[serde(default)]
    text: String,
    #[serde(default)]
    quoted_text: Option<String>,
    #[serde(default)]
    media_placeholder: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BaileysOutput {
    State {
        state: String,
        #[serde(default)]
        detail: Option<String>,
    },
    AuthRequired {
        #[serde(default)]
        detail: Option<String>,
    },
    Paired {
        #[serde(default)]
        self_jid: Option<String>,
        #[serde(default)]
        self_lid: Option<String>,
    },
    ChatMetaBatch {
        #[serde(default)]
        items: Vec<BaileysChatMeta>,
    },
    HistoryBatch {
        #[serde(default)]
        sync_type: String,
        #[serde(default)]
        progress: Option<f64>,
        #[serde(default)]
        is_latest: bool,
        #[serde(default)]
        messages: Vec<BaileysMessage>,
    },
    Message(Box<BaileysMessage>),
}

fn reader_program(config: &BridgeConfig) -> (String, Vec<String>) {
    if let Some((first, rest)) = config.whatsapp.reader_command.split_first() {
        (first.clone(), rest.to_vec())
    } else {
        ("hey-computer-baileys-reader".to_string(), Vec::new())
    }
}

fn whatsapp_session_dir(config: &BridgeConfig) -> Result<PathBuf> {
    let dir = config
        .resolved_state_dir()?
        .join("whatsapp")
        .join("session");
    ensure_private_dir(&dir)?;
    Ok(dir)
}

/// Supervise `baileys-reader` until `stop` becomes `true` or `auth_required`
/// pauses retries until re-pairing.
pub async fn run(
    config: BridgeConfig,
    tx: mpsc::Sender<RawAdapterEvent>,
    mut ctrl_rx: mpsc::Receiver<WhatsappControl>,
    mut stop: watch::Receiver<bool>,
) {
    let session_dir = match whatsapp_session_dir(&config) {
        Ok(dir) => dir,
        Err(e) => {
            let _ = tx
                .send(RawAdapterEvent::PlatformStateChanged {
                    platform: Platform::Whatsapp,
                    state: PlatformState::Reconnecting,
                    detail: Some(format!("failed to prepare whatsapp session dir: {e:#}")),
                })
                .await;
            return;
        }
    };

    let (program, base_args) = reader_program(&config);
    let uses_default_reader = config.whatsapp.reader_command.is_empty();
    let creds_path = session_dir.join("creds.json");
    let mut backoff_secs = 2u64;

    loop {
        if *stop.borrow() {
            break;
        }

        if uses_default_reader && (!creds_path.is_file() || is_pairing_in_progress(&session_dir)) {
            let _ = tx
                .send(RawAdapterEvent::PlatformStateChanged {
                    platform: Platform::Whatsapp,
                    state: PlatformState::AuthRequired,
                    detail: Some(
                        "WhatsApp device not linked yet — run `hey-computer-bridge pair whatsapp`."
                            .to_string(),
                    ),
                })
                .await;
            while !*stop.borrow() && (!creds_path.is_file() || is_pairing_in_progress(&session_dir))
            {
                if wait_or_stop(Duration::from_secs(2), &mut stop).await {
                    return;
                }
            }
            if *stop.borrow() {
                break;
            }
        }

        let creds_mtime_before = std::fs::metadata(&creds_path)
            .and_then(|m| m.modified())
            .ok();

        let _ = tx
            .send(RawAdapterEvent::PlatformStateChanged {
                platform: Platform::Whatsapp,
                state: PlatformState::Starting,
                detail: None,
            })
            .await;

        let mut cmd = Command::new(&program);
        cmd.args(&base_args).arg("--session").arg(&session_dir);
        if !config.whatsapp.sync_full_history {
            cmd.arg("--no-full-history");
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // Put the helper in its own process group so shutdown tears it down cleanly.
        unsafe {
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("failed to spawn whatsapp reader ({program}): {e}");
                let _ = tx
                    .send(RawAdapterEvent::PlatformStateChanged {
                        platform: Platform::Whatsapp,
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
        let mut child_stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let stderr_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            while let Ok(Line::Text(line)) = lines::read(&mut reader, 16 * 1024).await {
                tracing::debug!(platform = "whatsapp", stderr = %line);
            }
        });

        let mut reader = BufReader::new(stdout);
        let mut auth_required = false;

        loop {
            tokio::select! {
                _ = stop.changed() => {
                    if *stop.borrow() {
                        drop(child_stdin);
                        terminate_process_group(child_pid);
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        stderr_task.abort();
                        return;
                    }
                }
                Some(ctrl) = ctrl_rx.recv() => {
                    match ctrl {
                        WhatsappControl::FetchHistory {
                            chat_jid,
                            oldest_msg_id,
                            oldest_from_me,
                            oldest_ts_ms,
                            count,
                        } => {
                            let payload = StdinControlPayload::FetchHistory {
                                chat_jid: &chat_jid,
                                oldest_msg_id: &oldest_msg_id,
                                oldest_from_me,
                                oldest_ts_ms,
                                count,
                            };
                            if let Ok(mut line) = serde_json::to_string(&payload) {
                                line.push('\n');
                                if let Err(e) = child_stdin.write_all(line.as_bytes()).await {
                                    tracing::debug!("failed writing fetch_history to baileys stdin: {e}");
                                } else {
                                    let _ = child_stdin.flush().await;
                                }
                            }
                        }
                    }
                }
                line = lines::read(&mut reader, MAX_BRIDGE_LINE) => match line {
                    Ok(Line::Text(raw)) => {
                        if raw.trim().is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<BaileysOutput>(&raw) {
                            Ok(BaileysOutput::State { state, detail }) => {
                                let mapped = match state.as_str() {
                                    "connected" => {
                                        backoff_secs = 2;
                                        PlatformState::Connected
                                    }
                                    "starting" => PlatformState::Starting,
                                    "auth_required" => {
                                        auth_required = true;
                                        PlatformState::AuthRequired
                                    }
                                    _ => PlatformState::Reconnecting,
                                };
                                let _ = tx.send(RawAdapterEvent::PlatformStateChanged {
                                    platform: Platform::Whatsapp,
                                    state: mapped,
                                    detail,
                                }).await;
                            }
                            Ok(BaileysOutput::AuthRequired { detail }) => {
                                auth_required = true;
                                let _ = tx.send(RawAdapterEvent::PlatformStateChanged {
                                    platform: Platform::Whatsapp,
                                    state: PlatformState::AuthRequired,
                                    detail: detail.or_else(|| Some(
                                        "WhatsApp session unlinked (401); run `hey-computer-bridge pair whatsapp`.".to_string(),
                                    )),
                                }).await;
                            }
                            Ok(BaileysOutput::Paired { self_jid, self_lid }) => {
                                tracing::debug!(?self_jid, ?self_lid, "baileys paired event");
                            }
                            Ok(BaileysOutput::ChatMetaBatch { items }) => {
                                let records: Vec<ChatMetaRecord> = items
                                    .into_iter()
                                    .filter_map(normalize_baileys_chat_meta)
                                    .collect();
                                if !records.is_empty() {
                                    let _ = tx.send(RawAdapterEvent::ChatMetaBatch(records)).await;
                                }
                            }
                            Ok(BaileysOutput::HistoryBatch {
                                sync_type,
                                progress,
                                is_latest,
                                messages,
                            }) => {
                                let pct = progress
                                    .map(|p| p.round().clamp(0.0, 100.0) as u8)
                                    .or_else(|| is_latest.then_some(100));
                                let normalized: Vec<InboundRawMessage> = messages
                                    .into_iter()
                                    .map(convert_baileys_message)
                                    .collect();
                                tracing::debug!(
                                    platform = "whatsapp",
                                    %sync_type,
                                    ?pct,
                                    is_latest,
                                    count = normalized.len(),
                                    "received whatsapp history batch"
                                );
                                let _ = tx
                                    .send(RawAdapterEvent::HistoryBatch {
                                        platform: Platform::Whatsapp,
                                        progress: pct,
                                        messages: normalized,
                                    })
                                    .await;
                            }
                            Ok(BaileysOutput::Message(msg)) => {
                                tracing::debug!(
                                    platform = "whatsapp",
                                    upsert_type = %msg.upsert_type,
                                    from_me = msg.from_me,
                                    is_group = msg.is_group,
                                    is_self_chat = msg.is_self_chat,
                                    "received whatsapp upsert"
                                );
                                let inbound = convert_baileys_message(*msg);
                                let _ = tx.send(RawAdapterEvent::Message(Box::new(inbound))).await;
                            }
                            Err(e) => {
                                tracing::debug!("ignoring unparseable baileys line: {e}");
                            }
                        }
                    }
                    Ok(Line::TooLong(n)) => {
                        tracing::warn!(bytes = n, "dropping oversized baileys stdout line");
                    }
                    Ok(Line::NotUtf8) => {
                        tracing::warn!("dropping non-UTF-8 baileys stdout line");
                    }
                    Ok(Line::Eof) | Err(_) => break,
                }
            }
        }

        drop(child_stdin);
        let status = child.wait().await;
        stderr_task.abort();

        if *stop.borrow() {
            break;
        }
        if auth_required {
            tracing::warn!(
                "whatsapp adapter paused due to auth_required (401); waiting for re-pair"
            );
            while !*stop.borrow() {
                if wait_or_stop(Duration::from_secs(2), &mut stop).await {
                    return;
                }
                if is_pairing_in_progress(&session_dir) {
                    continue;
                }
                let creds_mtime_now = std::fs::metadata(&creds_path)
                    .and_then(|m| m.modified())
                    .ok();
                if creds_mtime_now.is_some() && creds_mtime_now != creds_mtime_before {
                    backoff_secs = 2;
                    break;
                }
            }
            continue;
        }

        tracing::warn!(
            status = ?status.ok(),
            retry_secs = backoff_secs,
            "whatsapp reader exited; scheduling restart"
        );
        let _ = tx
            .send(RawAdapterEvent::PlatformStateChanged {
                platform: Platform::Whatsapp,
                state: PlatformState::Reconnecting,
                detail: Some(format!(
                    "baileys-reader exited; retrying in {backoff_secs}s"
                )),
            })
            .await;

        if wait_or_stop(Duration::from_secs(backoff_secs), &mut stop).await {
            break;
        }
        backoff_secs = (backoff_secs * 2).min(60);
    }
}

fn normalize_baileys_chat_meta(item: BaileysChatMeta) -> Option<ChatMetaRecord> {
    let norm_raw = normalize_jid(&item.raw_jid);
    if norm_raw.is_empty()
        || norm_raw == "status@broadcast"
        || norm_raw.ends_with("@broadcast")
        || norm_raw.ends_with("@newsletter")
    {
        return None;
    }
    let is_group = item.is_group || norm_raw.ends_with("@g.us");
    let e164 = if is_group {
        None
    } else {
        item.resolved_e164
            .as_deref()
            .and_then(normalize_e164)
            .or_else(|| normalize_e164(&norm_raw))
    };
    let chat_id = if is_group {
        normalize_identifier(&norm_raw)
    } else if let Some(num) = &e164 {
        num.clone()
    } else {
        normalize_identifier(&norm_raw)
    };

    let mut aliases = Vec::new();
    aliases.push(normalize_identifier(&norm_raw));
    if let Some(num) = &e164 {
        aliases.push(num.clone());
    }
    for a in item.aliases {
        if !a.trim().is_empty() {
            aliases.push(normalize_identifier(&a));
        }
    }

    Some(ChatMetaRecord {
        platform: Platform::Whatsapp,
        chat_id,
        aliases,
        is_group,
        is_self_chat: false,
        title: item
            .title
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty()),
        raw_jid: Some(norm_raw),
    })
}

fn convert_baileys_message(msg: BaileysMessage) -> InboundRawMessage {
    normalize_baileys_message(
        msg.id,
        msg.remote_jid,
        msg.remote_jid_alt,
        msg.participant,
        msg.participant_alt,
        msg.resolved_chat_e164,
        msg.resolved_sender_e164,
        msg.from_me,
        msg.is_group,
        msg.is_self_chat,
        msg.push_name,
        msg.chat_title,
        msg.timestamp_ms,
        msg.text,
        msg.quoted_text,
        msg.media_placeholder,
    )
}

#[allow(clippy::too_many_arguments)]
fn normalize_baileys_message(
    id: String,
    remote_jid: String,
    remote_jid_alt: Option<String>,
    participant: Option<String>,
    participant_alt: Option<String>,
    resolved_chat_e164: Option<String>,
    resolved_sender_e164: Option<String>,
    from_me: bool,
    is_group: bool,
    is_self_chat: bool,
    push_name: Option<String>,
    chat_title: Option<String>,
    timestamp_ms: i64,
    text: String,
    quoted_text: Option<String>,
    media_placeholder: Option<String>,
) -> InboundRawMessage {
    let norm_remote = normalize_jid(&remote_jid);
    let norm_remote_alt = remote_jid_alt.as_deref().map(normalize_jid);
    let norm_part = participant.as_deref().map(normalize_jid);
    let norm_part_alt = participant_alt.as_deref().map(normalize_jid);

    let chat_e164 = resolved_chat_e164
        .as_deref()
        .and_then(normalize_e164)
        .or_else(|| norm_remote_alt.as_deref().and_then(normalize_e164))
        .or_else(|| normalize_e164(&norm_remote));

    let chat_id = if is_group {
        normalize_identifier(&norm_remote)
    } else if let Some(e164) = &chat_e164 {
        e164.clone()
    } else {
        normalize_identifier(&norm_remote)
    };

    let mut chat_aliases = Vec::new();
    chat_aliases.push(normalize_identifier(&norm_remote));
    if let Some(alt) = &norm_remote_alt {
        chat_aliases.push(normalize_identifier(alt));
    }
    if let Some(e164) = &chat_e164 {
        chat_aliases.push(e164.clone());
    }

    let sender_e164 = resolved_sender_e164
        .as_deref()
        .and_then(normalize_e164)
        .or_else(|| {
            if is_group {
                norm_part_alt
                    .as_deref()
                    .and_then(normalize_e164)
                    .or_else(|| norm_part.as_deref().and_then(normalize_e164))
            } else {
                chat_e164.clone()
            }
        });

    let sender_id = if let Some(e164) = &sender_e164 {
        e164.clone()
    } else if is_group {
        norm_part
            .as_deref()
            .map(normalize_identifier)
            .unwrap_or_else(|| chat_id.clone())
    } else {
        chat_id.clone()
    };

    let mut sender_aliases = Vec::new();
    sender_aliases.push(sender_id.clone());
    if is_group {
        if let Some(p) = &norm_part {
            sender_aliases.push(normalize_identifier(p));
        }
        if let Some(pa) = &norm_part_alt {
            sender_aliases.push(normalize_identifier(pa));
        }
    } else {
        sender_aliases.extend(chat_aliases.iter().cloned());
    }

    let raw_jid = if !norm_remote.is_empty() {
        Some(norm_remote)
    } else {
        None
    };

    let clean_chat_title = chat_title
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            if !is_group && !from_me && !is_self_chat {
                push_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string)
            } else {
                None
            }
        });

    InboundRawMessage {
        id,
        platform: Platform::Whatsapp,
        chat_id,
        chat_aliases,
        raw_jid,
        chat_title: clean_chat_title,
        sender_id,
        sender_aliases,
        sender_name: push_name.unwrap_or_default(),
        from_me,
        is_group,
        is_self_chat,
        timestamp_ms,
        text,
        quoted_text,
        media_placeholder,
    }
}

/// Interactive one-time QR pairing for WhatsApp (`hey-computer-bridge pair whatsapp`).
pub async fn pair_whatsapp(config: &BridgeConfig) -> Result<()> {
    let session_dir = whatsapp_session_dir(config)?;
    let _pair_lock = PairingLockGuard::acquire(&session_dir);
    let (program, base_args) = reader_program(config);
    let mut cmd = Command::new(&program);
    cmd.args(&base_args)
        .arg("--pair-only")
        .arg("--session")
        .arg(&session_dir);
    if !config.whatsapp.sync_full_history {
        cmd.arg("--no-full-history");
    }
    let status = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("spawning {program} --pair-only"))?;
    if !status.success() {
        bail!("WhatsApp pairing failed with status {status}");
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
