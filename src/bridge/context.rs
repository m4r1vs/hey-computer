//! Persistent SQLite-backed + bounded LRU conversation context core, burst
//! debouncing, age fencing, Signal Desktop background importer, and the
//! read-only Unix socket broadcast & query server.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio::time::Instant;

use crate::bridge::config::{Allowlist, BridgeConfig, normalize_identifier};
use crate::bridge::protocol::{
    self, BridgeEvent, BridgeRequest, ContextAuthor, ContextMessage, MAX_BRIDGE_LINE, MessageKind,
    PROTOCOL_VERSION, Platform, PlatformState,
};
use crate::bridge::signal_desktop;
use crate::bridge::store::{BridgeStore, ChatMetaRecord, StoredMessageInput};
use crate::bridge::whatsapp::WhatsappControl;
use crate::lines::{self, Line};

/// Maximum UTF-8 bytes stored per context message in the in-memory ring buffer.
const MAX_CONTEXT_TEXT_BYTES: usize = 2048;
/// Bounded deduplication set size so reconnect replays never double-emit.
const MAX_SEEN_IDS: usize = 2048;
/// Client handshake read timeout.
const CLIENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Broadcast channel capacity for live subscribers.
const BROADCAST_CAPACITY: usize = 256;
/// Periodic Signal Desktop incremental sync interval.
const SIGNAL_DESKTOP_SYNC_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Normalized message emitted internally by the WhatsApp or Signal adapter.
#[derive(Debug, Clone)]
pub struct InboundRawMessage {
    pub id: String,
    pub platform: Platform,
    pub chat_id: String,
    pub chat_aliases: Vec<String>,
    pub raw_jid: Option<String>,
    pub chat_title: Option<String>,
    pub sender_id: String,
    pub sender_aliases: Vec<String>,
    pub sender_name: String,
    pub from_me: bool,
    pub is_group: bool,
    pub is_self_chat: bool,
    pub timestamp_ms: i64,
    pub text: String,
    pub quoted_text: Option<String>,
    pub media_placeholder: Option<String>,
}

/// Internal events sent from platform adapters into the central bridge core.
#[derive(Debug, Clone)]
pub enum RawAdapterEvent {
    PlatformStateChanged {
        platform: Platform,
        state: PlatformState,
        detail: Option<String>,
    },
    ContactAlias {
        platform: Platform,
        chat_id: String,
        aliases: Vec<String>,
        display_name: Option<String>,
    },
    ChatMetaBatch(Vec<ChatMetaRecord>),
    HistoryBatch {
        platform: Platform,
        progress: Option<u8>,
        messages: Vec<InboundRawMessage>,
    },
    Message(Box<InboundRawMessage>),
}

#[derive(Debug)]
enum CoreQuery {
    Status(oneshot::Sender<BridgeEvent>),
    Ready(oneshot::Sender<BridgeEvent>),
    GetContext {
        platform: Option<Platform>,
        sender_or_chat: String,
        limit: Option<usize>,
        reply: oneshot::Sender<BridgeEvent>,
    },
    ListChats {
        platform: Option<Platform>,
        include_groups: bool,
        include_dms: bool,
        limit: Option<usize>,
        reply: oneshot::Sender<BridgeEvent>,
    },
    GetMessages {
        platform: Option<Platform>,
        chat: String,
        before_ms: Option<i64>,
        after_ms: Option<i64>,
        limit: Option<usize>,
        reply: oneshot::Sender<BridgeEvent>,
    },
    Search {
        query: String,
        platform: Option<Platform>,
        chat: Option<String>,
        limit: Option<usize>,
        reply: oneshot::Sender<BridgeEvent>,
    },
    FetchHistory {
        platform: Platform,
        chat: String,
        count: Option<usize>,
        reply: oneshot::Sender<BridgeEvent>,
    },
    SyncSignalDesktop {
        reply: oneshot::Sender<BridgeEvent>,
    },
}

#[derive(Debug, Clone)]
struct ChatEntry {
    chat_id: String,
    aliases: HashSet<String>,
    display_names: Vec<String>,
    messages: VecDeque<ContextMessage>,
    last_touched: u64,
}

/// Bounded in-memory LRU ring buffer of recent messages per `(Platform, chat_id)`.
#[derive(Debug)]
pub struct ContextCache {
    max_chats: usize,
    max_messages_per_chat: usize,
    touch_seq: u64,
    chats: HashMap<(Platform, String), ChatEntry>,
}

impl ContextCache {
    pub fn new(max_chats: usize, max_messages_per_chat: usize) -> Self {
        Self {
            max_chats: max_chats.max(1),
            max_messages_per_chat: max_messages_per_chat.max(1),
            touch_seq: 0,
            chats: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.chats.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chats.is_empty()
    }

    pub fn record_contact(
        &mut self,
        platform: Platform,
        chat_id: &str,
        aliases: &[String],
        display_name: Option<&str>,
    ) {
        let canonical = normalize_identifier(chat_id);
        let entry = self.entry_mut(platform, &canonical);
        for alias in aliases {
            if !alias.trim().is_empty() {
                entry.aliases.insert(normalize_identifier(alias));
            }
        }
        if let Some(name) = display_name.map(str::trim).filter(|n| !n.is_empty())
            && !entry
                .display_names
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(name))
        {
            entry.display_names.push(name.to_string());
        }
    }

    pub fn push_message(
        &mut self,
        platform: Platform,
        chat_id: &str,
        aliases: &[String],
        display_name: Option<&str>,
        msg: ContextMessage,
    ) {
        let canonical = normalize_identifier(chat_id);
        let max_messages = self.max_messages_per_chat;
        let entry = self.entry_mut(platform, &canonical);
        for alias in aliases {
            if !alias.trim().is_empty() {
                entry.aliases.insert(normalize_identifier(alias));
            }
        }
        if let Some(name) = display_name.map(str::trim).filter(|n| !n.is_empty())
            && !entry
                .display_names
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(name))
        {
            entry.display_names.push(name.to_string());
        }
        let mut stored = msg;
        stored.text = truncate_utf8(&stored.text, MAX_CONTEXT_TEXT_BYTES);
        entry.messages.push_back(stored);
        while entry.messages.len() > max_messages {
            entry.messages.pop_front();
        }
    }

    pub fn recent(&self, platform: Platform, chat_id: &str) -> Vec<ContextMessage> {
        let canonical = normalize_identifier(chat_id);
        self.chats
            .get(&(platform, canonical))
            .map(|entry| entry.messages.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Look up recent messages by canonical chat ID, alias (phone/LID/UUID), or
    /// chat/contact display name (case-insensitive exact match, then substring).
    pub fn lookup(
        &self,
        platform: Option<Platform>,
        sender_or_chat: &str,
        limit: Option<usize>,
    ) -> Option<(Platform, String, Option<String>, Vec<ContextMessage>)> {
        let query = sender_or_chat.trim();
        if query.is_empty() {
            return None;
        }
        let norm_query = normalize_identifier(query);
        let lower_query = query.to_lowercase();

        let mut best: Option<(u8, u64, Platform, &ChatEntry)> = None;
        for ((plat, key), entry) in &self.chats {
            if let Some(filter) = platform
                && *plat != filter
            {
                continue;
            }
            let score = if *key == norm_query || entry.aliases.contains(&norm_query) {
                3u8
            } else if entry
                .display_names
                .iter()
                .any(|n| n.to_lowercase() == lower_query)
            {
                2u8
            } else if entry
                .display_names
                .iter()
                .any(|n| n.to_lowercase().contains(&lower_query))
            {
                1u8
            } else {
                0u8
            };
            if score == 0 {
                continue;
            }
            let replace = match best {
                None => true,
                Some((best_score, best_touch, _, _)) => {
                    score > best_score || (score == best_score && entry.last_touched > best_touch)
                }
            };
            if replace {
                best = Some((score, entry.last_touched, *plat, entry));
            }
        }

        let (_, _, plat, entry) = best?;
        let mut msgs: Vec<ContextMessage> = entry.messages.iter().cloned().collect();
        if let Some(limit) = limit
            && msgs.len() > limit
        {
            let keep_from = msgs.len() - limit;
            msgs.drain(..keep_from);
        }
        Some((
            plat,
            entry.chat_id.clone(),
            entry.display_names.last().cloned(),
            msgs,
        ))
    }

    fn entry_mut(&mut self, platform: Platform, canonical_chat_id: &str) -> &mut ChatEntry {
        self.touch_seq = self.touch_seq.saturating_add(1);
        let seq = self.touch_seq;
        let key = (platform, canonical_chat_id.to_string());
        if !self.chats.contains_key(&key) {
            while self.chats.len() >= self.max_chats {
                let oldest_key = self
                    .chats
                    .iter()
                    .min_by_key(|(_, entry)| entry.last_touched)
                    .map(|(k, _)| k.clone());
                if let Some(k) = oldest_key {
                    self.chats.remove(&k);
                } else {
                    break;
                }
            }
            let mut aliases = HashSet::new();
            aliases.insert(canonical_chat_id.to_string());
            self.chats.insert(
                key.clone(),
                ChatEntry {
                    chat_id: canonical_chat_id.to_string(),
                    aliases,
                    display_names: Vec::new(),
                    messages: VecDeque::new(),
                    last_touched: seq,
                },
            );
        }
        let entry = self.chats.get_mut(&key).expect("entry inserted above");
        entry.last_touched = seq;
        entry
    }
}

#[derive(Debug, Clone)]
struct PendingBurst {
    kind: MessageKind,
    first_at: Instant,
    flush_at: Instant,
    latest: InboundRawMessage,
    texts: Vec<String>,
    quoted_text: Option<String>,
    media_placeholder: Option<String>,
}

struct BridgeCore {
    config: BridgeConfig,
    db_path: PathBuf,
    store: BridgeStore,
    whatsapp_allow: Allowlist,
    signal_allow: Allowlist,
    whatsapp_state: PlatformState,
    whatsapp_detail: Option<String>,
    whatsapp_sync_progress: Option<u8>,
    whatsapp_ctrl_tx: Option<mpsc::Sender<WhatsappControl>>,
    signal_state: PlatformState,
    signal_detail: Option<String>,
    context: ContextCache,
    seen_order: VecDeque<(Platform, String)>,
    seen_set: HashSet<(Platform, String)>,
    pending: HashMap<(Platform, String), PendingBurst>,
}

impl BridgeCore {
    fn new(
        config: BridgeConfig,
        whatsapp_ctrl_tx: Option<mpsc::Sender<WhatsappControl>>,
    ) -> Result<Self> {
        let state_dir = config.resolved_state_dir()?;
        let db_path = state_dir.join("history.db");
        let store = BridgeStore::open(&db_path)?;

        let whatsapp_allow = Allowlist::load(
            &config.whatsapp.allow_from,
            config.whatsapp.allow_from_file.as_deref(),
        )?;
        let signal_allow = Allowlist::load(
            &config.signal.allow_from,
            config.signal.allow_from_file.as_deref(),
        )?;
        let whatsapp_state = if config.whatsapp.enabled {
            PlatformState::Starting
        } else {
            PlatformState::Disabled
        };
        let signal_state = if config.signal.enabled {
            PlatformState::Starting
        } else {
            PlatformState::Disabled
        };
        tracing::info!(
            whatsapp_enabled = config.whatsapp.enabled,
            whatsapp_allow_count = whatsapp_allow.len(),
            signal_enabled = config.signal.enabled,
            signal_allow_count = signal_allow.len(),
            "bridge core initialized (strictly read-only)"
        );
        let context = ContextCache::new(config.max_tracked_chats, config.context_messages);
        Ok(Self {
            config,
            db_path,
            store,
            whatsapp_allow,
            signal_allow,
            whatsapp_state,
            whatsapp_detail: None,
            whatsapp_sync_progress: None,
            whatsapp_ctrl_tx,
            signal_state,
            signal_detail: None,
            context,
            seen_order: VecDeque::new(),
            seen_set: HashSet::new(),
            pending: HashMap::new(),
        })
    }

    fn ready_event(&self) -> BridgeEvent {
        BridgeEvent::Ready {
            version: PROTOCOL_VERSION,
            read_only: true,
            whatsapp: self.whatsapp_state,
            signal: self.signal_state,
        }
    }

    fn status_event(&self) -> BridgeEvent {
        let (stored_chats, stored_messages) = self.store.counts().unwrap_or((0, 0));
        BridgeEvent::Status {
            version: PROTOCOL_VERSION,
            read_only: true,
            whatsapp: self.whatsapp_state,
            signal: self.signal_state,
            whatsapp_detail: self.whatsapp_detail.clone(),
            signal_detail: self.signal_detail.clone(),
            tracked_chats: self.context.len(),
            stored_chats,
            stored_messages,
            whatsapp_sync_progress: self.whatsapp_sync_progress,
        }
    }

    fn context_event(
        &self,
        platform: Option<Platform>,
        sender_or_chat: &str,
        limit: Option<usize>,
    ) -> BridgeEvent {
        let lim = limit.unwrap_or(self.config.context_messages).clamp(1, 100);
        if let Ok((Some(chat), rows)) =
            self.store
                .get_messages(platform, sender_or_chat, None, None, lim)
            && !rows.is_empty()
        {
            let messages = rows.iter().map(stored_to_context_message).collect();
            return BridgeEvent::Context {
                platform: Some(chat.platform),
                chat_id: Some(chat.chat_id),
                sender_name: (!chat.title.is_empty()).then_some(chat.title),
                messages,
            };
        }

        match self.context.lookup(platform, sender_or_chat, limit) {
            Some((plat, chat_id, sender_name, messages)) => BridgeEvent::Context {
                platform: Some(plat),
                chat_id: Some(chat_id),
                sender_name,
                messages,
            },
            None => BridgeEvent::Context {
                platform,
                chat_id: None,
                sender_name: None,
                messages: Vec::new(),
            },
        }
    }

    fn recent_for_chat(&self, platform: Platform, canonical_chat: &str) -> Vec<ContextMessage> {
        if let Ok((_, rows)) = self.store.get_messages(
            Some(platform),
            canonical_chat,
            None,
            None,
            self.config.context_messages,
        ) && !rows.is_empty()
        {
            return rows.iter().map(stored_to_context_message).collect();
        }
        self.context.recent(platform, canonical_chat)
    }

    fn handle_raw_event(
        &mut self,
        event: RawAdapterEvent,
        now_instant: Instant,
        now_ms: i64,
    ) -> Vec<BridgeEvent> {
        match event {
            RawAdapterEvent::PlatformStateChanged {
                platform,
                state,
                detail,
            } => {
                let changed = match platform {
                    Platform::Whatsapp => {
                        let c = self.whatsapp_state != state || self.whatsapp_detail != detail;
                        self.whatsapp_state = state;
                        self.whatsapp_detail = detail.clone();
                        c
                    }
                    Platform::Signal => {
                        let c = self.signal_state != state || self.signal_detail != detail;
                        self.signal_state = state;
                        self.signal_detail = detail.clone();
                        c
                    }
                };
                if changed {
                    tracing::info!(%platform, %state, "platform state transition");
                    vec![BridgeEvent::PlatformState {
                        platform,
                        state,
                        detail,
                    }]
                } else {
                    Vec::new()
                }
            }
            RawAdapterEvent::ContactAlias {
                platform,
                chat_id,
                aliases,
                display_name,
            } => {
                self.context
                    .record_contact(platform, &chat_id, &aliases, display_name.as_deref());
                if let Err(e) = self.store.upsert_chat_meta(&ChatMetaRecord {
                    platform,
                    chat_id,
                    aliases,
                    is_group: false,
                    is_self_chat: false,
                    title: display_name,
                    raw_jid: None,
                }) {
                    tracing::warn!("failed persisting contact alias: {e:#}");
                }
                Vec::new()
            }
            RawAdapterEvent::ChatMetaBatch(items) => {
                let filtered: Vec<ChatMetaRecord> = items
                    .into_iter()
                    .filter(|m| !m.is_group || self.groups_enabled_for(m.platform))
                    .collect();
                for m in &filtered {
                    self.context.record_contact(
                        m.platform,
                        &m.chat_id,
                        &m.aliases,
                        m.title.as_deref(),
                    );
                }
                if let Err(e) = self.store.upsert_chat_meta_batch(&filtered) {
                    tracing::warn!("failed persisting chat_meta_batch: {e:#}");
                }
                Vec::new()
            }
            RawAdapterEvent::HistoryBatch {
                platform,
                progress,
                messages,
            } => {
                if platform == Platform::Whatsapp
                    && let Some(p) = progress
                {
                    self.whatsapp_sync_progress =
                        Some(self.whatsapp_sync_progress.map_or(p, |prev| prev.max(p)));
                }
                let batch_inputs: Vec<StoredMessageInput> = messages
                    .iter()
                    .filter(|m| !m.is_group || self.groups_enabled_for(m.platform))
                    .map(raw_to_store_input)
                    .collect();
                if !batch_inputs.is_empty() {
                    match self
                        .store
                        .upsert_messages_batch(&batch_inputs, self.config.max_stored_messages)
                    {
                        Ok(n) if n > 0 => {
                            tracing::info!(
                                %platform,
                                imported = n,
                                progress = ?self.whatsapp_sync_progress,
                                "indexed messenger history batch"
                            );
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!("failed indexing history_batch: {e:#}"),
                    }
                }
                Vec::new()
            }
            RawAdapterEvent::Message(msg) => self.handle_message(*msg, now_instant, now_ms),
        }
    }

    fn groups_enabled_for(&self, platform: Platform) -> bool {
        match platform {
            Platform::Whatsapp => self.config.whatsapp.groups_enabled,
            Platform::Signal => self.config.signal.groups_enabled,
        }
    }

    fn is_duplicate(&mut self, platform: Platform, id: &str) -> bool {
        let key = (platform, id.to_string());
        if self.seen_set.contains(&key) {
            return true;
        }
        self.seen_set.insert(key.clone());
        self.seen_order.push_back(key);
        while self.seen_order.len() > MAX_SEEN_IDS {
            if let Some(old) = self.seen_order.pop_front() {
                self.seen_set.remove(&old);
            }
        }
        false
    }

    fn persist_live_message(&mut self, msg: &InboundRawMessage, canonical_chat: &str) {
        let store_input = raw_to_store_input(msg);
        if let Err(e) = self
            .store
            .upsert_messages_batch(&[store_input], self.config.max_stored_messages)
        {
            tracing::warn!("failed persisting live message into history.db: {e:#}");
        }

        // Keep the bounded RAM cache synchronized as well (strictly separating
        // group chat_title from group member sender_name).
        if !msg.is_self_chat {
            let body = effective_text(&msg.text, msg.media_placeholder.as_deref());
            if !body.is_empty() {
                let chat_display_name: Option<&str> = if msg.is_group {
                    msg.chat_title.as_deref()
                } else {
                    msg.chat_title.as_deref().or_else(|| {
                        if !msg.sender_name.trim().is_empty() {
                            Some(msg.sender_name.trim())
                        } else {
                            None
                        }
                    })
                };
                let author = if msg.from_me {
                    ContextAuthor::Me
                } else {
                    ContextAuthor::Them
                };
                let speaker = if msg.from_me {
                    "Me".to_string()
                } else if msg.sender_name.trim().is_empty() {
                    normalize_identifier(&msg.sender_id)
                } else {
                    msg.sender_name.trim().to_string()
                };
                let mut aliases = msg.chat_aliases.clone();
                if !msg.is_group && !msg.from_me {
                    aliases.extend(msg.sender_aliases.iter().cloned());
                }
                self.context.push_message(
                    msg.platform,
                    canonical_chat,
                    &aliases,
                    chat_display_name,
                    ContextMessage {
                        author,
                        sender_name: speaker,
                        timestamp_ms: msg.timestamp_ms,
                        text: body,
                    },
                );
            }
        }
    }

    fn handle_message(
        &mut self,
        msg: InboundRawMessage,
        now_instant: Instant,
        now_ms: i64,
    ) -> Vec<BridgeEvent> {
        if self.is_duplicate(msg.platform, &msg.id) {
            tracing::debug!(platform = %msg.platform, "dropping duplicate message id");
            return Vec::new();
        }

        if msg.is_group && !self.groups_enabled_for(msg.platform) {
            return Vec::new();
        }

        let canonical_chat = normalize_identifier(&msg.chat_id);
        let max_age_ms = (self.config.max_message_age_secs as i64).saturating_mul(1000);
        let is_stale = now_ms.saturating_sub(msg.timestamp_ms) > max_age_ms;

        // CRITICAL INDEXING SPLIT:
        // Index every valid DM, self-chat, and enabled group message into
        // `history.db` + `ContextCache` BEFORE allowlist / staleness gating so
        // `hey-computer-bridge chats|groups|messages|search` has complete history,
        // while live `BridgeEvent::Message` broadcast triggers remain strictly
        // gated by `is_stale` and `allowlist`.
        self.persist_live_message(&msg, &canonical_chat);

        // 1. Verified Phone Self-Chat
        if msg.from_me && msg.is_self_chat {
            let self_chat_enabled = match msg.platform {
                Platform::Whatsapp => self.config.whatsapp.self_chat,
                Platform::Signal => self.config.signal.self_chat,
            };
            if !self_chat_enabled || msg.is_group {
                return Vec::new();
            }
            if is_stale {
                tracing::debug!(
                    platform = %msg.platform,
                    "age-fenced historical self-chat message"
                );
                return Vec::new();
            }
            let delay = Duration::from_millis(self.config.self_chat_debounce_ms);
            self.enqueue_burst(
                msg.platform,
                canonical_chat,
                MessageKind::SelfChat,
                msg,
                now_instant,
                delay,
            );
            return Vec::new();
        }

        // 2. Outbound message sent by the user to another contact or group (e.g. from phone)
        if msg.from_me && !msg.is_self_chat {
            // User answered the chat directly: cancel any pending inbound burst.
            self.pending.remove(&(msg.platform, canonical_chat.clone()));

            if is_stale {
                return Vec::new();
            }
            tracing::info!(platform = %msg.platform, "recorded outbound reply (user_replied)");
            return vec![BridgeEvent::UserReplied {
                platform: msg.platform,
                chat_id: canonical_chat,
                timestamp_ms: msg.timestamp_ms,
            }];
        }

        // 3. Inbound message from another contact (DM or enabled group): gate live triggers by allowlist + age fence.
        let allowlist = match msg.platform {
            Platform::Whatsapp => &self.whatsapp_allow,
            Platform::Signal => &self.signal_allow,
        };
        let mut candidates: Vec<&str> = Vec::new();
        candidates.push(msg.chat_id.as_str());
        candidates.push(msg.sender_id.as_str());
        if let Some(title) = msg.chat_title.as_deref() {
            candidates.push(title);
        }
        for a in &msg.chat_aliases {
            candidates.push(a.as_str());
        }
        for a in &msg.sender_aliases {
            candidates.push(a.as_str());
        }
        if !allowlist.matches_any(candidates) {
            tracing::debug!(
                platform = %msg.platform,
                "indexed inbound message; skipping live broadcast (not on allowlist)"
            );
            return Vec::new();
        }

        if is_stale {
            tracing::debug!(
                platform = %msg.platform,
                "age-fenced historical inbound message (indexed for context/history only)"
            );
            return Vec::new();
        }

        let kind = if msg.is_group {
            MessageKind::GroupMessage
        } else {
            MessageKind::DirectMessage
        };
        let delay = Duration::from_millis(self.config.debounce_ms);
        self.enqueue_burst(msg.platform, canonical_chat, kind, msg, now_instant, delay);
        Vec::new()
    }

    fn enqueue_burst(
        &mut self,
        platform: Platform,
        canonical_chat: String,
        kind: MessageKind,
        msg: InboundRawMessage,
        now: Instant,
        debounce: Duration,
    ) {
        let max_window = Duration::from_millis(self.config.max_debounce_ms.max(1));
        let key = (platform, canonical_chat);
        let text_piece = msg.text.trim().to_string();

        if let Some(existing) = self.pending.get_mut(&key) {
            if !text_piece.is_empty() {
                existing.texts.push(text_piece);
            }
            if msg.quoted_text.is_some() {
                existing.quoted_text = msg.quoted_text.clone();
            }
            if msg.media_placeholder.is_some() {
                existing.media_placeholder = msg.media_placeholder.clone();
            }
            existing.latest = msg;
            let max_deadline = existing.first_at + max_window;
            existing.flush_at = (now + debounce).min(max_deadline);
        } else {
            let mut texts = Vec::new();
            if !text_piece.is_empty() {
                texts.push(text_piece);
            }
            let quoted_text = msg.quoted_text.clone();
            let media_placeholder = msg.media_placeholder.clone();
            self.pending.insert(
                key,
                PendingBurst {
                    kind,
                    first_at: now,
                    flush_at: now + debounce,
                    latest: msg,
                    texts,
                    quoted_text,
                    media_placeholder,
                },
            );
        }
    }

    fn next_flush_deadline(&self) -> Option<Instant> {
        self.pending.values().map(|b| b.flush_at).min()
    }

    fn flush_due(&mut self, now: Instant) -> Vec<BridgeEvent> {
        let due_keys: Vec<(Platform, String)> = self
            .pending
            .iter()
            .filter(|(_, burst)| burst.flush_at <= now)
            .map(|(k, _)| k.clone())
            .collect();
        let mut out = Vec::new();
        for key in due_keys {
            if let Some(burst) = self.pending.remove(&key) {
                let (platform, canonical_chat) = key;
                let text = burst.texts.join("\n");
                let recent_messages = match burst.kind {
                    MessageKind::SelfChat => Vec::new(),
                    _ => self.recent_for_chat(platform, &canonical_chat),
                };
                let sender_id = normalize_identifier(&burst.latest.sender_id);
                let sender_name = if burst.latest.sender_name.trim().is_empty() {
                    if burst.kind == MessageKind::SelfChat {
                        "Me".to_string()
                    } else {
                        sender_id.clone()
                    }
                } else {
                    burst.latest.sender_name.trim().to_string()
                };
                tracing::info!(
                    %platform,
                    kind = ?burst.kind,
                    recent_count = recent_messages.len(),
                    "emitting bridge message event"
                );
                out.push(BridgeEvent::Message {
                    id: format!("{platform}:{}", burst.latest.id),
                    platform,
                    kind: burst.kind,
                    chat_id: canonical_chat,
                    sender_id,
                    sender_name,
                    timestamp_ms: burst.latest.timestamp_ms,
                    text,
                    quoted_text: burst.quoted_text,
                    media_placeholder: burst.media_placeholder,
                    recent_messages,
                });
            }
        }
        out
    }

    fn handle_list_chats(
        &self,
        platform: Option<Platform>,
        include_groups: bool,
        include_dms: bool,
        limit: Option<usize>,
    ) -> BridgeEvent {
        let lim = limit.unwrap_or(50).clamp(1, 5_000);
        match self
            .store
            .list_chats(platform, include_groups, include_dms, lim)
        {
            Ok(chats) => BridgeEvent::Chats { chats },
            Err(e) => BridgeEvent::Error {
                message: format!("list_chats failed: {e}"),
            },
        }
    }

    fn handle_get_messages(
        &self,
        platform: Option<Platform>,
        chat: &str,
        before_ms: Option<i64>,
        after_ms: Option<i64>,
        limit: Option<usize>,
    ) -> BridgeEvent {
        let lim = limit.unwrap_or(50).clamp(1, 5_000);
        match self
            .store
            .get_messages(platform, chat, before_ms, after_ms, lim)
        {
            Ok((chat_summary, messages)) => BridgeEvent::Messages {
                chat: chat_summary,
                messages,
            },
            Err(e) => BridgeEvent::Error {
                message: format!("get_messages failed: {e}"),
            },
        }
    }

    fn handle_search(
        &self,
        query: &str,
        platform: Option<Platform>,
        chat: Option<&str>,
        limit: Option<usize>,
    ) -> BridgeEvent {
        let lim = limit.unwrap_or(30).clamp(1, 1_000);
        match self.store.search_messages(query, platform, chat, lim) {
            Ok(hits) => BridgeEvent::SearchResults { hits },
            Err(e) => BridgeEvent::Error {
                message: format!("search failed: {e}"),
            },
        }
    }

    fn handle_fetch_history(
        &self,
        platform: Platform,
        chat: &str,
        count: Option<usize>,
    ) -> BridgeEvent {
        if platform != Platform::Whatsapp {
            return BridgeEvent::Error {
                message: "fetch_history is only supported for whatsapp (use sync_signal_desktop for signal)"
                    .to_string(),
            };
        }
        let Some(ctrl_tx) = &self.whatsapp_ctrl_tx else {
            return BridgeEvent::Error {
                message: "whatsapp adapter is disabled".to_string(),
            };
        };
        if self.whatsapp_state != PlatformState::Connected {
            return BridgeEvent::Error {
                message: format!("whatsapp is currently {}", self.whatsapp_state),
            };
        }
        let cursor = match self.store.oldest_cursor(Platform::Whatsapp, chat) {
            Ok(Some(c)) => c,
            Ok(None) => {
                return BridgeEvent::Error {
                    message: format!("no stored WhatsApp message cursor found for '{chat}'"),
                };
            }
            Err(e) => {
                return BridgeEvent::Error {
                    message: format!("lookup cursor failed: {e}"),
                };
            }
        };
        let clamped_count = count.unwrap_or(50).clamp(1, 200);
        match ctrl_tx.try_send(WhatsappControl::FetchHistory {
            chat_jid: cursor.raw_jid,
            oldest_msg_id: cursor.oldest_msg_id,
            oldest_from_me: cursor.oldest_msg_from_me,
            oldest_ts_ms: cursor.oldest_msg_ts_ms,
            count: clamped_count,
        }) {
            Ok(()) => BridgeEvent::HistoryRequested {
                chat_id: cursor.chat_id,
                count: clamped_count,
            },
            Err(_) => BridgeEvent::Error {
                message: "whatsapp control channel is busy or closed".to_string(),
            },
        }
    }
}

pub(crate) fn raw_to_store_input(msg: &InboundRawMessage) -> StoredMessageInput {
    StoredMessageInput {
        platform: msg.platform,
        chat_id: msg.chat_id.clone(),
        chat_aliases: msg.chat_aliases.clone(),
        raw_jid: msg.raw_jid.clone(),
        msg_id: msg.id.clone(),
        timestamp_ms: msg.timestamp_ms,
        from_me: msg.from_me,
        is_group: msg.is_group,
        is_self_chat: msg.is_self_chat,
        chat_title: msg.chat_title.clone(),
        sender_id: msg.sender_id.clone(),
        sender_name: msg.sender_name.clone(),
        text: msg.text.clone(),
        quoted_text: msg.quoted_text.clone(),
        media_placeholder: msg.media_placeholder.clone(),
    }
}

fn stored_to_context_message(m: &crate::bridge::protocol::StoredMessage) -> ContextMessage {
    let author = if m.from_me {
        ContextAuthor::Me
    } else {
        ContextAuthor::Them
    };
    let sender_name = if m.from_me {
        "Me".to_string()
    } else if m.sender_name.trim().is_empty() {
        m.sender_id.clone()
    } else {
        m.sender_name.clone()
    };
    let body = truncate_utf8(
        &effective_text(&m.text, m.media_placeholder.as_deref()),
        MAX_CONTEXT_TEXT_BYTES,
    );
    ContextMessage {
        author,
        sender_name,
        timestamp_ms: m.timestamp_ms,
        text: body,
    }
}

fn spawn_signal_desktop_sync_task(
    config: BridgeConfig,
    db_path: PathBuf,
    sync_lock: Arc<Mutex<()>>,
    reply: Option<oneshot::Sender<BridgeEvent>>,
) {
    tokio::spawn(async move {
        let _guard = sync_lock.lock().await;
        let res = tokio::task::spawn_blocking(move || {
            BridgeStore::open(&db_path)
                .and_then(|mut store| signal_desktop::sync_signal_desktop(&config, &mut store))
        })
        .await
        .map_err(|e| anyhow::anyhow!("Signal Desktop sync worker panicked: {e}"))
        .and_then(|r| r);

        match res {
            Ok(summary) => {
                if summary.messages_imported > 0 {
                    tracing::info!(
                        conversations = summary.conversations,
                        messages_imported = summary.messages_imported,
                        "synced Signal Desktop SQLCipher history"
                    );
                }
                if let Some(tx) = reply {
                    let _ = tx.send(BridgeEvent::SyncCompleted {
                        conversations: summary.conversations,
                        messages_imported: summary.messages_imported,
                    });
                }
            }
            Err(e) => {
                tracing::warn!("Signal Desktop SQLCipher sync failed: {e:#}");
                if let Some(tx) = reply {
                    let _ = tx.send(BridgeEvent::Error {
                        message: format!("Signal Desktop sync failed: {e}"),
                    });
                }
            }
        }
    });
}

/// Serve `hey-computer-bridge` on `socket` until `shutdown` resolves.
pub async fn serve(
    config: BridgeConfig,
    socket: &Path,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    config.validate()?;
    let listener = bind_socket(socket).await?;
    tracing::info!("hey-computer-bridge listening on {}", socket.display());

    let (wa_ctrl_tx, wa_ctrl_rx) = if config.whatsapp.enabled {
        let (tx, rx) = mpsc::channel::<WhatsappControl>(32);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    let mut core = match BridgeCore::new(config.clone(), wa_ctrl_tx) {
        Ok(core) => core,
        Err(e) => {
            let _ = std::fs::remove_file(socket);
            return Err(e);
        }
    };

    let sync_lock = Arc::new(Mutex::new(()));
    if config.resolved_signal_desktop_dir().is_some() {
        spawn_signal_desktop_sync_task(
            config.clone(),
            core.db_path.clone(),
            sync_lock.clone(),
            None,
        );
    }

    let (raw_tx, mut raw_rx) = mpsc::channel::<RawAdapterEvent>(512);
    let (query_tx, mut query_rx) = mpsc::channel::<CoreQuery>(64);
    let (bcast_tx, _) = broadcast::channel::<BridgeEvent>(BROADCAST_CAPACITY);
    let (adapter_stop_tx, adapter_stop_rx) = tokio::sync::watch::channel(false);

    let mut adapters = tokio::task::JoinSet::new();
    if let Some(ctrl_rx) = wa_ctrl_rx {
        let cfg = config.clone();
        let tx = raw_tx.clone();
        let stop = adapter_stop_rx.clone();
        adapters.spawn(async move {
            crate::bridge::whatsapp::run(cfg, tx, ctrl_rx, stop).await;
        });
    }
    if config.signal.enabled {
        let cfg = config.clone();
        let tx = raw_tx.clone();
        let stop = adapter_stop_rx.clone();
        adapters.spawn(async move {
            crate::bridge::signal::run(cfg, tx, stop).await;
        });
    }
    drop(raw_tx);

    let mut sig_desktop_tick = tokio::time::interval(SIGNAL_DESKTOP_SYNC_INTERVAL);
    // First tick fires immediately; we already kicked off initial sync above.
    sig_desktop_tick.tick().await;

    let mut connections = tokio::task::JoinSet::new();
    tokio::pin!(shutdown);

    loop {
        let next_deadline = core.next_flush_deadline();
        tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let qtx = query_tx.clone();
                    let btx = bcast_tx.clone();
                    let stop = adapter_stop_rx.clone();
                    connections.spawn(async move {
                        if let Err(e) = handle_client(stream, qtx, btx, stop).await {
                            tracing::debug!("bridge client connection ended: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("bridge socket accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(raw) = raw_rx.recv() => {
                let events = core.handle_raw_event(raw, Instant::now(), unix_timestamp_ms());
                for ev in events {
                    let _ = bcast_tx.send(ev);
                }
            }
            Some(query) = query_rx.recv() => {
                match query {
                    CoreQuery::Status(reply) => {
                        let _ = reply.send(core.status_event());
                    }
                    CoreQuery::Ready(reply) => {
                        let _ = reply.send(core.ready_event());
                    }
                    CoreQuery::GetContext { platform, sender_or_chat, limit, reply } => {
                        let _ = reply.send(core.context_event(platform, &sender_or_chat, limit));
                    }
                    CoreQuery::ListChats { platform, include_groups, include_dms, limit, reply } => {
                        let _ = reply.send(core.handle_list_chats(platform, include_groups, include_dms, limit));
                    }
                    CoreQuery::GetMessages { platform, chat, before_ms, after_ms, limit, reply } => {
                        let _ = reply.send(core.handle_get_messages(platform, &chat, before_ms, after_ms, limit));
                    }
                    CoreQuery::Search { query, platform, chat, limit, reply } => {
                        let _ = reply.send(core.handle_search(&query, platform, chat.as_deref(), limit));
                    }
                    CoreQuery::FetchHistory { platform, chat, count, reply } => {
                        let _ = reply.send(core.handle_fetch_history(platform, &chat, count));
                    }
                    CoreQuery::SyncSignalDesktop { reply } => {
                        spawn_signal_desktop_sync_task(
                            core.config.clone(),
                            core.db_path.clone(),
                            sync_lock.clone(),
                            Some(reply),
                        );
                    }
                }
            }
            _ = sig_desktop_tick.tick() => {
                if core.config.resolved_signal_desktop_dir().is_some() {
                    spawn_signal_desktop_sync_task(
                        core.config.clone(),
                        core.db_path.clone(),
                        sync_lock.clone(),
                        None,
                    );
                }
            }
            () = async {
                match next_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let events = core.flush_due(Instant::now());
                for ev in events {
                    let _ = bcast_tx.send(ev);
                }
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }

    tracing::info!("hey-computer-bridge shutting down");
    drop(listener);
    let _ = std::fs::remove_file(socket);
    let _ = adapter_stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while adapters.join_next().await.is_some() {}
    })
    .await;
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

async fn handle_client(
    stream: UnixStream,
    query_tx: mpsc::Sender<CoreQuery>,
    bcast_tx: broadcast::Sender<BridgeEvent>,
    mut stop_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let line = tokio::time::timeout(
        CLIENT_REQUEST_TIMEOUT,
        lines::read(&mut reader, MAX_BRIDGE_LINE),
    )
    .await
    .context("client timed out before sending request")?
    .context("reading client request")?;

    let text = match line {
        Line::Text(t) => t,
        Line::Eof => return Ok(()),
        Line::TooLong(_) => {
            send_event(
                &mut write,
                &BridgeEvent::Error {
                    message: "request line too long".to_string(),
                },
            )
            .await?;
            return Ok(());
        }
        Line::NotUtf8 => {
            send_event(
                &mut write,
                &BridgeEvent::Error {
                    message: "request must be UTF-8".to_string(),
                },
            )
            .await?;
            return Ok(());
        }
    };

    let req = match protocol::parse_request(&text) {
        Ok(r) => r,
        Err(message) => {
            send_event(&mut write, &BridgeEvent::Error { message }).await?;
            return Ok(());
        }
    };

    match req {
        BridgeRequest::Status => {
            let (tx, rx) = oneshot::channel();
            query_tx
                .send(CoreQuery::Status(tx))
                .await
                .context("bridge core stopped")?;
            let reply = rx.await.context("bridge core dropped status reply")?;
            send_event(&mut write, &reply).await?;
        }
        BridgeRequest::GetContext {
            platform,
            sender_or_chat,
            limit,
        } => {
            let (tx, rx) = oneshot::channel();
            query_tx
                .send(CoreQuery::GetContext {
                    platform,
                    sender_or_chat,
                    limit,
                    reply: tx,
                })
                .await
                .context("bridge core stopped")?;
            let reply = rx.await.context("bridge core dropped context reply")?;
            send_event(&mut write, &reply).await?;
        }
        BridgeRequest::ListChats {
            platform,
            include_groups,
            include_dms,
            limit,
        } => {
            let (tx, rx) = oneshot::channel();
            query_tx
                .send(CoreQuery::ListChats {
                    platform,
                    include_groups,
                    include_dms,
                    limit,
                    reply: tx,
                })
                .await
                .context("bridge core stopped")?;
            let reply = rx.await.context("bridge core dropped list_chats reply")?;
            send_event(&mut write, &reply).await?;
        }
        BridgeRequest::GetMessages {
            platform,
            chat,
            before_ms,
            after_ms,
            limit,
        } => {
            let (tx, rx) = oneshot::channel();
            query_tx
                .send(CoreQuery::GetMessages {
                    platform,
                    chat,
                    before_ms,
                    after_ms,
                    limit,
                    reply: tx,
                })
                .await
                .context("bridge core stopped")?;
            let reply = rx.await.context("bridge core dropped get_messages reply")?;
            send_event(&mut write, &reply).await?;
        }
        BridgeRequest::Search {
            query,
            platform,
            chat,
            limit,
        } => {
            let (tx, rx) = oneshot::channel();
            query_tx
                .send(CoreQuery::Search {
                    query,
                    platform,
                    chat,
                    limit,
                    reply: tx,
                })
                .await
                .context("bridge core stopped")?;
            let reply = rx.await.context("bridge core dropped search reply")?;
            send_event(&mut write, &reply).await?;
        }
        BridgeRequest::FetchHistory {
            platform,
            chat,
            count,
        } => {
            let (tx, rx) = oneshot::channel();
            query_tx
                .send(CoreQuery::FetchHistory {
                    platform,
                    chat,
                    count,
                    reply: tx,
                })
                .await
                .context("bridge core stopped")?;
            let reply = rx
                .await
                .context("bridge core dropped fetch_history reply")?;
            send_event(&mut write, &reply).await?;
        }
        BridgeRequest::SyncSignalDesktop => {
            let (tx, rx) = oneshot::channel();
            query_tx
                .send(CoreQuery::SyncSignalDesktop { reply: tx })
                .await
                .context("bridge core stopped")?;
            let reply = rx.await.context("bridge core dropped sync reply")?;
            send_event(&mut write, &reply).await?;
        }
        BridgeRequest::Subscribe => {
            // Subscribe before fetching Ready so no state transitions between Ready and stream are missed.
            let mut rx = bcast_tx.subscribe();
            let (tx, ready_rx) = oneshot::channel();
            query_tx
                .send(CoreQuery::Ready(tx))
                .await
                .context("bridge core stopped")?;
            let ready = ready_rx.await.context("bridge core dropped ready reply")?;
            send_event(&mut write, &ready).await?;

            let mut scratch = [0u8; 64];
            loop {
                tokio::select! {
                    ev = rx.recv() => match ev {
                        Ok(event) => send_event(&mut write, &event).await?,
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(skipped, "bridge subscriber lagged; continuing");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    res = reader.read(&mut scratch) => match res {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    },
                    _ = stop_rx.changed() => {
                        if *stop_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

async fn send_event(write: &mut tokio::net::unix::OwnedWriteHalf, ev: &BridgeEvent) -> Result<()> {
    let mut line = protocol::encode(ev);
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    write.flush().await?;
    Ok(())
}

/// Bind `$XDG_RUNTIME_DIR/hey-computer-bridge.sock` with mode `0600`, refusing
/// to displace a live bridge daemon or overwrite a non-socket file.
async fn bind_socket(path: &Path) -> Result<UnixListener> {
    if UnixStream::connect(path).await.is_ok() {
        bail!(
            "another hey-computer-bridge daemon is already listening on {}",
            path.display()
        );
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            std::fs::remove_file(path)
                .with_context(|| format!("removing stale socket {}", path.display()))?;
        }
        Ok(_) => bail!(
            "{} exists and is not a socket; refusing to replace it",
            path.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("checking {}", path.display())),
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(path);
        return Err(e).with_context(|| format!("setting mode 0600 on {}", path.display()));
    }
    Ok(listener)
}

fn effective_text(text: &str, media_placeholder: Option<&str>) -> String {
    let t = text.trim();
    match (t.is_empty(), media_placeholder) {
        (false, Some(media)) => format!("{media} {t}"),
        (false, None) => t.to_string(),
        (true, Some(media)) => media.to_string(),
        (true, None) => String::new(),
    }
}

fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

pub fn unix_timestamp_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_cache_lru_eviction_and_lookup_by_name_or_alias() {
        let mut cache = ContextCache::new(2, 3);
        cache.push_message(
            Platform::Whatsapp,
            "+4915100000001",
            &["1234567890@lid".to_string()],
            Some("Alice Test"),
            ContextMessage {
                author: ContextAuthor::Them,
                sender_name: "Alice Test".to_string(),
                timestamp_ms: 1000,
                text: "Dinner at 18:30?".to_string(),
            },
        );
        cache.push_message(
            Platform::Whatsapp,
            "+4915100000001",
            &[],
            None,
            ContextMessage {
                author: ContextAuthor::Me,
                sender_name: "Me".to_string(),
                timestamp_ms: 2000,
                text: "Sounds great".to_string(),
            },
        );
        cache.push_message(
            Platform::Signal,
            "+4915100000002",
            &["uuid:550e8400-e29b-41d4-a716-446655440000".to_string()],
            Some("Bob Test"),
            ContextMessage {
                author: ContextAuthor::Them,
                sender_name: "Bob Test".to_string(),
                timestamp_ms: 3000,
                text: "Hey there".to_string(),
            },
        );

        let (plat, cid, name, msgs) = cache
            .lookup(Some(Platform::Whatsapp), "alice test", None)
            .unwrap();
        assert_eq!(plat, Platform::Whatsapp);
        assert_eq!(cid, "+4915100000001");
        assert_eq!(name.as_deref(), Some("Alice Test"));
        assert_eq!(msgs.len(), 2);

        // Lookup by LID alias works too.
        assert!(
            cache
                .lookup(None, "1234567890@lid", Some(1))
                .unwrap()
                .3
                .len()
                == 1
        );

        // Adding a 3rd chat evicts the least-recently-touched chat (+4915100000001).
        cache.push_message(
            Platform::Signal,
            "+4915100000003",
            &[],
            Some("Cara Test"),
            ContextMessage {
                author: ContextAuthor::Them,
                sender_name: "Cara Test".to_string(),
                timestamp_ms: 4000,
                text: "Third chat".to_string(),
            },
        );
        assert_eq!(cache.len(), 2);
        assert!(
            cache
                .lookup(Some(Platform::Whatsapp), "+4915100000001", None)
                .is_none()
        );
        assert!(
            cache
                .lookup(Some(Platform::Signal), "Cara Test", None)
                .is_some()
        );
    }
}
