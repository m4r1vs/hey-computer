//! Background consumer for the strictly read-only `hey-computer-bridge` socket
//! (Phase 6 `#36`).
//!
//! Responsibilities:
//! 1. Subscribes to `$XDG_RUNTIME_DIR/hey-computer-bridge.sock` with `1s..30s`
//!    reconnect backoff and tracks connected platform states (`whatsapp`, `signal`).
//! 2. Exposes [`fetch_recent_context`] (150 ms bounded `GetContext` RPC, failing
//!    open to `Vec::new()`) so `NotifyHold` (`#35`) enriches WhatsApp/Signal holds
//!    with bidirectional rolling conversation context whenever the bridge runs.
//! 3. Handles `BridgeEvent::UserReplied` by calling `store.dismiss_drafts_for_chat`
//!    so stale pending drafts auto-dismiss when the user replies from their phone.
//! 4. Routes verified `MessageKind::SelfChat` messages to standard desktop turns
//!    (desktop presentation only, zero outbound messenger echo) and allowlisted
//!    `MessageKind::DirectMessage` events to `Lane::Restricted` fallback drafts
//!    when `prefer_over_notifications` is enabled and no recent `NotifyHold` already
//!    handled that message.
//! 5. Surfaces `PlatformState::AuthRequired` with a rate-limited desktop alert.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use crate::bridge::Subscriber;
use crate::bridge::config as bridge_config;
use crate::bridge::protocol::{
    BridgeEvent, BridgeRequest, ContextAuthor, ContextMessage, MAX_BRIDGE_LINE, MessageKind,
    Platform, PlatformState, encode as encode_bridge,
};
use crate::conductor::{AskParams, Handle as ConductorHandle, Sink};
use crate::config::{Config, SelfChatLane};
use crate::icons::IconResolver;
use crate::lines::{self, Line};
use crate::notify_filter::{
    self, ChatContextMessage, EnrichmentResult, LocalTime, PreparedNotification,
};
use crate::present::{Presentation, Presenter, Urgency};
use crate::protocol::{InputSource, Reply};
use crate::store::{self, NewDraftCheckpoint, Store};

/// Strict wall-clock cap for opportunistic `GetContext` enrichment calls from `NotifyHold`.
const CONTEXT_FETCH_TIMEOUT: Duration = Duration::from_millis(150);
/// Minimum reconnect backoff for the bridge subscriber loop.
const MIN_RECONNECT_SECS: u64 = 1;
/// Maximum reconnect backoff for the bridge subscriber loop.
const MAX_RECONNECT_SECS: u64 = 30;
/// Window during which a `NotifyHold` suppresses duplicate bridge DM drafting.
const HOLD_DEDUPE_WINDOW: Duration = Duration::from_secs(30);
/// Upper bound on tracked recent `NotifyHold` entries.
const MAX_RECENT_HOLDS: usize = 64;
/// Minimum interval between repeated `AuthRequired` desktop alerts per platform.
const AUTH_ALERT_COOLDOWN: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone)]
struct RecentHold {
    channel: String,
    sender_lower: String,
    body_lower: String,
    at: Instant,
}

#[derive(Debug)]
struct MessengerState {
    connected: bool,
    whatsapp: PlatformState,
    signal: PlatformState,
    recent_holds: VecDeque<RecentHold>,
    dm_cooldowns: HashMap<(Platform, String), Instant>,
    auth_alerts: HashMap<Platform, Instant>,
}

impl Default for MessengerState {
    fn default() -> Self {
        Self {
            connected: false,
            whatsapp: PlatformState::Disabled,
            signal: PlatformState::Disabled,
            recent_holds: VecDeque::new(),
            dm_cooldowns: HashMap::new(),
            auth_alerts: HashMap::new(),
        }
    }
}

/// Shared handle to the daemon's messenger bridge subscriber & hold-dedup tracker.
#[derive(Clone)]
pub struct MessengerHandle {
    state: Arc<Mutex<MessengerState>>,
    stop_tx: watch::Sender<bool>,
}

impl MessengerHandle {
    pub fn inactive() -> Self {
        let (stop_tx, _) = watch::channel(true);
        Self {
            state: Arc::new(Mutex::new(MessengerState::default())),
            stop_tx,
        }
    }

    pub fn shutdown(&self) {
        let _ = self.stop_tx.send(true);
    }

    /// Record a WhatsApp/Signal message currently handled by SwayNC `NotifyHold`
    /// so a simultaneous `BridgeEvent::Message(DirectMessage)` does not draft twice.
    pub fn record_notify_hold(&self, channel: &str, sender: &str, body: &str) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let now = Instant::now();
        while let Some(front) = state.recent_holds.front() {
            if now.saturating_duration_since(front.at) > HOLD_DEDUPE_WINDOW {
                state.recent_holds.pop_front();
            } else {
                break;
            }
        }
        while state.recent_holds.len() >= MAX_RECENT_HOLDS {
            state.recent_holds.pop_front();
        }
        state.recent_holds.push_back(RecentHold {
            channel: channel.trim().to_ascii_lowercase(),
            sender_lower: sender.trim().to_ascii_lowercase(),
            body_lower: body.trim().to_ascii_lowercase(),
            at: now,
        });
    }

    /// Snapshot of `(bridge_connected, whatsapp_state, signal_state)`.
    pub fn platform_states(&self) -> (bool, PlatformState, PlatformState) {
        self.state
            .lock()
            .map(|s| (s.connected, s.whatsapp, s.signal))
            .unwrap_or((false, PlatformState::Disabled, PlatformState::Disabled))
    }

    fn was_recently_held(
        &self,
        platform: Platform,
        sender_name: &str,
        chat_id: &str,
        text: &str,
    ) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        let now = Instant::now();
        while let Some(front) = state.recent_holds.front() {
            if now.saturating_duration_since(front.at) > HOLD_DEDUPE_WINDOW {
                state.recent_holds.pop_front();
            } else {
                break;
            }
        }
        let chan = platform.to_string();
        let sender_l = sender_name.trim().to_ascii_lowercase();
        let chat_l = chat_id.trim().to_ascii_lowercase();
        let body_l = text.trim().to_ascii_lowercase();
        state.recent_holds.iter().any(|h| {
            h.channel == chan
                && (h.sender_lower == sender_l
                    || h.sender_lower == chat_l
                    || (!body_l.is_empty()
                        && (h.body_lower == body_l || body_l.contains(&h.body_lower))))
        })
    }

    fn allow_dm_cooldown(&self, platform: Platform, chat_id: &str, cooldown: Duration) -> bool {
        if cooldown.is_zero() {
            return true;
        }
        let Ok(mut state) = self.state.lock() else {
            return true;
        };
        let now = Instant::now();
        let key = (platform, chat_id.to_string());
        if let Some(&prev) = state.dm_cooldowns.get(&key)
            && now.saturating_duration_since(prev) < cooldown
        {
            return false;
        }
        state.dm_cooldowns.insert(key, now);
        true
    }

    fn allow_auth_alert(&self, platform: Platform) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return true;
        };
        let now = Instant::now();
        if let Some(&prev) = state.auth_alerts.get(&platform)
            && now.saturating_duration_since(prev) < AUTH_ALERT_COOLDOWN
        {
            return false;
        }
        state.auth_alerts.insert(platform, now);
        true
    }
}

/// Resolve the configured bridge socket path, falling back to
/// `$HEY_COMPUTER_BRIDGE_SOCKET` or `$XDG_RUNTIME_DIR/hey-computer-bridge.sock`.
pub fn resolve_bridge_socket(config: &Config) -> Option<PathBuf> {
    if !config.messenger.socket.as_os_str().is_empty() {
        return Some(config.messenger.socket.clone());
    }
    bridge_config::default_socket_path(None).ok()
}

/// Map a notification channel name (`"whatsapp"`, `"signal"`) to [`Platform`].
pub fn channel_platform(channel: &str) -> Option<Platform> {
    let lower = channel.trim().to_ascii_lowercase();
    if lower == "whatsapp" || lower.starts_with("whatsapp-") {
        Some(Platform::Whatsapp)
    } else if lower == "signal" || lower.starts_with("signal-") {
        Some(Platform::Signal)
    } else {
        None
    }
}

/// Convert bridge [`ContextMessage`] records into sanitized [`ChatContextMessage`]
/// entries for [`notify_filter::build_enrichment_prompt`].
pub fn convert_context_messages(messages: &[ContextMessage]) -> Vec<ChatContextMessage> {
    messages
        .iter()
        .filter(|m| !m.text.trim().is_empty())
        .map(|m| {
            let author = match m.author {
                ContextAuthor::Me => "me".to_string(),
                ContextAuthor::Them => {
                    let name = m.sender_name.trim();
                    if name.is_empty() {
                        "them".to_string()
                    } else {
                        name.to_string()
                    }
                }
            };
            let timestamp = LocalTime::from_epoch_ms(m.timestamp_ms).map(|lt| {
                let hh = lt.minutes_since_midnight / 60;
                let mm = lt.minutes_since_midnight % 60;
                format!("{hh:02}:{mm:02}")
            });
            ChatContextMessage {
                author,
                text: m.text.clone(),
                timestamp,
            }
        })
        .collect()
}

/// Opportunistically fetch recent bidirectional chat context from
/// `hey-computer-bridge.sock` with a hard 150 ms timeout, failing open to
/// `Vec::new()` on any connection, timeout, or protocol error.
pub async fn fetch_recent_context(
    socket_path: &Path,
    platform: Option<Platform>,
    sender_or_chat: &str,
    limit: usize,
) -> Vec<ChatContextMessage> {
    if sender_or_chat.trim().is_empty() || limit == 0 {
        return Vec::new();
    }
    let req = BridgeRequest::GetContext {
        platform,
        sender_or_chat: sender_or_chat.to_string(),
        limit: Some(limit),
    };
    let fut = async {
        let stream = UnixStream::connect(socket_path).await.ok()?;
        let (read, mut write) = stream.into_split();
        let mut line = encode_bridge(&req);
        line.push('\n');
        write.write_all(line.as_bytes()).await.ok()?;
        write.flush().await.ok()?;
        let mut reader = BufReader::new(read);
        match lines::read(&mut reader, MAX_BRIDGE_LINE).await.ok()? {
            Line::Text(raw) => serde_json::from_str::<BridgeEvent>(&raw).ok(),
            _ => None,
        }
    };
    match tokio::time::timeout(CONTEXT_FETCH_TIMEOUT, fut).await {
        Ok(Some(BridgeEvent::Context { messages, .. })) => convert_context_messages(&messages),
        _ => Vec::new(),
    }
}

/// Spawn the background messenger bridge subscriber task (or return an inactive
/// handle when `config.messenger.enabled` is `false`).
pub fn spawn(
    config: Arc<Config>,
    conductor: ConductorHandle,
    store: Store,
    presenter: Arc<dyn Presenter>,
) -> (MessengerHandle, JoinHandle<()>) {
    let state = Arc::new(Mutex::new(MessengerState::default()));
    let (stop_tx, stop_rx) = watch::channel(false);
    let handle = MessengerHandle {
        state: Arc::clone(&state),
        stop_tx,
    };

    if !config.messenger.enabled {
        let task = tokio::spawn(async {});
        return (handle, task);
    }

    let handle_clone = handle.clone();
    let task = tokio::spawn(async move {
        run_subscriber(config, conductor, store, presenter, handle_clone, stop_rx).await;
    });
    (handle, task)
}

async fn run_subscriber(
    config: Arc<Config>,
    conductor: ConductorHandle,
    store: Store,
    presenter: Arc<dyn Presenter>,
    handle: MessengerHandle,
    mut stop_rx: watch::Receiver<bool>,
) {
    let Some(socket) = resolve_bridge_socket(&config) else {
        tracing::warn!("messenger bridge enabled but cannot resolve bridge socket path");
        return;
    };
    let mut backoff_secs = MIN_RECONNECT_SECS;

    while !*stop_rx.borrow() {
        let connect_res = tokio::select! {
            res = Subscriber::connect(&socket) => Some(res),
            _ = stop_rx.changed() => None,
        };
        let Some(connect_res) = connect_res else {
            break;
        };
        let (ready, mut sub) = match connect_res {
            Ok(pair) => {
                backoff_secs = MIN_RECONNECT_SECS;
                pair
            }
            Err(e) => {
                tracing::debug!(
                    "messenger bridge unavailable at {}: {e:#}; retrying in {backoff_secs}s",
                    socket.display()
                );
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {}
                    _ = stop_rx.changed() => break,
                }
                backoff_secs = (backoff_secs * 2).min(MAX_RECONNECT_SECS);
                continue;
            }
        };

        if let BridgeEvent::Ready {
            whatsapp, signal, ..
        } = ready
        {
            if let Ok(mut s) = handle.state.lock() {
                s.connected = true;
                s.whatsapp = whatsapp;
                s.signal = signal;
            }
            tracing::info!(
                "connected to hey-computer-bridge (whatsapp={whatsapp}, signal={signal})"
            );
        }

        loop {
            let next = tokio::select! {
                ev = sub.next(None) => Some(ev),
                _ = stop_rx.changed() => None,
            };
            let Some(next) = next else {
                break;
            };
            match next {
                Ok(event) => {
                    handle_event(&config, &conductor, &store, &presenter, &handle, event).await;
                }
                Err(e) => {
                    tracing::debug!("messenger bridge stream ended: {e:#}");
                    break;
                }
            }
        }

        if let Ok(mut s) = handle.state.lock() {
            s.connected = false;
        }
        if *stop_rx.borrow() {
            break;
        }
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {}
            _ = stop_rx.changed() => break,
        }
        backoff_secs = (backoff_secs * 2).min(MAX_RECONNECT_SECS);
    }
}

async fn handle_event(
    config: &Arc<Config>,
    conductor: &ConductorHandle,
    store: &Store,
    presenter: &Arc<dyn Presenter>,
    handle: &MessengerHandle,
    event: BridgeEvent,
) {
    match event {
        BridgeEvent::PlatformState {
            platform,
            state,
            detail,
        } => {
            if let Ok(mut s) = handle.state.lock() {
                match platform {
                    Platform::Whatsapp => s.whatsapp = state,
                    Platform::Signal => s.signal = state,
                }
            }
            tracing::info!("messenger bridge {platform} state: {state}");
            if state == PlatformState::AuthRequired && handle.allow_auth_alert(platform) {
                let body = detail.unwrap_or_else(|| {
                    format!(
                        "{platform} session unlinked; run `hey-computer-bridge pair {platform}` to re-link."
                    )
                });
                presenter.present(Presentation::Notice {
                    summary: format!("Messenger bridge: {platform} needs re-linking"),
                    body,
                    urgency: Urgency::Normal,
                    replaces_id: None,
                });
            }
        }
        BridgeEvent::UserReplied {
            platform,
            chat_id,
            timestamp_ms,
        } => {
            let channel = platform.to_string();
            match store
                .dismiss_drafts_for_chat(channel, chat_id, timestamp_ms)
                .await
            {
                Ok(n) if n > 0 => {
                    tracing::info!("dismissed {n} pending {platform} draft(s) after phone reply");
                }
                Ok(_) => {}
                Err(e) => tracing::debug!("failed to dismiss drafts for chat: {e:#}"),
            }
        }
        BridgeEvent::Message {
            platform,
            kind: MessageKind::SelfChat,
            text,
            quoted_text,
            media_placeholder,
            ..
        } => {
            if !config.messenger.self_chat_enabled {
                return;
            }
            let prompt = format_self_chat_prompt(
                platform,
                &text,
                quoted_text.as_deref(),
                media_placeholder.as_deref(),
            );
            if prompt.trim().is_empty() {
                return;
            }
            let background = config.messenger.self_chat_lane == SelfChatLane::Background;
            let initial_icons = IconResolver::from_config(config, None)
                .command_icon(match platform {
                    Platform::Whatsapp => "whatsapp",
                    Platform::Signal => "signal",
                })
                .into_iter()
                .collect();
            let params = AskParams {
                text: prompt,
                new: true,
                profile: config.messenger.self_chat_profile.clone(),
                background,
                restricted: false,
                suppress_token: None,
                attachments: Vec::new(),
                // Typed source avoids unsolicited TTS while presenting on desktop only
                // (never sends anything back to WhatsApp/Signal).
                source: InputSource::Typed,
                reply_to: None,
                distill: false,
                schedule_id: None,
                schedule_has_script: false,
                initial_icons,
            };
            match conductor.ask(params, Sink::Present) {
                Ok(id) => tracing::info!(
                    "queued verified {platform} self-chat turn #{id} (background={background})"
                ),
                Err(msg) => tracing::warn!("rejected {platform} self-chat turn: {msg}"),
            }
        }
        BridgeEvent::Message {
            platform,
            kind: MessageKind::DirectMessage,
            chat_id,
            sender_name,
            text,
            quoted_text,
            media_placeholder,
            recent_messages,
            ..
        } => {
            if !config.messenger.prefer_over_notifications {
                return;
            }
            if conductor.is_locked() {
                return;
            }
            if is_messenger_quiet_now(config) {
                return;
            }
            let body = compose_dm_body(&text, quoted_text.as_deref(), media_placeholder.as_deref());
            if body.trim().is_empty() {
                return;
            }
            if config.notifications.drop_2fa_codes
                && notify_filter::contains_2fa_code(&sender_name, &body)
            {
                return;
            }
            if handle.was_recently_held(platform, &sender_name, &chat_id, &body) {
                tracing::debug!(
                    "skipping bridge DM draft for {platform}: already handled by NotifyHold"
                );
                return;
            }
            let cooldown = Duration::from_secs_f64(config.messenger.cooldown_secs.max(0.0));
            if !handle.allow_dm_cooldown(platform, &chat_id, cooldown) {
                return;
            }

            let channel = platform.to_string();
            let app_name = match platform {
                Platform::Whatsapp => "WhatsApp".to_string(),
                Platform::Signal => "Signal".to_string(),
            };
            let display_sender = if sender_name.trim().is_empty() {
                chat_id.clone()
            } else {
                sender_name
            };
            let prepared = PreparedNotification {
                channel: channel.clone(),
                app_name: app_name.clone(),
                sender: display_sender.clone(),
                chat_target: chat_id.clone(),
                body,
                window_class: channel.clone(),
                source_notif_id: None,
                readonly: true,
                is_group: false,
                group_name: None,
            };
            let ctx = convert_context_messages(&recent_messages);
            let prompt = notify_filter::build_enrichment_prompt(&prepared, &ctx);
            let profile = config
                .messenger
                .draft_profile
                .clone()
                .or_else(|| config.notifications.profile.clone());

            let (tx, rx) = oneshot::channel();
            let turn_id = match conductor.ask(
                AskParams {
                    text: prompt,
                    new: true,
                    profile,
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
                    tracing::debug!("skipping bridge DM draft for {platform}: {msg}");
                    return;
                }
            };

            let store = store.clone();
            let presenter = Arc::clone(presenter);
            let draft_ttl_ms = (config.notifications.draft_ttl_secs * 1000.0).round() as i64;
            tokio::spawn(async move {
                let Ok(Reply::Answer {
                    text: answer,
                    suppressed: false,
                    ..
                }) = rx.await
                else {
                    return;
                };
                let EnrichmentResult::Enriched {
                    context_note,
                    draft_text,
                } = notify_filter::parse_enrichment_output(&answer)
                else {
                    return;
                };
                let now_ms = store::unix_timestamp_ms();
                let ctx_note = context_note.unwrap_or_default();
                let draft = draft_text.unwrap_or_default();
                match store
                    .create_draft_checkpoint(NewDraftCheckpoint {
                        turn_id: Some(turn_id),
                        channel: channel.clone(),
                        app_name,
                        sender: display_sender.clone(),
                        chat_target: chat_id,
                        source_notif_id: None,
                        window_class: channel.clone(),
                        context_note: ctx_note.clone(),
                        draft_text: draft.clone(),
                        created_at_ms: now_ms,
                        expires_at_ms: now_ms.saturating_add(draft_ttl_ms),
                    })
                    .await
                {
                    Ok(cp) => {
                        presenter.present(Presentation::DraftReady {
                            id: cp.id,
                            turn_id: Some(turn_id),
                            token: cp.token,
                            channel,
                            sender: display_sender,
                            context_note: ctx_note,
                            draft_text: draft,
                            readonly: true,
                        });
                    }
                    Err(e) => {
                        tracing::warn!("cannot persist bridge draft checkpoint: {e:#}");
                    }
                }
            });
        }
        BridgeEvent::Message {
            kind: MessageKind::GroupMessage,
            ..
        }
        | BridgeEvent::Ready { .. }
        | BridgeEvent::Status { .. }
        | BridgeEvent::Context { .. }
        | BridgeEvent::Error { .. } => {}
    }
}

fn is_messenger_quiet_now(config: &Config) -> bool {
    let (start_s, end_s) =
        if !config.messenger.quiet_start.is_empty() && !config.messenger.quiet_end.is_empty() {
            (&config.messenger.quiet_start, &config.messenger.quiet_end)
        } else if !config.notifications.quiet_start.is_empty()
            && !config.notifications.quiet_end.is_empty()
        {
            (
                &config.notifications.quiet_start,
                &config.notifications.quiet_end,
            )
        } else {
            return false;
        };
    let Some((start, end)) =
        notify_filter::parse_hh_mm(start_s).zip(notify_filter::parse_hh_mm(end_s))
    else {
        return false;
    };
    let now_ms = store::unix_timestamp_ms();
    LocalTime::from_epoch_ms(now_ms)
        .is_some_and(|lt| notify_filter::is_in_quiet_hours(lt.minutes_since_midnight, start, end))
}

fn compose_dm_body(
    text: &str,
    quoted_text: Option<&str>,
    media_placeholder: Option<&str>,
) -> String {
    let mut parts = Vec::new();
    if let Some(q) = quoted_text.map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(format!("(replying to: {q})"));
    }
    let body = text.trim();
    if !body.is_empty() {
        parts.push(body.to_string());
    } else if let Some(m) = media_placeholder.map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(m.to_string());
    }
    parts.join("\n")
}

fn format_self_chat_prompt(
    platform: Platform,
    text: &str,
    quoted_text: Option<&str>,
    media_placeholder: Option<&str>,
) -> String {
    let body = compose_dm_body(text, quoted_text, media_placeholder);
    if body.trim().is_empty() {
        return String::new();
    }
    format!(
        "[From phone self-chat via {platform} — respond on desktop only; never attempt to send messages over {platform}]\n{body}"
    )
}
