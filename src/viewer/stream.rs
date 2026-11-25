//! Background streaming, TTS, and schedule control IPC clients over
//! `$XDG_RUNTIME_DIR/hey-computer.sock`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use hey_computer::config::Speak as SpeakConfig;
use hey_computer::ipc;
use hey_computer::protocol::{InputSource, Reply, Request, WidgetSpec};
use hey_computer::render::render_markdown;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug)]
pub enum StreamUiEvent {
    Started {
        id: u64,
    },
    Progress {
        id: u64,
        status: String,
    },
    Chunk {
        id: u64,
        delta: String,
        markdown: String,
        html: String,
    },
    Widget {
        id: u64,
        widget: WidgetSpec,
        widgets: Vec<WidgetSpec>,
    },
    Reset {
        id: u64,
    },
    Done {
        id: u64,
        conversation_id: u64,
        markdown: String,
        html: String,
        widgets: Vec<WidgetSpec>,
        denied: Vec<String>,
        profile: Option<String>,
        duration_ms: u64,
    },
    Error {
        id: Option<u64>,
        message: String,
    },
}

#[derive(Debug)]
pub enum TtsUiEvent {
    Started,
    Finished,
    Error(String),
}

/// Normalize a UI profile string into `Option<String>` for `Request::Ask`.
/// `"auto"` and empty strings mean automatic/default routing (`None`).
pub fn normalize_ask_profile(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub fn spawn_ask_stream(
    socket: PathBuf,
    prompt: String,
    profile: Option<String>,
    reply_to: Option<u64>,
    tx: UnboundedSender<StreamUiEvent>,
) {
    let _ = std::thread::Builder::new()
        .name("hc-viewer-stream".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    let _ = tx.send(StreamUiEvent::Error {
                        id: None,
                        message: format!("cannot start viewer IPC runtime: {err}"),
                    });
                    return;
                }
            };

            rt.block_on(async move {
                let request = Request::Ask {
                    text: prompt,
                    wait: true,
                    new: false,
                    profile,
                    background: false,
                    suppress_token: None,
                    attachments: Vec::new(),
                    source: InputSource::Typed,
                    reply_to,
                    stream: true,
                    distill: false,
                    schedule_id: None,
                };

                let started = Instant::now();
                let (accepted_id, mut client) = match ipc::ask_request(&socket, &request).await {
                    Ok(pair) => pair,
                    Err(err) => {
                        let _ = tx.send(StreamUiEvent::Error {
                            id: None,
                            message: format!("{err:#}"),
                        });
                        return;
                    }
                };

                if tx.send(StreamUiEvent::Started { id: accepted_id }).is_err() {
                    return;
                }

                let mut text_buf = String::new();
                let mut widgets_buf: Vec<WidgetSpec> = Vec::new();

                loop {
                    match client.next(None).await {
                        Ok(Reply::Progress { id, status }) => {
                            if tx.send(StreamUiEvent::Progress { id, status }).is_err() {
                                break;
                            }
                        }
                        Ok(Reply::Chunk { id, text }) => {
                            text_buf.push_str(&text);
                            let html = render_markdown(&text_buf);
                            if tx
                                .send(StreamUiEvent::Chunk {
                                    id,
                                    delta: text,
                                    markdown: text_buf.clone(),
                                    html,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(Reply::Widget { id, widget }) => {
                            widgets_buf.push(widget.clone());
                            if tx
                                .send(StreamUiEvent::Widget {
                                    id,
                                    widget,
                                    widgets: widgets_buf.clone(),
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(Reply::Reset { id }) => {
                            text_buf.clear();
                            widgets_buf.clear();
                            if tx.send(StreamUiEvent::Reset { id }).is_err() {
                                break;
                            }
                        }
                        Ok(Reply::Answer {
                            id,
                            text,
                            denied,
                            widgets,
                            conversation_id,
                            profile,
                            ..
                        }) => {
                            let final_md = if text.is_empty() && !text_buf.is_empty() {
                                text_buf
                            } else {
                                text
                            };
                            let final_widgets = if widgets.is_empty() && !widgets_buf.is_empty() {
                                widgets_buf
                            } else {
                                widgets
                            };
                            let final_html = render_markdown(&final_md);
                            let _ = tx.send(StreamUiEvent::Done {
                                id,
                                conversation_id: conversation_id.unwrap_or(id),
                                markdown: final_md,
                                html: final_html,
                                widgets: final_widgets,
                                denied,
                                profile,
                                duration_ms: started.elapsed().as_millis() as u64,
                            });
                            break;
                        }
                        Ok(Reply::Failed {
                            id,
                            reason,
                            message,
                        }) => {
                            let msg = if message.trim().is_empty() {
                                reason.headline().to_string()
                            } else {
                                format!("{}: {message}", reason.headline())
                            };
                            let _ = tx.send(StreamUiEvent::Error {
                                id: Some(id),
                                message: msg,
                            });
                            break;
                        }
                        Ok(Reply::Error { message }) => {
                            let _ = tx.send(StreamUiEvent::Error {
                                id: Some(accepted_id),
                                message,
                            });
                            break;
                        }
                        Ok(_) => {}
                        Err(err) => {
                            let _ = tx.send(StreamUiEvent::Error {
                                id: Some(accepted_id),
                                message: format!("{err:#}"),
                            });
                            break;
                        }
                    }
                }
            });
        });
}

/// Stream TTS playback over `hey-computer.sock` (`Request::ReadAloud`) with
/// automatic offline fallback to `present::speak_text_local`.
pub fn spawn_tts_stream(
    socket: Option<PathBuf>,
    speak_config: SpeakConfig,
    text: String,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
    tx: UnboundedSender<TtsUiEvent>,
) {
    let _ = std::thread::Builder::new()
        .name("hc-viewer-tts".to_string())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                let _ = tx.send(TtsUiEvent::Error(
                    "cannot start viewer TTS runtime".to_string(),
                ));
                return;
            };
            rt.block_on(async move {
                if *cancel_rx.borrow() {
                    let _ = tx.send(TtsUiEvent::Finished);
                    return;
                }

                let mut fallback_offline = socket.is_none();
                if let Some(ref sock_path) = socket {
                    let req = Request::ReadAloud { text: text.clone() };
                    match ipc::Client::send(sock_path, &req).await {
                        Ok(mut client) => {
                            let mut started = false;
                            loop {
                                tokio::select! {
                                    biased;
                                    res = cancel_rx.changed() => {
                                        if res.is_err() || *cancel_rx.borrow() {
                                            // Dropping `client` closes the socket so the daemon
                                            // stops `piper | pw-play` and unpauses ducked media.
                                            drop(client);
                                            let _ = tx.send(TtsUiEvent::Finished);
                                            return;
                                        }
                                    }
                                    reply = client.next(None) => {
                                        match reply {
                                            Ok(Reply::SpeakStarted { .. }) => {
                                                if !started {
                                                    started = true;
                                                    let _ = tx.send(TtsUiEvent::Started);
                                                }
                                            }
                                            Ok(Reply::SpeakFinished { .. }) => {
                                                let _ = tx.send(TtsUiEvent::Finished);
                                                return;
                                            }
                                            Ok(Reply::Error { message }) => {
                                                let _ = tx.send(TtsUiEvent::Error(message));
                                                return;
                                            }
                                            Ok(_) => {}
                                            Err(err) => {
                                                if started {
                                                    let _ = tx.send(TtsUiEvent::Finished);
                                                } else {
                                                    let _ = tx.send(TtsUiEvent::Error(format!("{err:#}")));
                                                }
                                                return;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            if err.downcast_ref::<ipc::NotRunning>().is_some() {
                                fallback_offline = true;
                            } else {
                                let _ = tx.send(TtsUiEvent::Error(format!("{err:#}")));
                                return;
                            }
                        }
                    }
                }

                if fallback_offline {
                    let _ = tx.send(TtsUiEvent::Started);
                    match hey_computer::present::speak_text_local(&speak_config, text, cancel_rx)
                        .await
                    {
                        Ok(_) => {
                            let _ = tx.send(TtsUiEvent::Finished);
                        }
                        Err(err) => {
                            let _ = tx.send(TtsUiEvent::Error(format!("{err:#}")));
                        }
                    }
                }
            });
        });
}

/// Query currently running schedule IDs from the daemon (gracefully returning
/// an empty set when the daemon is offline or slow).
pub fn query_running_schedule_ids(socket: Option<&Path>) -> HashSet<u64> {
    let Some(path) = socket else {
        return HashSet::new();
    };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return HashSet::new();
    };
    rt.block_on(async move {
        let req_fut = ipc::request(path, &Request::Status);
        let Ok(Ok(Reply::Status(status))) =
            tokio::time::timeout(Duration::from_millis(250), req_fut).await
        else {
            return HashSet::new();
        };
        status.running_schedules.into_iter().collect()
    })
}

/// Send `Request::ScheduleRun` to the daemon.
pub fn send_schedule_run(socket: Option<&Path>, target: &str) -> Result<String> {
    let Some(path) = socket else {
        bail!("No daemon socket path configured");
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let target = target.to_string();
    rt.block_on(async move {
        match ipc::request(path, &Request::ScheduleRun { target }).await? {
            Reply::Ok { message } => Ok(message),
            Reply::Error { message } => bail!("{message}"),
            other => bail!("unexpected daemon reply: {other:?}"),
        }
    })
}

/// Best-effort `Request::ScheduleReload` to wake the daemon scheduler after
/// pausing, resuming, or removing a schedule in `history.db`.
pub fn send_schedule_reload_best_effort(socket: Option<&Path>) {
    let Some(path) = socket else {
        return;
    };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    rt.block_on(async move {
        let _ = tokio::time::timeout(
            Duration::from_millis(400),
            ipc::request(path, &Request::ScheduleReload),
        )
        .await;
    });
}
