//! The long-lived half: the socket, and the conductor behind it.

use std::future::Future;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;

use crate::attachments;
use crate::conductor::{self, Handle, Sink};
use crate::config::Config;
use crate::context::{ContextProvider, SystemContext};
use crate::keyring::{DbusKeyringWatcher, KeyringWatcher};
use crate::lines::{self, Line};
use crate::messenger::{self, MessengerHandle};
use crate::notify_filter::{
    self, EnrichmentResult, FilterDecision, IncomingNotification, NotificationFilter,
};
use crate::present::{self, Desktop, Presentation, Presenter};
use crate::protocol::{
    self, DraftAction, FailReason, InputSource, MAX_REQUEST_LINE, NotifyReleaseAction, Reply,
    Request,
};
use crate::scheduler::{self, SchedulerHandle};
use crate::store::{self, CheckpointClaim, DraftStatus, NewDraftCheckpoint, Store};
use crate::{environment, ipc};

/// A client gets this long to send its request line.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// At shutdown, queued notifications get this long to be shown.
const PRESENT_GRACE: Duration = Duration::from_secs(5);
/// At shutdown, waiting clients get this long to be sent their outcome.
const REPLY_GRACE: Duration = Duration::from_secs(1);

/// Run the daemon with the desktop presenter until SIGTERM or SIGINT.
pub async fn run(config: Config, socket: &Path) -> Result<()> {
    let answers = ipc::runtime_dir()?.join("hey-computer");
    let desktop = Desktop::spawn(&config.present, answers);
    let presenter: Arc<dyn Presenter> = desktop.clone();
    let result = serve(config, socket, presenter, termination()).await;
    // The last popup replaces "Thinking…"; without it that one would stay up
    // until it expires.
    if tokio::time::timeout(PRESENT_GRACE, desktop.finish())
        .await
        .is_err()
    {
        tracing::warn!("gave up on the last notifications after {PRESENT_GRACE:?}");
    }
    result
}

/// Serve on `socket` until `shutdown` completes.
pub async fn serve(
    config: Config,
    socket: &Path,
    presenter: Arc<dyn Presenter>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    serve_with_context(config, socket, presenter, Arc::new(SystemContext), shutdown).await
}

/// Serve on `socket` with a custom ambient context provider (for tests).
pub async fn serve_with_context(
    config: Config,
    socket: &Path,
    presenter: Arc<dyn Presenter>,
    context: Arc<dyn ContextProvider>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    serve_with_deps(
        config,
        socket,
        presenter,
        context,
        Arc::new(DbusKeyringWatcher),
        shutdown,
    )
    .await
}

#[derive(Clone)]
struct DaemonServices {
    config: Arc<Config>,
    handle: Handle,
    scheduler: SchedulerHandle,
    store: Store,
    notify_filter: Arc<NotificationFilter>,
    messenger: MessengerHandle,
    presenter: Arc<dyn Presenter>,
}

/// Serve on `socket` with injectable ambient context provider and keyring
/// watcher (for integration tests).
pub async fn serve_with_deps(
    config: Config,
    socket: &Path,
    presenter: Arc<dyn Presenter>,
    context: Arc<dyn ContextProvider>,
    keyring: Arc<dyn KeyringWatcher>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    // `Config::load` validates too, but a config built in code (the tests,
    // another front end) comes here directly.
    config.validate()?;
    // The socket first: a second daemon started by mistake must be refused
    // before it touches the running one's agent directory.
    let listener = bind(socket).await?;
    // Prepared once here, so that a skill directory that does not exist
    // stops the daemon at start instead of failing every agent start after
    // it. The conductor prepares it again before each start.
    let paths = match environment::prepare(&config) {
        Ok(paths) => paths,
        Err(e) => {
            let _ = std::fs::remove_file(socket);
            return Err(e.context("preparing the agent's directory"));
        }
    };
    let (store, initial_last_id) = match Store::open(&paths.state_dir, &config.history) {
        Ok(opened) => opened,
        Err(e) => {
            let _ = std::fs::remove_file(socket);
            return Err(e.context("opening the history database"));
        }
    };
    if let Err(e) = crate::store::sync_managed_schedules(
        &paths.state_dir,
        &config.scheduler.jobs,
        crate::store::unix_timestamp_ms(),
    ) {
        let _ = std::fs::remove_file(socket);
        return Err(e.context("reconciling declarative [scheduler.jobs]"));
    }
    tracing::info!("agent directory {}", paths.agent_dir.display());
    tracing::info!("listening on {}", socket.display());

    let config = Arc::new(config);
    let (handle, mut conductor) = conductor::spawn_with_deps(
        Arc::clone(&config),
        Arc::clone(&presenter),
        context,
        Some((store.clone(), initial_last_id)),
        keyring,
    );
    let (sched_handle, sched_task) = scheduler::spawn(
        Arc::clone(&config),
        paths.state_dir.clone(),
        handle.clone(),
        Arc::clone(&presenter),
    );
    let (messenger_handle, messenger_task) = messenger::spawn(
        Arc::clone(&config),
        handle.clone(),
        store.clone(),
        Arc::clone(&presenter),
    );
    let services = DaemonServices {
        config: Arc::clone(&config),
        handle: handle.clone(),
        scheduler: sched_handle.clone(),
        store: store.clone(),
        notify_filter: Arc::new(NotificationFilter::new(config.notifications.clone())),
        messenger: messenger_handle.clone(),
        presenter,
    };
    tokio::pin!(shutdown);
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let svc = services.clone();
                    connections.spawn(async move {
                        if let Err(e) = connection(&svc, stream).await {
                            tracing::debug!("client connection: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            () = &mut shutdown => break,
            // Finished connections are collected as they end.
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            // The conductor only ends when told to. If it ended on its own
            // (a panic), the daemon would accept requests and answer none;
            // exiting with an error lets systemd restart it instead.
            ended = &mut conductor => {
                messenger_handle.shutdown();
                let _ = messenger_task.await;
                sched_handle.shutdown();
                let _ = sched_task.await;
                let _ = std::fs::remove_file(socket);
                return Err(match ended {
                    Err(e) if e.is_panic() => anyhow::anyhow!("the conductor panicked"),
                    _ => anyhow::anyhow!("the conductor stopped unexpectedly"),
                });
            }
        }
    }

    tracing::info!("shutting down");
    // Closed at once, so a client that comes now is told the daemon is not
    // running instead of waiting on a socket nobody accepts from.
    drop(listener);
    if let Err(e) = std::fs::remove_file(socket) {
        tracing::warn!("cannot remove {}: {e}", socket.display());
    }
    messenger_handle.shutdown();
    let _ = messenger_task.await;
    sched_handle.shutdown();
    let _ = sched_task.await;
    // Cancels the running request too: with an agent that keeps working,
    // waiting for the turn would hold up a restart until systemd kills us.
    handle.shutdown();
    let _ = conductor.await;
    store.shutdown().await;
    // Every request has its outcome now; let the clients waiting for one
    // get it before the process ends. A client that never sent a line is
    // not waited for.
    let _ = tokio::time::timeout(REPLY_GRACE, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    Ok(())
}

async fn termination() {
    let mut sigterm = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        Ok(signal) => signal,
        Err(e) => {
            tracing::error!("cannot watch for SIGTERM: {e}");
            std::future::pending::<()>().await;
            return;
        }
    };
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("got SIGTERM"),
        result = tokio::signal::ctrl_c() => match result {
            Ok(()) => tracing::info!("got SIGINT"),
            Err(e) => {
                tracing::error!("cannot watch for SIGINT: {e}");
                std::future::pending::<()>().await;
            }
        },
    }
}

/// Take over the socket, refusing to displace a daemon that is running.
async fn bind(path: &Path) -> Result<UnixListener> {
    if UnixStream::connect(path).await.is_ok() {
        bail!(
            "another hey-computer daemon is already listening on {}",
            path.display()
        );
    }
    // Nothing answered, so a socket here is stale. Anything else is not
    // ours to delete: a mistyped --socket must not cost the user a file.
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            std::fs::remove_file(path)
                .with_context(|| format!("removing the stale socket {}", path.display()))?;
            tracing::debug!("removed the stale socket at {}", path.display());
        }
        Ok(_) => bail!(
            "{} exists and is not a socket; refusing to replace it",
            path.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("checking {}", path.display())),
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {} to 0600", path.display()))?;
    Ok(listener)
}

/// One request in, one or more replies out.
async fn connection(svc: &DaemonServices, stream: UnixStream) -> Result<()> {
    let handle = &svc.handle;
    let scheduler = &svc.scheduler;
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);

    let line = tokio::time::timeout(REQUEST_TIMEOUT, lines::read(&mut reader, MAX_REQUEST_LINE))
        .await
        .context("the client sent no request")??;
    let request = match line {
        Line::Eof => return Ok(()),
        Line::Text(line) => {
            serde_json::from_str::<Request>(&line).map_err(|e| format!("not a request: {e}"))
        }
        Line::TooLong(n) => Err(format!(
            "the request is {n} bytes, more than the {MAX_REQUEST_LINE} allowed"
        )),
        Line::NotUtf8 => Err("the request is not UTF-8".to_string()),
    };
    let request = match request {
        Ok(request) => request,
        Err(message) => {
            tracing::warn!("refused a malformed request");
            return send(&mut write, &Reply::Error { message }).await;
        }
    };

    match request {
        Request::Status => {
            let mut status = handle.status();
            status.running_schedules = scheduler.running_ids();
            send(&mut write, &Reply::Status(status)).await
        }
        Request::Speak { action } => {
            let (enabled, speaking) = handle.control_speak(action);
            send(&mut write, &Reply::SpeakStatus { enabled, speaking }).await
        }
        Request::ReadAloud { text } => {
            if text.trim().is_empty() {
                return send(
                    &mut write,
                    &Reply::Error {
                        message: "the text to read aloud is empty".to_string(),
                    },
                )
                .await;
            }
            let (id, finished_rx) = match handle.read_aloud(text) {
                Ok(pair) => pair,
                Err(message) => return send(&mut write, &Reply::Error { message }).await,
            };
            send(&mut write, &Reply::SpeakStarted { id }).await?;
            tokio::select! {
                _ = finished_rx => {
                    send(&mut write, &Reply::SpeakFinished { id }).await
                }
                () = hung_up(&mut reader) => {
                    tracing::debug!("read_aloud client for speech {id} left");
                    handle.stop_speech(Some(id));
                    Ok(())
                }
            }
        }
        Request::ScheduleReload => {
            scheduler.reload();
            send(
                &mut write,
                &Reply::Ok {
                    message: "scheduler reloaded".to_string(),
                },
            )
            .await
        }
        Request::ScheduleRun { target } => match scheduler.run_now(target).await {
            Ok(message) => send(&mut write, &Reply::Ok { message }).await,
            Err(message) => send(&mut write, &Reply::Error { message }).await,
        },
        Request::Cancel { id } => {
            let cancelled_id = handle.cancel(id, FailReason::Cancelled);
            send(&mut write, &Reply::Cancelled { id: cancelled_id }).await
        }
        Request::Ask {
            text, attachments, ..
        } if text.trim().is_empty() && attachments.is_empty() => {
            send(
                &mut write,
                &Reply::Error {
                    message: "the request is empty".to_string(),
                },
            )
            .await
        }
        Request::Ask {
            text,
            wait,
            new,
            profile,
            background,
            suppress_token,
            attachments,
            source,
            reply_to,
            stream,
            distill,
            schedule_id,
        } => {
            if new && reply_to.is_some() {
                return send(
                    &mut write,
                    &Reply::Error {
                        message: "cannot combine `new` and `reply_to`".to_string(),
                    },
                )
                .await;
            }
            if let Some(tok) = &suppress_token
                && tok.trim().is_empty()
            {
                return send(
                    &mut write,
                    &Reply::Error {
                        message: "suppress_token must not be empty".to_string(),
                    },
                )
                .await;
            }
            if let Err(message) = handle.validate_profile(profile.as_deref()) {
                return send(&mut write, &Reply::Error { message }).await;
            }
            let loaded_attachments = if attachments.is_empty() {
                Vec::new()
            } else {
                match tokio::task::spawn_blocking(move || attachments::load_all(&attachments)).await
                {
                    Ok(Ok(loaded)) => loaded,
                    Ok(Err(e)) => {
                        return send(
                            &mut write,
                            &Reply::Error {
                                message: format!("{e:#}"),
                            },
                        )
                        .await;
                    }
                    Err(e) => {
                        return send(
                            &mut write,
                            &Reply::Error {
                                message: format!("failed to load attachments: {e}"),
                            },
                        )
                        .await;
                    }
                }
            };
            let params = conductor::AskParams {
                text,
                new,
                profile,
                background,
                restricted: false,
                suppress_token,
                attachments: loaded_attachments,
                source,
                reply_to,
                distill,
                schedule_id,
                schedule_has_script: false,
                initial_icons: Vec::new(),
            };
            if stream {
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let id = match handle.ask(params, Sink::Stream(tx)) {
                    Ok(id) => id,
                    Err(message) => return send(&mut write, &Reply::Error { message }).await,
                };
                send(&mut write, &Reply::Accepted { id }).await?;
                loop {
                    let next = tokio::select! {
                        item = rx.recv() => item,
                        () = hung_up(&mut reader) => {
                            tracing::debug!("the streaming client of request {id} left");
                            handle.cancel(Some(id), FailReason::Cancelled);
                            return Ok(());
                        }
                    };
                    match next {
                        Some(reply) => {
                            let terminal =
                                matches!(reply, Reply::Answer { .. } | Reply::Failed { .. });
                            send(&mut write, &reply).await?;
                            if terminal {
                                return Ok(());
                            }
                        }
                        None => {
                            return send(
                                &mut write,
                                &Reply::Failed {
                                    id,
                                    reason: FailReason::AgentDown,
                                    message: "the daemon is shutting down".to_string(),
                                },
                            )
                            .await;
                        }
                    }
                }
            }
            if !wait {
                return match handle.ask(params, Sink::Present) {
                    Ok(id) => send(&mut write, &Reply::Accepted { id }).await,
                    Err(message) => send(&mut write, &Reply::Error { message }).await,
                };
            }
            let (tx, rx) = oneshot::channel();
            let id = match handle.ask(params, Sink::Client(tx)) {
                Ok(id) => id,
                Err(message) => return send(&mut write, &Reply::Error { message }).await,
            };
            send(&mut write, &Reply::Accepted { id }).await?;
            // The client waits for the answer; if it hangs up first, the
            // request is cancelled, since nobody would see the answer.
            let reply = tokio::select! {
                reply = rx => reply,
                () = hung_up(&mut reader) => {
                    tracing::debug!("the client of request {id} left");
                    handle.cancel(Some(id), FailReason::Cancelled);
                    return Ok(());
                }
            };
            match reply {
                Ok(reply) => send(&mut write, &reply).await,
                Err(_) => {
                    send(
                        &mut write,
                        &Reply::Failed {
                            id,
                            reason: FailReason::AgentDown,
                            message: "the daemon is shutting down".to_string(),
                        },
                    )
                    .await
                }
            }
        }
        Request::NotifyHold {
            noti_id,
            replaces_id: _,
            app_name,
            desktop_entry,
            summary,
            body,
            category,
            urgency,
            has_default_action: _,
            dnd,
        } => {
            handle_notify_hold(
                svc,
                &mut reader,
                &mut write,
                IncomingNotification {
                    app_name,
                    desktop_entry: (!desktop_entry.trim().is_empty()).then_some(desktop_entry),
                    summary,
                    body,
                    category: (!category.trim().is_empty()).then_some(category),
                    urgency: (!urgency.trim().is_empty()).then_some(urgency),
                    dnd,
                    source_notif_id: (noti_id > 0).then_some(noti_id),
                    ..IncomingNotification::default()
                },
            )
            .await
        }
        Request::DraftResume { token, action } => {
            handle_draft_resume(svc, &mut write, token, action).await
        }
    }
}

fn show_raw_reply(reason: impl Into<String>) -> Reply {
    Reply::NotifyRelease {
        action: NotifyReleaseAction::ShowRaw,
        turn_id: None,
        token: None,
        context_note: None,
        draft_text: None,
        readonly: None,
        reason: Some(reason.into()),
    }
}

async fn handle_notify_hold(
    svc: &DaemonServices,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    write: &mut tokio::net::unix::OwnedWriteHalf,
    incoming: IncomingNotification,
) -> Result<()> {
    // Immediate < 1 ms fail-open when keyring is locked so lock-screen toasts
    // are never held.
    if svc.handle.is_locked() {
        return send(write, &show_raw_reply("keyring_locked")).await;
    }

    // Tier 2 deterministic AI-less filter (< 50 us).
    let prepared = match svc.notify_filter.evaluate(&incoming) {
        FilterDecision::Drop(reason) => {
            return send(write, &show_raw_reply(reason.as_str())).await;
        }
        FilterDecision::Allow(prepared) => prepared,
    };

    svc.messenger
        .record_notify_hold(&prepared.channel, &prepared.chat_target, &prepared.body);
    if !prepared.chat_target.eq_ignore_ascii_case(&prepared.sender) {
        svc.messenger
            .record_notify_hold(&prepared.channel, &prepared.sender, &prepared.body);
    }

    let max_hold_ms = (svc.config.notifications.max_hold_secs * 1000.0).round() as u64;
    send(write, &Reply::NotifyAccepted { max_hold_ms }).await?;

    // Opportunistically fetch bidirectional recent chat context from the
    // read-only messenger bridge (#36) within 150 ms, failing open to empty.
    let recent_chat = if let Some(platform) = messenger::channel_platform(&prepared.channel)
        && (svc.config.messenger.enabled || !svc.config.messenger.socket.as_os_str().is_empty())
        && let Some(bridge_sock) = messenger::resolve_bridge_socket(&svc.config)
    {
        messenger::fetch_recent_context(&bridge_sock, Some(platform), &prepared.chat_target, 10)
            .await
    } else {
        Vec::new()
    };

    // Stage 1 ultra-fast binary triage (`notifications.triage_profile`): decide
    // quickly whether the notification involves dates, availability, a specific
    // location, or event proposals/cancellations; otherwise release immediately.
    if svc.config.notifications.triage_profile.is_some() {
        let triage_prompt = notify_filter::build_triage_prompt(&prepared, &recent_chat);
        let triage_cancel = Arc::new(conductor::CancelToken::default());
        let triage_rx = svc
            .handle
            .triage_notification(triage_prompt, Arc::clone(&triage_cancel));
        let triage_res = tokio::select! {
            res = triage_rx => res,
            () = hung_up(reader) => {
                tracing::debug!("notify_hold client disconnected during Stage 1 triage");
                triage_cancel.cancel(FailReason::Cancelled);
                return Ok(());
            }
        };
        match triage_res {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => {
                return send(write, &show_raw_reply("triage_skip")).await;
            }
            Ok(Err((reason, message))) => {
                if !matches!(
                    reason,
                    FailReason::Cancelled | FailReason::Superseded | FailReason::KeyringLocked
                ) {
                    svc.presenter.present(Presentation::Error {
                        summary: format!(
                            "{} triage failed ({})",
                            prepared.app_name, prepared.sender
                        ),
                        body: format!("{}: {message}", reason.headline()),
                        replaces_id: None,
                    });
                }
                return send(write, &show_raw_reply("triage_failed")).await;
            }
            Err(_) => {
                return send(write, &show_raw_reply("triage_failed")).await;
            }
        }
    }

    let prompt = notify_filter::build_enrichment_prompt_with_tools(
        &prepared,
        &recent_chat,
        &svc.config.notifications.allowed_mcp_tools,
    );
    let (tx, rx) = oneshot::channel();
    let turn_id = match svc.handle.ask(
        conductor::AskParams {
            text: prompt,
            new: true,
            profile: svc.config.notifications.profile.clone(),
            background: true,
            restricted: true,
            suppress_token: Some("HEARTBEAT_OK".to_string()),
            attachments: Vec::new(),
            source: InputSource::Cli,
            reply_to: None,
            distill: false,
            schedule_id: None,
            schedule_has_script: false,
            initial_icons: Vec::new(),
        },
        Sink::Client(tx),
    ) {
        Ok(id) => id,
        Err(msg) => {
            tracing::warn!("notify_hold failed to queue restricted turn: {msg}");
            svc.presenter.present(Presentation::Error {
                summary: format!(
                    "{} enrichment failed ({})",
                    prepared.app_name, prepared.sender
                ),
                body: msg,
                replaces_id: None,
            });
            return send(write, &show_raw_reply("queue_full")).await;
        }
    };

    // Cancel the restricted drafting turn immediately if SwayNC closes the
    // socket (notification closed by app, watchdog expired, or SwayNC exited).
    let reply = tokio::select! {
        reply = rx => reply,
        () = hung_up(reader) => {
            tracing::debug!("notify_hold client disconnected; cancelling restricted turn {turn_id}");
            svc.handle.cancel(Some(turn_id), FailReason::Cancelled);
            return Ok(());
        }
    };

    let text = match reply {
        Ok(Reply::Answer {
            text,
            suppressed: false,
            ..
        }) => text,
        Ok(Reply::Answer {
            suppressed: true, ..
        }) => {
            return send(write, &show_raw_reply("suppressed")).await;
        }
        Ok(Reply::Failed {
            reason, message, ..
        }) => {
            if !matches!(
                reason,
                FailReason::Cancelled | FailReason::Superseded | FailReason::KeyringLocked
            ) {
                svc.presenter.present(Presentation::Error {
                    summary: format!(
                        "{} enrichment failed ({})",
                        prepared.app_name, prepared.sender
                    ),
                    body: format!("#{turn_id} · {}: {message}", reason.headline()),
                    replaces_id: None,
                });
            }
            return send(write, &show_raw_reply("failed")).await;
        }
        Ok(_) | Err(_) => {
            return send(write, &show_raw_reply("suppressed")).await;
        }
    };

    let EnrichmentResult::Enriched {
        context_note,
        draft_text,
    } = notify_filter::parse_enrichment_output(&text)
    else {
        return send(write, &show_raw_reply("suppressed")).await;
    };

    let now_ms = store::unix_timestamp_ms();
    let ttl_ms = (svc.config.notifications.draft_ttl_secs * 1000.0).round() as i64;
    let cp = match svc
        .store
        .create_draft_checkpoint(NewDraftCheckpoint {
            turn_id: Some(turn_id),
            channel: prepared.channel.clone(),
            app_name: prepared.app_name.clone(),
            sender: prepared.sender.clone(),
            chat_target: prepared.chat_target.clone(),
            source_notif_id: prepared.source_notif_id,
            window_class: prepared.window_class.clone(),
            context_note: context_note.clone().unwrap_or_default(),
            draft_text: draft_text.clone().unwrap_or_default(),
            created_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(ttl_ms),
        })
        .await
    {
        Ok(cp) => cp,
        Err(e) => {
            tracing::warn!("cannot persist draft checkpoint for turn {turn_id}: {e:#}");
            svc.presenter.present(Presentation::Error {
                summary: format!(
                    "{} draft save failed ({})",
                    prepared.app_name, prepared.sender
                ),
                body: format!("{e:#}"),
                replaces_id: None,
            });
            return send(write, &show_raw_reply("store_error")).await;
        }
    };

    send(
        write,
        &Reply::NotifyRelease {
            action: NotifyReleaseAction::ShowEnriched,
            turn_id: Some(turn_id),
            token: Some(cp.token),
            context_note,
            draft_text,
            readonly: Some(prepared.readonly),
            reason: None,
        },
    )
    .await
}

async fn handle_draft_resume(
    svc: &DaemonServices,
    write: &mut tokio::net::unix::OwnedWriteHalf,
    token: String,
    action: DraftAction,
) -> Result<()> {
    let peeked = match svc.store.get_draft_checkpoint(token.clone()).await {
        Ok(cp) => cp,
        Err(e) => {
            return send(
                write,
                &Reply::Error {
                    message: format!("reading draft checkpoint: {e:#}"),
                },
            )
            .await;
        }
    };

    // Enforce strict WhatsApp/Signal read-only guarantee: Paste always degrades
    // to clipboard-only Copy on read-only channels.
    let effective_action = match (&peeked, action) {
        (Some(cp), DraftAction::Paste) if notify_filter::is_readonly_channel(&cp.channel) => {
            DraftAction::Copy
        }
        _ => action,
    };

    let now_ms = store::unix_timestamp_ms();
    let claim = match svc
        .store
        .claim_draft_checkpoint(token, effective_action, now_ms)
        .await
    {
        Ok(claim) => claim,
        Err(e) => {
            return send(
                write,
                &Reply::Error {
                    message: format!("claiming draft checkpoint: {e:#}"),
                },
            )
            .await;
        }
    };

    match claim {
        CheckpointClaim::NotFound => {
            send(
                write,
                &Reply::Error {
                    message: "draft checkpoint not found".to_string(),
                },
            )
            .await
        }
        CheckpointClaim::Expired(cp) => {
            send(
                write,
                &Reply::DraftResumed {
                    id: cp.id,
                    turn_id: cp.turn_id,
                    action: effective_action,
                    status: "expired".to_string(),
                    message: "draft checkpoint expired".to_string(),
                },
            )
            .await
        }
        CheckpointClaim::AlreadyUsed(cp) => {
            // Allow opening the viewer for `Edit` even after an earlier `Copy`
            // claim (e.g. clicking Copy then Edit on the same SwayNC card),
            // while still reporting `already_used` for single-use token auditing.
            if effective_action == DraftAction::Edit && cp.status != DraftStatus::Dismissed {
                let present_cfg = svc.config.present.clone();
                let turn_id = cp.turn_id;
                let draft_text = cp.draft_text.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        present::open_draft_editor(&present_cfg, turn_id, &draft_text).await
                    {
                        tracing::debug!("draft edit viewer launch skipped/failed: {e:#}");
                    }
                });
            }
            send(
                write,
                &Reply::DraftResumed {
                    id: cp.id,
                    turn_id: cp.turn_id,
                    action: effective_action,
                    status: "already_used".to_string(),
                    message: format!("draft checkpoint already {}", cp.status),
                },
            )
            .await
        }
        CheckpointClaim::Claimed(cp) => {
            let message = match effective_action {
                DraftAction::Copy | DraftAction::Paste => {
                    if !cp.draft_text.is_empty() && !svc.config.present.clipboard.is_empty() {
                        let present_cfg = svc.config.present.clone();
                        let text = cp.draft_text.clone();
                        tokio::spawn(async move {
                            if let Err(e) = present::copy_to_clipboard(&present_cfg, &text).await {
                                tracing::debug!("draft copy_to_clipboard failed: {e:#}");
                            }
                        });
                    }
                    "copied draft to clipboard".to_string()
                }
                DraftAction::Edit => {
                    let present_cfg = svc.config.present.clone();
                    let turn_id = cp.turn_id;
                    let text = cp.draft_text.clone();
                    tokio::spawn(async move {
                        if !text.is_empty() && !present_cfg.clipboard.is_empty() {
                            let _ = present::copy_to_clipboard(&present_cfg, &text).await;
                        }
                        if let Err(e) =
                            present::open_draft_editor(&present_cfg, turn_id, &text).await
                        {
                            tracing::debug!("draft edit viewer launch skipped/failed: {e:#}");
                        }
                    });
                    "opened draft in viewer".to_string()
                }
                DraftAction::Dismiss => "dismissed draft".to_string(),
            };
            send(
                write,
                &Reply::DraftResumed {
                    id: cp.id,
                    turn_id: cp.turn_id,
                    action: effective_action,
                    status: "claimed".to_string(),
                    message,
                },
            )
            .await
        }
    }
}

/// Completes when the client closes its side (or sends more, which it
/// should not).
async fn hung_up(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) {
    let mut scratch = [0u8; 256];
    loop {
        match reader.read(&mut scratch).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

async fn send(write: &mut tokio::net::unix::OwnedWriteHalf, reply: &Reply) -> Result<()> {
    let mut line = protocol::encode(reply);
    line.push('\n');
    write.write_all(line.as_bytes()).await.context("replying")?;
    write.flush().await.context("replying")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::present::{Presentation, Recorder};
    use crate::protocol::{InputSource, SpeakAction};
    use crate::store;
    use tokio::sync::oneshot;

    /// Spawn an in-process daemon backed by a tiny Python ACP script to test
    /// streaming IPC, `mcp-ui` widget capture, voice vs. text modality, and
    /// `reply_to` Path A (live session) + Path B (DB-seeded session) resumption.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_widget_capture_voice_modality_and_reply_to_resumption() {
        let dir = std::env::temp_dir().join(format!("hc-daemon-p5-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("d.sock");
        let agent_script = dir.join("fake-acp.sh");
        let agent_log = dir.join("agent.log");

        std::fs::write(
            &agent_script,
            r#"#!/bin/sh
log_path="$1"
state_dir="$2"
seq=0

while IFS= read -r line; do
  [ -z "$line" ] && continue
  mid=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentInfo":{"name":"sh-mock","version":"0.1"}}}\n' "$mid"
      ;;
    *'"method":"session/new"'*)
      seq=$((seq + 1))
      sid="s${seq}"
      echo 0 > "${state_dir}/turns_${sid}"
      printf 'new %s\n' "$sid" >> "$log_path"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"%s","configOptions":[{"id":"model","currentValue":"gemini-4-hydrogen"},{"id":"thought_level","currentValue":"low"},{"id":"mode","currentValue":"default"}]}}\n' "$mid" "$sid"
      ;;
    *'"method":"session/set_config_option"'*|*'"method":"session/close"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$mid"
      ;;
    *'"method":"session/prompt"'*)
      sid=$(printf '%s\n' "$line" | sed -n 's/.*"sessionId":"\([^"]*\)".*/\1/p')
      prev=$(cat "${state_dir}/turns_${sid}" 2>/dev/null || echo 0)
      turn_n=$((prev + 1))
      echo "$turn_n" > "${state_dir}/turns_${sid}"
      printf 'prompt %s t%s :: %s\n' "$sid" "$turn_n" "$line" >> "$log_path"
      case "$line" in
        *'"text":"TRIGGER_WIDGET_CARD"'*)
          tc_id="tc_${seq}_${turn_n}"
          tc=$(printf '{"sessionUpdate":"tool_call","toolCallId":"%s","name":"call_mcp_tool","kind":"execute","status":"pending","rawInput":{"ServerName":"mcp-ui","ToolName":"show_widget","Arguments":{"id":"w-board","kind":"transit_board","title":"U3 Borgweg","data":{"departures":[{"line":"U3","inMin":4}]}}}}' "$tc_id")
          printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":%s}}\n' "$sid" "$tc"
          printf '{"jsonrpc":"2.0","id":"perm1","method":"session/request_permission","params":{"sessionId":"%s","toolCall":%s,"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"}]}}\n' "$sid" "$tc"
          IFS= read -r _perm_reply
          printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"tool_call_update","toolCallId":"%s","status":"completed"}}}\n' "$sid" "$tc_id"
          printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$mid"
          ;;
        *)
          printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"ack %s/t%s"}}}}\n' "$sid" "$sid" "$turn_n"
          printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$mid"
          ;;
      esac
      ;;
  esac
done
"#,
        )
        .unwrap();
        std::fs::set_permissions(&agent_script, std::fs::Permissions::from_mode(0o700)).unwrap();

        let mut cfg = Config {
            state_dir: dir.join("state"),
            ..Config::default()
        };
        cfg.agent.command = vec![
            "/bin/sh".to_string(),
            agent_script.to_string_lossy().into_owned(),
            agent_log.to_string_lossy().into_owned(),
            dir.to_string_lossy().into_owned(),
        ];
        cfg.conversation.follow_up_secs = 0.05;
        cfg.present.speak.enabled = true;
        cfg.present.speak.voice_only = true;

        let recorder = Recorder::new();
        let presenter: Arc<dyn Presenter> = recorder.clone();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let sock_clone = socket.clone();
        let state_dir = cfg.state_dir.clone();
        let daemon_task = tokio::spawn(async move {
            serve(cfg, &sock_clone, presenter, async {
                let _ = stop_rx.await;
            })
            .await
        });

        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // 1. Streaming widget-only turn emits Progress, Widget, and Answer (never FailReason::Empty).
        let (t1_id, mut stream_client) = ipc::ask_request(
            &socket,
            &Request::Ask {
                text: "TRIGGER_WIDGET_CARD".to_string(),
                wait: true,
                new: true,
                profile: None,
                background: false,
                suppress_token: None,
                attachments: vec![],
                source: InputSource::Typed,
                reply_to: None,
                stream: true,
                distill: false,
                schedule_id: None,
            },
        )
        .await
        .unwrap();
        let mut seen_widget = false;
        let mut seen_progress = false;
        loop {
            match stream_client
                .next(Some(Duration::from_secs(10)))
                .await
                .unwrap()
            {
                Reply::Progress { status, .. } => {
                    if status.contains("Render widget: U3 Borgweg") {
                        seen_progress = true;
                    }
                }
                Reply::Widget { widget, .. } => {
                    assert_eq!(widget.id, "w-board");
                    assert_eq!(widget.kind, "transit_board");
                    seen_widget = true;
                }
                Reply::Answer {
                    id,
                    text,
                    widgets,
                    conversation_id,
                    ..
                } => {
                    assert_eq!(id, t1_id);
                    assert!(text.is_empty());
                    assert_eq!(widgets.len(), 1);
                    assert_eq!(conversation_id, Some(t1_id));
                    break;
                }
                other => panic!("unexpected stream reply: {other:?}"),
            }
        }
        assert!(seen_progress, "expected Render widget progress");
        assert!(seen_widget, "expected streamed Reply::Widget");

        // 2. Wait past follow_up_secs (50 ms) and test Path A (`reply_to = Some(t1_id)`):
        //    reuses live session `s1` (`ack s1/t2`) even after follow_up_secs expired!
        tokio::time::sleep(Duration::from_millis(120)).await;
        let (t2_id, mut c2) = ipc::ask_request(
            &socket,
            &Request::Ask {
                text: "follow up on board".to_string(),
                wait: true,
                new: false,
                profile: None,
                background: false,
                suppress_token: None,
                attachments: vec![],
                source: InputSource::Typed,
                reply_to: Some(t1_id),
                stream: false,
                distill: false,
                schedule_id: None,
            },
        )
        .await
        .unwrap();
        match c2.next(Some(Duration::from_secs(10))).await.unwrap() {
            Reply::Answer {
                text,
                conversation_id,
                ..
            } => {
                assert_eq!(text, "ack s1/t2");
                assert_eq!(conversation_id, Some(t1_id));
            }
            other => panic!("unexpected reply: {other:?}"),
        }

        // 3. Branching `reply_to = Some(t1_id)` now that `last_turn_id == Some(t2_id)`
        //    takes Path B: opens a fresh session seeded with `<untrusted_conversation_history>`.
        let (_t3_id, mut c3) = ipc::ask_request(
            &socket,
            &Request::Ask {
                text: "branch from turn 1".to_string(),
                wait: true,
                new: false,
                profile: None,
                background: false,
                suppress_token: None,
                attachments: vec![],
                source: InputSource::Typed,
                reply_to: Some(t1_id),
                stream: false,
                distill: false,
                schedule_id: None,
            },
        )
        .await
        .unwrap();
        match c3.next(Some(Duration::from_secs(10))).await.unwrap() {
            Reply::Answer {
                text,
                conversation_id,
                ..
            } => {
                assert!(
                    text.ends_with("/t1"),
                    "expected fresh seeded session, got {text}"
                );
                assert_eq!(conversation_id, Some(t1_id));
            }
            other => panic!("unexpected reply: {other:?}"),
        }
        assert!(t2_id > t1_id);

        // 4. Voice vs. Text Modality on Sink::Present (`wait = false`):
        //    - Typed request NEVER emits Speech* when `voice_only = true`.
        //    - Voice request prepends `<modality source="voice">` and DOES emit SpeechStart/SpeechChunk/SpeechFinish!
        let (typed_id, _) = ipc::ask_request(
            &socket,
            &Request::Ask {
                text: "typed hello".to_string(),
                wait: false,
                new: true,
                profile: None,
                background: false,
                suppress_token: None,
                attachments: vec![],
                source: InputSource::Typed,
                reply_to: None,
                stream: false,
                distill: false,
                schedule_id: None,
            },
        )
        .await
        .unwrap();
        recorder
            .wait_for(Duration::from_secs(5), |items| {
                items
                    .iter()
                    .any(|i| matches!(i, Presentation::Answer { id, .. } if *id == typed_id))
            })
            .await
            .unwrap();
        assert!(
            !recorder.items().iter().any(|i| matches!(
                i,
                Presentation::SpeechStart { .. }
                    | Presentation::SpeechChunk { .. }
                    | Presentation::SpeechFinish { .. }
            )),
            "typed request must not emit Speech* when voice_only is true: {:?}",
            recorder.items()
        );

        let (voice_id, _) = ipc::ask_request(
            &socket,
            &Request::Ask {
                text: "spoken hello".to_string(),
                wait: false,
                new: true,
                profile: None,
                background: false,
                suppress_token: None,
                attachments: vec![],
                source: InputSource::Voice,
                reply_to: None,
                stream: false,
                distill: false,
                schedule_id: None,
            },
        )
        .await
        .unwrap();
        let items = recorder
            .wait_for(Duration::from_secs(5), |items| {
                items
                    .iter()
                    .any(|i| matches!(i, Presentation::Answer { id, .. } if *id == voice_id))
            })
            .await
            .unwrap();
        assert!(
            items
                .iter()
                .any(|i| matches!(i, Presentation::SpeechStart { id } if *id == voice_id))
        );
        assert!(
            items
                .iter()
                .any(|i| matches!(i, Presentation::SpeechChunk { id, .. } if *id == voice_id))
        );
        assert!(
            items
                .iter()
                .any(|i| matches!(i, Presentation::SpeechFinish { id, .. } if *id == voice_id))
        );

        // Speak IPC request works over the socket.
        match ipc::request(
            &socket,
            &Request::Speak {
                action: SpeakAction::Toggle,
            },
        )
        .await
        .unwrap()
        {
            Reply::SpeakStatus { enabled, .. } => assert!(!enabled),
            other => panic!("expected SpeakStatus, got {other:?}"),
        }
        assert!(!store::load_speak_state(&state_dir, true));

        let logs = std::fs::read_to_string(&agent_log).unwrap();
        assert!(
            logs.contains("<untrusted_conversation_history>"),
            "Path B must seed conversation history: {logs}"
        );
        assert!(
            logs.contains(r#"<modality source=\"voice\">"#),
            "Voice ask must carry voice modality header: {logs}"
        );

        let _ = stop_tx.send(());
        daemon_task.await.unwrap().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
