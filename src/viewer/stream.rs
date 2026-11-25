//! Background streaming client over `$XDG_RUNTIME_DIR/hey-computer.sock`.

use std::path::PathBuf;
use std::time::Instant;

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
