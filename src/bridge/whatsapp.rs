//! WhatsApp adapter: supervises the strictly read-only `baileys-reader` helper.
//!
//! The Node helper is spawned with `stdin = /dev/null` and emits one-way JSONL
//! events on stdout. On `401` (`auth_required`), the supervisor transitions
//! WhatsApp to `PlatformState::AuthRequired` and stops respawning so the user
//! gets a single clear unlink alert instead of a tight crash loop.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::sync::{mpsc, watch};

use crate::bridge::config::{
    BridgeConfig, ensure_private_dir, normalize_e164, normalize_identifier, normalize_jid,
};
use crate::bridge::context::{InboundRawMessage, RawAdapterEvent};
use crate::bridge::protocol::{MAX_BRIDGE_LINE, Platform, PlatformState};
use crate::lines::{self, Line};

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
/// stops retries.
pub async fn run(
    config: BridgeConfig,
    tx: mpsc::Sender<RawAdapterEvent>,
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
    let mut backoff_secs = 2u64;

    loop {
        if *stop.borrow() {
            break;
        }

        let _ = tx
            .send(RawAdapterEvent::PlatformStateChanged {
                platform: Platform::Whatsapp,
                state: PlatformState::Starting,
                detail: None,
            })
            .await;

        let mut cmd = Command::new(&program);
        cmd.args(&base_args)
            .arg("--session")
            .arg(&session_dir)
            // Layer 1 read-only enforcement: stdin is /dev/null.
            .stdin(Stdio::null())
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
                        terminate_process_group(child_pid);
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        stderr_task.abort();
                        return;
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
                            Ok(BaileysOutput::Message(msg)) => {
                                let BaileysMessage {
                                    upsert_type,
                                    id,
                                    remote_jid,
                                    remote_jid_alt,
                                    participant,
                                    participant_alt,
                                    resolved_chat_e164,
                                    resolved_sender_e164,
                                    from_me,
                                    is_group,
                                    is_self_chat,
                                    push_name,
                                    timestamp_ms,
                                    text,
                                    quoted_text,
                                    media_placeholder,
                                } = *msg;
                                tracing::debug!(
                                    platform = "whatsapp",
                                    %upsert_type,
                                    from_me,
                                    is_group,
                                    is_self_chat,
                                    "received whatsapp upsert"
                                );
                                let inbound = normalize_baileys_message(
                                    id,
                                    remote_jid,
                                    remote_jid_alt,
                                    participant,
                                    participant_alt,
                                    resolved_chat_e164,
                                    resolved_sender_e164,
                                    from_me,
                                    is_group,
                                    is_self_chat,
                                    push_name,
                                    timestamp_ms,
                                    text,
                                    quoted_text,
                                    media_placeholder,
                                );
                                let _ = tx.send(RawAdapterEvent::Message(inbound)).await;
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

        let status = child.wait().await;
        stderr_task.abort();

        if *stop.borrow() {
            break;
        }
        if auth_required {
            tracing::warn!("whatsapp adapter stopped due to auth_required (401)");
            // Wait until shutdown so we do not tight-loop reconnect while unlinked.
            while !*stop.borrow() {
                if stop.changed().await.is_err() {
                    break;
                }
            }
            break;
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

    InboundRawMessage {
        id,
        platform: Platform::Whatsapp,
        chat_id,
        chat_aliases,
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
    let (program, base_args) = reader_program(config);
    let status = Command::new(&program)
        .args(&base_args)
        .arg("--pair-only")
        .arg("--session")
        .arg(&session_dir)
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
