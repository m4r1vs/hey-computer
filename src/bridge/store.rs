//! Persistent private SQLite + FTS5 history store (`<state_dir>/history.db`) for
//! `hey-computer-bridge`.
//!
//! Enforces `0700` state directory and `0600` database permissions, maintains an
//! external-content FTS5 index over `messages`, and strictly separates
//! `chats.title` (contact display name for 1:1 DMs; group subject for groups)
//! from `messages.sender_name` (individual group speaker or `"Me"`).

use std::collections::HashSet;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

use crate::bridge::config::{ensure_private_dir, normalize_identifier};
pub use crate::bridge::protocol::{ChatSummary, Platform, SearchHit, StoredMessage};

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS chats (
    platform TEXT NOT NULL,
    chat_id TEXT NOT NULL,
    is_group INTEGER NOT NULL DEFAULT 0,
    is_self_chat INTEGER NOT NULL DEFAULT 0,
    title TEXT NOT NULL DEFAULT '',
    last_message_ms INTEGER NOT NULL DEFAULT 0,
    message_count INTEGER NOT NULL DEFAULT 0,
    oldest_msg_id TEXT,
    oldest_msg_ts_ms INTEGER,
    oldest_msg_from_me INTEGER,
    raw_jid TEXT,
    PRIMARY KEY (platform, chat_id)
);

CREATE TABLE IF NOT EXISTS chat_aliases (
    platform TEXT NOT NULL,
    alias TEXT NOT NULL,
    chat_id TEXT NOT NULL,
    PRIMARY KEY (platform, alias)
);

CREATE TABLE IF NOT EXISTS messages (
    rowid INTEGER PRIMARY KEY,
    platform TEXT NOT NULL,
    chat_id TEXT NOT NULL,
    msg_id TEXT NOT NULL,
    timestamp_ms INTEGER NOT NULL,
    from_me INTEGER NOT NULL DEFAULT 0,
    is_group INTEGER NOT NULL DEFAULT 0,
    sender_id TEXT NOT NULL DEFAULT '',
    sender_name TEXT NOT NULL DEFAULT '',
    text TEXT NOT NULL DEFAULT '',
    quoted_text TEXT,
    media_placeholder TEXT,
    UNIQUE(platform, chat_id, msg_id)
);

CREATE INDEX IF NOT EXISTS idx_messages_chat_ts
    ON messages(platform, chat_id, timestamp_ms DESC);
CREATE INDEX IF NOT EXISTS idx_chats_last_msg
    ON chats(last_message_ms DESC);
CREATE INDEX IF NOT EXISTS idx_chat_aliases_target
    ON chat_aliases(platform, chat_id);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    sender_name,
    text,
    quoted_text,
    content='messages',
    content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 2'
);

CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, sender_name, text, quoted_text)
    VALUES (NEW.rowid, NEW.sender_name, NEW.text, NEW.quoted_text);
END;

CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, sender_name, text, quoted_text)
    VALUES ('delete', OLD.rowid, OLD.sender_name, OLD.text, OLD.quoted_text);
END;

CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, sender_name, text, quoted_text)
    VALUES ('delete', OLD.rowid, OLD.sender_name, OLD.text, OLD.quoted_text);
    INSERT INTO messages_fts(rowid, sender_name, text, quoted_text)
    VALUES (NEW.rowid, NEW.sender_name, NEW.text, NEW.quoted_text);
END;

CREATE TABLE IF NOT EXISTS sync_state (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

/// Chat metadata update (`chats.title`, aliases, group/self flags, raw JID).
#[derive(Debug, Clone)]
pub struct ChatMetaRecord {
    pub platform: Platform,
    pub chat_id: String,
    pub aliases: Vec<String>,
    pub is_group: bool,
    pub is_self_chat: bool,
    pub title: Option<String>,
    pub raw_jid: Option<String>,
}

/// Normalized message input for batch persistence into `BridgeStore`.
#[derive(Debug, Clone)]
pub struct StoredMessageInput {
    pub platform: Platform,
    pub chat_id: String,
    pub chat_aliases: Vec<String>,
    pub raw_jid: Option<String>,
    pub msg_id: String,
    pub timestamp_ms: i64,
    pub from_me: bool,
    pub is_group: bool,
    pub is_self_chat: bool,
    /// Explicit chat title (group subject for groups; contact display name for 1:1 DMs).
    /// Group member `sender_name` must NEVER be passed here.
    pub chat_title: Option<String>,
    pub sender_id: String,
    pub sender_name: String,
    pub text: String,
    pub quoted_text: Option<String>,
    pub media_placeholder: Option<String>,
}

/// Cursor identifying the oldest stored message in a chat for on-demand paging
/// (`sock.fetchMessageHistory` on WhatsApp).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OldestCursor {
    pub chat_id: String,
    pub raw_jid: String,
    pub oldest_msg_id: String,
    pub oldest_msg_ts_ms: i64,
    pub oldest_msg_from_me: bool,
}

pub struct BridgeStore {
    conn: Connection,
}

impl BridgeStore {
    /// Create or open `<path>` in read-write WAL mode with `0700` parent dir and
    /// `0600` file permissions.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            ensure_private_dir(parent)?;
        }

        // Pre-create with mode 0600 before SQLite opens it so no world-readable window exists.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("creating bridge database {}", path.display()))?;
        let meta = file
            .metadata()
            .with_context(|| format!("stat {}", path.display()))?;
        if meta.permissions().mode() & 0o177 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("chmod 0600 {}", path.display()))?;
        }
        drop(file);

        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags)
            .with_context(|| format!("opening bridge SQLite store {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(10))
            .context("setting bridge SQLite busy_timeout")?;

        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )
        .context("configuring bridge SQLite PRAGMAs")?;

        conn.execute_batch(SCHEMA_SQL)
            .context("initializing bridge SQLite schema")?;

        for suffix in ["-wal", "-shm"] {
            let sidecar = path.with_file_name(format!(
                "{}{suffix}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
            if let Ok(m) = std::fs::metadata(&sidecar)
                && m.permissions().mode() & 0o177 != 0
            {
                let _ = std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o600));
            }
        }

        Ok(Self { conn })
    }

    /// Open an existing `history.db` strictly read-only (`SQLITE_OPEN_READ_ONLY`)
    /// for offline CLI queries (`chats`, `groups`, `messages`, `search`).
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags)
            .with_context(|| format!("opening bridge database read-only at {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(10))
            .context("setting read-only bridge SQLite busy_timeout")?;
        conn.execute_batch("PRAGMA query_only = ON;")
            .context("configuring read-only bridge SQLite PRAGMAs")?;
        Ok(Self { conn })
    }

    /// Return `(stored_chats, stored_messages)`.
    pub fn counts(&self) -> Result<(usize, usize)> {
        let chats: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM chats", [], |r| r.get(0))?;
        let messages: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
        Ok((chats.max(0) as usize, messages.max(0) as usize))
    }

    pub fn get_sync_state(&self, key: &str) -> Result<Option<String>> {
        let val = self
            .conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = ?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(val)
    }

    pub fn set_sync_state(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_state(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Upsert chat metadata (`chats.title`, aliases, `is_group`, `is_self_chat`, `raw_jid`).
    pub fn upsert_chat_meta(&mut self, meta: &ChatMetaRecord) -> Result<()> {
        if meta.chat_id.trim().is_empty() {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        upsert_chat_meta_tx(&tx, meta)?;
        tx.commit()?;
        Ok(())
    }

    /// Upsert multiple chat metadata records in one transaction.
    pub fn upsert_chat_meta_batch(&mut self, items: &[ChatMetaRecord]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for item in items {
            if !item.chat_id.trim().is_empty() {
                upsert_chat_meta_tx(&tx, item)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Upsert a batch of messages inside a single transaction, hydrating any
    /// previously empty rows, recomputing per-chat aggregates accurately, and
    /// optionally pruning oldest messages beyond `max_stored` (`0` = unlimited).
    pub fn upsert_messages_batch(
        &mut self,
        msgs: &[StoredMessageInput],
        max_stored: usize,
    ) -> Result<usize> {
        if msgs.is_empty() {
            return Ok(0);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut inserted_or_hydrated = 0usize;
        let mut touched_chats: HashSet<(Platform, String)> = HashSet::new();

        {
            let mut upsert_msg_stmt = tx.prepare_cached(
                "INSERT INTO messages (
                    platform, chat_id, msg_id, timestamp_ms, from_me, is_group,
                    sender_id, sender_name, text, quoted_text, media_placeholder
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(platform, chat_id, msg_id) DO UPDATE SET
                    text = CASE
                        WHEN messages.text = '' AND excluded.text != '' THEN excluded.text
                        ELSE messages.text
                    END,
                    quoted_text = COALESCE(messages.quoted_text, excluded.quoted_text),
                    media_placeholder = COALESCE(messages.media_placeholder, excluded.media_placeholder),
                    sender_name = CASE
                        WHEN messages.sender_name = '' AND excluded.sender_name != '' THEN excluded.sender_name
                        ELSE messages.sender_name
                    END
                 WHERE (messages.text = '' AND excluded.text != '')
                    OR (messages.quoted_text IS NULL AND excluded.quoted_text IS NOT NULL)
                    OR (messages.media_placeholder IS NULL AND excluded.media_placeholder IS NOT NULL)
                    OR (messages.sender_name = '' AND excluded.sender_name != '')",
            )?;

            for msg in msgs {
                let text = msg.text.trim();
                let media = msg
                    .media_placeholder
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                if (text.is_empty() && media.is_none())
                    || msg.msg_id.trim().is_empty()
                    || msg.chat_id.trim().is_empty()
                {
                    continue;
                }

                let canonical_chat = normalize_identifier(&msg.chat_id);

                // CRITICAL GROUP INVARIANT:
                // Never allow a group member's `sender_name` to overwrite `chats.title`!
                // For groups, ONLY an explicit `msg.chat_title` updates `chats.title`.
                let explicit_title: Option<String> = if msg.is_group {
                    msg.chat_title
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(ToString::to_string)
                } else if msg.is_self_chat {
                    msg.chat_title
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(ToString::to_string)
                        .or_else(|| Some("Note to Self".to_string()))
                } else {
                    msg.chat_title
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(ToString::to_string)
                        .or_else(|| {
                            if !msg.from_me {
                                let sn = msg.sender_name.trim();
                                (!sn.is_empty()).then(|| sn.to_string())
                            } else {
                                None
                            }
                        })
                };

                upsert_chat_meta_tx(
                    &tx,
                    &ChatMetaRecord {
                        platform: msg.platform,
                        chat_id: canonical_chat.clone(),
                        aliases: msg.chat_aliases.clone(),
                        is_group: msg.is_group,
                        is_self_chat: msg.is_self_chat,
                        title: explicit_title,
                        raw_jid: msg.raw_jid.clone(),
                    },
                )?;

                let sender_id = if msg.sender_id.trim().is_empty() {
                    String::new()
                } else {
                    normalize_identifier(&msg.sender_id)
                };
                let sender_name = if msg.from_me {
                    "Me".to_string()
                } else {
                    msg.sender_name.trim().to_string()
                };
                let quoted = msg
                    .quoted_text
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());

                let changed = upsert_msg_stmt.execute(params![
                    msg.platform.as_str(),
                    canonical_chat,
                    msg.msg_id.trim(),
                    msg.timestamp_ms,
                    i64::from(msg.from_me),
                    i64::from(msg.is_group),
                    sender_id,
                    sender_name,
                    text,
                    quoted,
                    media,
                ])?;
                inserted_or_hydrated += changed;
                touched_chats.insert((msg.platform, canonical_chat));
            }
        }

        if max_stored > 0 {
            let total: i64 = tx.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
            let excess = total - (max_stored as i64);
            if excess > 0 {
                let mut prune_select = tx.prepare(
                    "SELECT rowid, platform, chat_id
                     FROM messages
                     ORDER BY timestamp_ms ASC, rowid ASC
                     LIMIT ?1",
                )?;
                let doomed: Vec<(i64, String, String)> = prune_select
                    .query_map(params![excess], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                drop(prune_select);

                let mut del_stmt = tx.prepare("DELETE FROM messages WHERE rowid = ?1")?;
                for (rowid, plat_str, cid) in doomed {
                    del_stmt.execute(params![rowid])?;
                    if let Some(plat) = Platform::parse(&plat_str) {
                        touched_chats.insert((plat, cid));
                    }
                }
            }
        }

        for (plat, cid) in &touched_chats {
            recompute_chat_aggregates_tx(&tx, *plat, cid)?;
        }

        tx.commit()?;
        Ok(inserted_or_hydrated)
    }

    /// Resolve a chat query using 3-tier ranking:
    /// - Tier 3: exact canonical `chat_id`, `chat_aliases` hit, or `raw_jid` match
    /// - Tier 2: exact case-insensitive `title` match (full Unicode lowercase)
    /// - Tier 1: case-insensitive substring `title` match
    ///
    /// Ties within the same tier are broken by `last_message_ms DESC`.
    pub fn resolve_chat(
        &self,
        platform: Option<Platform>,
        query: &str,
    ) -> Result<Option<ChatSummary>> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        let norm = normalize_identifier(trimmed);
        let raw_lower = trimmed.to_lowercase();

        // Tier 3: exact canonical chat_id, alias, or raw_jid match.
        let tier3 = match platform {
            Some(plat) => self
                .conn
                .query_row(
                    "SELECT c.platform, c.chat_id, c.is_group, c.is_self_chat, c.title,
                            c.last_message_ms, c.message_count, c.raw_jid
                     FROM chats c
                     WHERE c.platform = ?1
                       AND (
                           c.chat_id = ?2
                           OR LOWER(COALESCE(c.raw_jid, '')) = ?3
                           OR EXISTS (
                               SELECT 1 FROM chat_aliases ca
                               WHERE ca.platform = c.platform
                                 AND ca.chat_id = c.chat_id
                                 AND ca.alias = ?2
                           )
                       )
                     ORDER BY c.last_message_ms DESC
                     LIMIT 1",
                    params![plat.as_str(), norm, raw_lower],
                    row_to_chat_summary,
                )
                .optional()?,
            None => self
                .conn
                .query_row(
                    "SELECT c.platform, c.chat_id, c.is_group, c.is_self_chat, c.title,
                            c.last_message_ms, c.message_count, c.raw_jid
                     FROM chats c
                     WHERE c.chat_id = ?1
                        OR LOWER(COALESCE(c.raw_jid, '')) = ?2
                        OR EXISTS (
                            SELECT 1 FROM chat_aliases ca
                            WHERE ca.platform = c.platform
                              AND ca.chat_id = c.chat_id
                              AND ca.alias = ?1
                        )
                     ORDER BY c.last_message_ms DESC
                     LIMIT 1",
                    params![norm, raw_lower],
                    row_to_chat_summary,
                )
                .optional()?,
        };
        if tier3.is_some() {
            return Ok(tier3);
        }

        // Tier 2 (exact case-insensitive title) & Tier 1 (substring title).
        let candidates = self.list_chats(platform, true, true, 10_000)?;
        let mut best: Option<(u8, i64, ChatSummary)> = None;
        for chat in candidates {
            if chat.title.is_empty() {
                continue;
            }
            let title_lower = chat.title.to_lowercase();
            let tier = if title_lower == raw_lower {
                2u8
            } else if title_lower.contains(&raw_lower) {
                1u8
            } else {
                0u8
            };
            if tier == 0 {
                continue;
            }
            let replace = match &best {
                None => true,
                Some((b_tier, b_ts, _)) => {
                    tier > *b_tier || (tier == *b_tier && chat.last_message_ms > *b_ts)
                }
            };
            if replace {
                best = Some((tier, chat.last_message_ms, chat));
            }
        }
        Ok(best.map(|(_, _, c)| c))
    }

    /// List indexed chats ordered by `last_message_ms DESC`.
    pub fn list_chats(
        &self,
        platform: Option<Platform>,
        include_groups: bool,
        include_dms: bool,
        limit: usize,
    ) -> Result<Vec<ChatSummary>> {
        if (!include_groups && !include_dms) || limit == 0 {
            return Ok(Vec::new());
        }
        let plat_param = platform.map(Platform::as_str);
        let lim = limit.min(10_000) as i64;
        let mut stmt = self.conn.prepare(
            "SELECT platform, chat_id, is_group, is_self_chat, title,
                    last_message_ms, message_count, raw_jid
             FROM chats
             WHERE (?1 IS NULL OR platform = ?1)
               AND ((?2 != 0 AND is_group != 0) OR (?3 != 0 AND is_group = 0))
             ORDER BY last_message_ms DESC, message_count DESC, chat_id ASC
             LIMIT ?4",
        )?;
        let rows = stmt
            .query_map(
                params![
                    plat_param,
                    i64::from(include_groups),
                    i64::from(include_dms),
                    lim
                ],
                row_to_chat_summary,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Fetch messages for a resolved chat in chronological oldest-to-newest order
    /// for the selected `(after_ms, before_ms)` window.
    pub fn get_messages(
        &self,
        platform: Option<Platform>,
        chat_query: &str,
        before_ms: Option<i64>,
        after_ms: Option<i64>,
        limit: usize,
    ) -> Result<(Option<ChatSummary>, Vec<StoredMessage>)> {
        let Some(chat) = self.resolve_chat(platform, chat_query)? else {
            return Ok((None, Vec::new()));
        };
        if limit == 0 {
            return Ok((Some(chat), Vec::new()));
        }
        let lim = limit.min(5_000) as i64;
        let mut stmt = self.conn.prepare(
            "SELECT platform, chat_id, msg_id, timestamp_ms, from_me, is_group,
                    sender_id, sender_name, text, quoted_text, media_placeholder
             FROM messages
             WHERE platform = ?1
               AND chat_id = ?2
               AND (?3 IS NULL OR timestamp_ms < ?3)
               AND (?4 IS NULL OR timestamp_ms >= ?4)
             ORDER BY timestamp_ms DESC, rowid DESC
             LIMIT ?5",
        )?;
        let mut messages = stmt
            .query_map(
                params![
                    chat.platform.as_str(),
                    chat.chat_id,
                    before_ms,
                    after_ms,
                    lim
                ],
                row_to_stored_message,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        messages.reverse();
        Ok((Some(chat), messages))
    }

    /// Search messages across all or filtered chats using sanitized FTS5 `MATCH`,
    /// falling back to `LIKE` when the query contains only punctuation or no FTS
    /// hits on raw identifier substrings.
    pub fn search_messages(
        &self,
        query: &str,
        platform: Option<Platform>,
        chat_query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let trimmed = query.trim();
        if trimmed.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let resolved_filter = match chat_query.map(str::trim).filter(|s| !s.is_empty()) {
            Some(cq) => match self.resolve_chat(platform, cq)? {
                Some(c) => Some((c.platform, c.chat_id)),
                None => return Ok(Vec::new()),
            },
            None => None,
        };
        let eff_plat = resolved_filter
            .as_ref()
            .map(|(p, _)| p.as_str())
            .or_else(|| platform.map(Platform::as_str));
        let eff_chat = resolved_filter.as_ref().map(|(_, cid)| cid.as_str());
        let lim = limit.min(1_000) as i64;

        if let Some(fts_expr) = sanitize_fts5_query(trimmed) {
            let mut stmt = self.conn.prepare(
                "SELECT m.platform, m.chat_id, COALESCE(c.title, ''), m.is_group,
                        m.msg_id, m.timestamp_ms, m.from_me, m.sender_id,
                        m.sender_name, m.text, m.quoted_text, m.media_placeholder
                 FROM messages_fts
                 JOIN messages m ON m.rowid = messages_fts.rowid
                 LEFT JOIN chats c ON c.platform = m.platform AND c.chat_id = m.chat_id
                 WHERE messages_fts MATCH ?1
                   AND (?2 IS NULL OR m.platform = ?2)
                   AND (?3 IS NULL OR m.chat_id = ?3)
                 ORDER BY bm25(messages_fts, 1.0, 2.0, 0.5), m.timestamp_ms DESC
                 LIMIT ?4",
            )?;
            let hits = stmt
                .query_map(
                    params![fts_expr, eff_plat, eff_chat, lim],
                    row_to_search_hit,
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if !hits.is_empty() {
                return Ok(hits);
            }
        }

        // Fallback to case-insensitive LIKE (covers punctuation-only queries and sender_id/chat_id substrings).
        let escaped = escape_like(&trimmed.to_lowercase());
        let like_pat = format!("%{escaped}%");
        let mut stmt = self.conn.prepare(
            "SELECT m.platform, m.chat_id, COALESCE(c.title, ''), m.is_group,
                    m.msg_id, m.timestamp_ms, m.from_me, m.sender_id,
                    m.sender_name, m.text, m.quoted_text, m.media_placeholder
             FROM messages m
             LEFT JOIN chats c ON c.platform = m.platform AND c.chat_id = m.chat_id
             WHERE (?1 IS NULL OR m.platform = ?1)
               AND (?2 IS NULL OR m.chat_id = ?2)
               AND (
                   LOWER(m.text) LIKE ?3 ESCAPE '\\'
                   OR LOWER(m.sender_name) LIKE ?3 ESCAPE '\\'
                   OR LOWER(COALESCE(m.quoted_text, '')) LIKE ?3 ESCAPE '\\'
                   OR LOWER(m.sender_id) LIKE ?3 ESCAPE '\\'
               )
             ORDER BY m.timestamp_ms DESC
             LIMIT ?4",
        )?;
        let hits = stmt
            .query_map(
                params![eff_plat, eff_chat, like_pat, lim],
                row_to_search_hit,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(hits)
    }

    /// Return the oldest message cursor for `fetchMessageHistory` on `platform`.
    pub fn oldest_cursor(
        &self,
        platform: Platform,
        chat_query: &str,
    ) -> Result<Option<OldestCursor>> {
        let Some(chat) = self.resolve_chat(Some(platform), chat_query)? else {
            return Ok(None);
        };
        type CursorRow = (Option<String>, Option<String>, Option<i64>, Option<i64>);
        let row: Option<CursorRow> = self
            .conn
            .query_row(
                "SELECT raw_jid, oldest_msg_id, oldest_msg_ts_ms, oldest_msg_from_me
                 FROM chats
                 WHERE platform = ?1 AND chat_id = ?2",
                params![platform.as_str(), chat.chat_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;

        let (raw_jid_col, oldest_id_col, oldest_ts_col, oldest_from_me_col) =
            row.unwrap_or_default();
        let raw_jid = raw_jid_col
            .filter(|s| !s.trim().is_empty())
            .or_else(|| derive_whatsapp_jid(platform, &chat.chat_id));
        let Some(raw_jid) = raw_jid else {
            return Ok(None);
        };

        if let (Some(msg_id), Some(ts_ms)) = (oldest_id_col, oldest_ts_col) {
            return Ok(Some(OldestCursor {
                chat_id: chat.chat_id,
                raw_jid,
                oldest_msg_id: msg_id,
                oldest_msg_ts_ms: ts_ms,
                oldest_msg_from_me: oldest_from_me_col.unwrap_or(0) != 0,
            }));
        }

        let fallback: Option<(String, i64, i64)> = self
            .conn
            .query_row(
                "SELECT msg_id, timestamp_ms, from_me
                 FROM messages
                 WHERE platform = ?1 AND chat_id = ?2
                 ORDER BY timestamp_ms ASC, rowid ASC
                 LIMIT 1",
                params![platform.as_str(), chat.chat_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;

        Ok(fallback.map(|(msg_id, ts_ms, fm)| OldestCursor {
            chat_id: chat.chat_id,
            raw_jid,
            oldest_msg_id: msg_id,
            oldest_msg_ts_ms: ts_ms,
            oldest_msg_from_me: fm != 0,
        }))
    }
}

fn upsert_chat_meta_tx(tx: &Transaction<'_>, meta: &ChatMetaRecord) -> Result<()> {
    let canonical = normalize_identifier(&meta.chat_id);
    if canonical.is_empty() {
        return Ok(());
    }
    let clean_title = meta
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    let clean_raw_jid = meta
        .raw_jid
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    tx.execute(
        "INSERT INTO chats (platform, chat_id, is_group, is_self_chat, title, raw_jid)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(platform, chat_id) DO UPDATE SET
            is_group = CASE WHEN excluded.is_group != 0 THEN 1 ELSE chats.is_group END,
            is_self_chat = CASE WHEN excluded.is_self_chat != 0 THEN 1 ELSE chats.is_self_chat END,
            title = CASE WHEN excluded.title != '' THEN excluded.title ELSE chats.title END,
            raw_jid = COALESCE(excluded.raw_jid, chats.raw_jid)",
        params![
            meta.platform.as_str(),
            canonical,
            i64::from(meta.is_group),
            i64::from(meta.is_self_chat),
            clean_title,
            clean_raw_jid,
        ],
    )?;

    let mut alias_stmt = tx.prepare_cached(
        "INSERT INTO chat_aliases (platform, alias, chat_id)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(platform, alias) DO UPDATE SET chat_id = excluded.chat_id",
    )?;
    alias_stmt.execute(params![meta.platform.as_str(), canonical, canonical])?;
    for alias in &meta.aliases {
        let trimmed = alias.trim();
        if !trimmed.is_empty() {
            let norm_alias = normalize_identifier(trimmed);
            if !norm_alias.is_empty() {
                alias_stmt.execute(params![meta.platform.as_str(), norm_alias, canonical])?;
            }
        }
    }
    if let Some(jid) = clean_raw_jid {
        let norm_jid = normalize_identifier(jid);
        if !norm_jid.is_empty() {
            alias_stmt.execute(params![meta.platform.as_str(), norm_jid, canonical])?;
        }
    }
    Ok(())
}

fn recompute_chat_aggregates_tx(
    tx: &Transaction<'_>,
    platform: Platform,
    chat_id: &str,
) -> Result<()> {
    let plat_s = platform.as_str();
    let (count, max_ts): (i64, i64) = tx.query_row(
        "SELECT COUNT(*), COALESCE(MAX(timestamp_ms), 0)
         FROM messages
         WHERE platform = ?1 AND chat_id = ?2",
        params![plat_s, chat_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let oldest: Option<(String, i64, i64)> = tx
        .query_row(
            "SELECT msg_id, timestamp_ms, from_me
             FROM messages
             WHERE platform = ?1 AND chat_id = ?2
             ORDER BY timestamp_ms ASC, rowid ASC
             LIMIT 1",
            params![plat_s, chat_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let (oldest_id, oldest_ts, oldest_fm) = match oldest {
        Some((id, ts, fm)) => (Some(id), Some(ts), Some(fm)),
        None => (None, None, None),
    };
    tx.execute(
        "UPDATE chats
         SET message_count = ?3,
             last_message_ms = CASE WHEN ?4 > 0 THEN ?4 ELSE last_message_ms END,
             oldest_msg_id = ?5,
             oldest_msg_ts_ms = ?6,
             oldest_msg_from_me = ?7
         WHERE platform = ?1 AND chat_id = ?2",
        params![
            plat_s, chat_id, count, max_ts, oldest_id, oldest_ts, oldest_fm
        ],
    )?;
    Ok(())
}

fn derive_whatsapp_jid(platform: Platform, chat_id: &str) -> Option<String> {
    if platform != Platform::Whatsapp {
        return None;
    }
    if let Some(rest) = chat_id.strip_prefix("group:")
        && rest.ends_with("@g.us")
    {
        return Some(rest.to_string());
    }
    if let Some(digits) = chat_id.strip_prefix('+')
        && digits.bytes().all(|b| b.is_ascii_digit())
    {
        return Some(format!("{digits}@s.whatsapp.net"));
    }
    if chat_id.contains('@') {
        return Some(chat_id.to_string());
    }
    None
}

fn row_to_chat_summary(r: &rusqlite::Row<'_>) -> rusqlite::Result<ChatSummary> {
    let plat_s: String = r.get(0)?;
    let platform = Platform::parse(&plat_s).unwrap_or(Platform::Whatsapp);
    Ok(ChatSummary {
        platform,
        chat_id: r.get(1)?,
        is_group: r.get::<_, i64>(2)? != 0,
        is_self_chat: r.get::<_, i64>(3)? != 0,
        title: r.get(4)?,
        last_message_ms: r.get(5)?,
        message_count: r.get::<_, i64>(6)?.max(0) as usize,
        raw_jid: r.get(7)?,
    })
}

fn row_to_stored_message(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMessage> {
    let plat_s: String = r.get(0)?;
    let platform = Platform::parse(&plat_s).unwrap_or(Platform::Whatsapp);
    Ok(StoredMessage {
        platform,
        chat_id: r.get(1)?,
        msg_id: r.get(2)?,
        timestamp_ms: r.get(3)?,
        from_me: r.get::<_, i64>(4)? != 0,
        is_group: r.get::<_, i64>(5)? != 0,
        sender_id: r.get(6)?,
        sender_name: r.get(7)?,
        text: r.get(8)?,
        quoted_text: r.get(9)?,
        media_placeholder: r.get(10)?,
    })
}

fn row_to_search_hit(r: &rusqlite::Row<'_>) -> rusqlite::Result<SearchHit> {
    let plat_s: String = r.get(0)?;
    let platform = Platform::parse(&plat_s).unwrap_or(Platform::Whatsapp);
    Ok(SearchHit {
        platform,
        chat_id: r.get(1)?,
        chat_title: r.get(2)?,
        is_group: r.get::<_, i64>(3)? != 0,
        msg_id: r.get(4)?,
        timestamp_ms: r.get(5)?,
        from_me: r.get::<_, i64>(6)? != 0,
        sender_id: r.get(7)?,
        sender_name: r.get(8)?,
        text: r.get(9)?,
        quoted_text: r.get(10)?,
        media_placeholder: r.get(11)?,
    })
}

/// Sanitize arbitrary user text into safe FTS5 quoted tokens so punctuation
/// (`+49...`, `foo:bar`, `"`, `*`, `-`, `AND`, `OR`, `NEAR`) never causes an
/// FTS5 syntax error. Returns `None` when no alphanumeric tokens exist.
pub fn sanitize_fts5_query(input: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        if chars[i] == '"' {
            if let Some(rel_end) = chars[i + 1..].iter().position(|&c| c == '"') {
                let end = i + 1 + rel_end;
                let phrase: String = chars[i + 1..end].iter().collect();
                let trimmed = phrase.trim();
                if trimmed.chars().any(char::is_alphanumeric) {
                    terms.push(format!("\"{}\"", trimmed.replace('"', "\"\"")));
                }
                i = end + 1;
                continue;
            }
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '"' {
            i += 1;
        }
        let raw: String = chars[start..i].iter().collect();
        let (prefix, core) = match raw.strip_suffix('*') {
            Some(rest) if rest.chars().any(char::is_alphanumeric) => (true, rest),
            _ => (false, raw.as_str()),
        };
        // Strip leading/trailing non-alphanumeric operators while keeping inner characters intact.
        let cleaned = core.trim_matches(|c: char| !c.is_alphanumeric());
        if cleaned.is_empty() {
            continue;
        }
        let escaped = cleaned.replace('"', "\"\"");
        if prefix {
            terms.push(format!("\"{escaped}\"*"));
        } else {
            terms.push(format!("\"{escaped}\""));
        }
    }

    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("hc-bstore-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn group_title_never_overwritten_by_member_sender_name_and_fts_stays_valid() {
        let dir = temp_store_dir("group-inv");
        let db_path = dir.join("history.db");
        let mut store = BridgeStore::open(&db_path).unwrap();

        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let db_mode = std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(db_mode, 0o600);

        store
            .upsert_chat_meta(&ChatMetaRecord {
                platform: Platform::Whatsapp,
                chat_id: "120363001234567890@g.us".to_string(),
                aliases: vec!["120363001234567890@g.us".to_string()],
                is_group: true,
                is_self_chat: false,
                title: Some("Flatmates Group".to_string()),
                raw_jid: Some("120363001234567890@g.us".to_string()),
            })
            .unwrap();

        let inserted = store
            .upsert_messages_batch(
                &[
                    StoredMessageInput {
                        platform: Platform::Whatsapp,
                        chat_id: "120363001234567890@g.us".to_string(),
                        chat_aliases: vec![],
                        raw_jid: Some("120363001234567890@g.us".to_string()),
                        msg_id: "m-2".to_string(),
                        timestamp_ms: 2000,
                        from_me: false,
                        is_group: true,
                        is_self_chat: false,
                        // Group message without explicit chat_title must NEVER overwrite "Flatmates Group" with "Bob Test"!
                        chat_title: None,
                        sender_id: "+4915100000002".to_string(),
                        sender_name: "Bob Test".to_string(),
                        text: "Who bought coffee beans?".to_string(),
                        quoted_text: None,
                        media_placeholder: None,
                    },
                    StoredMessageInput {
                        platform: Platform::Whatsapp,
                        chat_id: "120363001234567890@g.us".to_string(),
                        chat_aliases: vec![],
                        raw_jid: Some("120363001234567890@g.us".to_string()),
                        msg_id: "m-1".to_string(),
                        timestamp_ms: 1000,
                        from_me: true,
                        is_group: true,
                        is_self_chat: false,
                        chat_title: None,
                        sender_id: "+4915100000000".to_string(),
                        sender_name: "Ignored Self PushName".to_string(),
                        text: "We are out of milk".to_string(),
                        quoted_text: Some("Grocery list".to_string()),
                        media_placeholder: None,
                    },
                ],
                0,
            )
            .unwrap();
        assert_eq!(inserted, 2);

        // Duplicate batch should report 0 newly inserted rows.
        let dup = store
            .upsert_messages_batch(
                &[StoredMessageInput {
                    platform: Platform::Whatsapp,
                    chat_id: "120363001234567890@g.us".to_string(),
                    chat_aliases: vec![],
                    raw_jid: None,
                    msg_id: "m-2".to_string(),
                    timestamp_ms: 2000,
                    from_me: false,
                    is_group: true,
                    is_self_chat: false,
                    chat_title: None,
                    sender_id: "+4915100000002".to_string(),
                    sender_name: "Bob Test".to_string(),
                    text: "Who bought coffee beans?".to_string(),
                    quoted_text: None,
                    media_placeholder: None,
                }],
                0,
            )
            .unwrap();
        assert_eq!(dup, 0);

        // Verify read-only offline store access, chronological order, and group title preservation.
        let ro = BridgeStore::open_read_only(&db_path).unwrap();
        let (chat, msgs) = ro.get_messages(None, "flatmates", None, None, 10).unwrap();
        let chat = chat.unwrap();
        assert_eq!(chat.title, "Flatmates Group");
        assert!(chat.is_group);
        assert_eq!(chat.message_count, 2);
        assert_eq!(chat.last_message_ms, 2000);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].msg_id, "m-1");
        assert_eq!(msgs[0].sender_name, "Me");
        assert_eq!(msgs[1].msg_id, "m-2");
        assert_eq!(msgs[1].sender_name, "Bob Test");

        // Oldest cursor tracks m-1 (timestamp 1000, from_me = true).
        let cursor = ro
            .oldest_cursor(Platform::Whatsapp, "Flatmates Group")
            .unwrap()
            .unwrap();
        assert_eq!(cursor.raw_jid, "120363001234567890@g.us");
        assert_eq!(cursor.oldest_msg_id, "m-1");
        assert_eq!(cursor.oldest_msg_ts_ms, 1000);
        assert!(cursor.oldest_msg_from_me);

        // Adversarial / punctuation FTS5 queries never error.
        for tricky in [
            "coffee",
            "\"coffee beans\"",
            "+4915100000002",
            "foo:bar -baz * \"unclosed",
            "--- *** ()",
        ] {
            let _ = ro.search_messages(tricky, None, None, 10).unwrap();
        }
        let hits = ro.search_messages("coffee*", None, None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chat_title, "Flatmates Group");
        assert_eq!(hits[0].sender_name, "Bob Test");

        // Verify SQLite FTS5 external-content integrity check after inserts, updates, and pruning.
        store
            .upsert_messages_batch(
                &[StoredMessageInput {
                    platform: Platform::Signal,
                    chat_id: "+4915100000001".to_string(),
                    chat_aliases: vec!["uuid:22222222-2222-2222-2222-222222222222".to_string()],
                    raw_jid: None,
                    msg_id: "s-1".to_string(),
                    timestamp_ms: 3000,
                    from_me: false,
                    is_group: false,
                    is_self_chat: false,
                    chat_title: Some("Alice Test".to_string()),
                    sender_id: "+4915100000001".to_string(),
                    sender_name: "Alice Test".to_string(),
                    text: "See you soon".to_string(),
                    quoted_text: None,
                    media_placeholder: None,
                }],
                2, // Prune oldest message (m-1) so only 2 rows remain.
            )
            .unwrap();
        assert_eq!(store.counts().unwrap(), (2, 2));
        store
            .conn
            .execute(
                "INSERT INTO messages_fts(messages_fts) VALUES('integrity-check')",
                [],
            )
            .unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }
}
