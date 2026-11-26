//! `hey-computer-bridge`: strictly read-only WhatsApp & Signal messenger bridge.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;
use tracing_subscriber::EnvFilter;

use hey_computer::bridge;
use hey_computer::bridge::config::{BridgeConfig, default_socket_path};
use hey_computer::bridge::protocol::{
    BridgeEvent, BridgeRequest, ChatSummary, Platform, SearchHit, StoredMessage,
};
use hey_computer::bridge::signal_desktop;
use hey_computer::bridge::store::BridgeStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CliPlatform {
    Whatsapp,
    Signal,
}

impl From<CliPlatform> for Platform {
    fn from(value: CliPlatform) -> Self {
        match value {
            CliPlatform::Whatsapp => Self::Whatsapp,
            CliPlatform::Signal => Self::Signal,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "hey-computer-bridge",
    about = "Strictly read-only WhatsApp and Signal bridge for hey-computer"
)]
struct Cli {
    /// Path to bridge.toml (default: ~/.config/hey-computer/bridge.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Path to the bridge UNIX socket (default: $XDG_RUNTIME_DIR/hey-computer-bridge.sock).
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the read-only messenger bridge supervisor daemon.
    Daemon,
    /// Print bridge status from the running daemon.
    Status {
        /// Output machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Interactively link a read-only WhatsApp or Signal device.
    Pair {
        #[command(subcommand)]
        platform: PairPlatform,
    },
    /// List indexed WhatsApp and Signal group chats (merges Signal Desktop directly when needed).
    Groups {
        /// Filter by platform (`whatsapp` or `signal`).
        #[arg(long)]
        platform: Option<CliPlatform>,
        /// Explicit Signal Desktop directory (default: auto-detect `~/.config/Signal`).
        #[arg(long)]
        signal_desktop: Option<PathBuf>,
        /// Maximum groups to return.
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Output machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// List indexed 1:1 and group chats ordered by most recent message.
    Chats {
        /// Filter by platform (`whatsapp` or `signal`).
        #[arg(long)]
        platform: Option<CliPlatform>,
        /// Show group chats only.
        #[arg(long, conflicts_with = "dms_only")]
        groups_only: bool,
        /// Show 1:1 direct/self chats only.
        #[arg(long, conflicts_with = "groups_only")]
        dms_only: bool,
        /// Maximum chats to return.
        #[arg(long, default_value_t = 30)]
        limit: usize,
        /// Output machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Read chronological messages from a contact or group chat.
    Messages {
        /// Contact name, group title, E.164 phone number, or chat ID.
        chat: String,
        /// Filter by platform (`whatsapp` or `signal`).
        #[arg(long)]
        platform: Option<CliPlatform>,
        /// Maximum messages to return.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Upper bound timestamp (`epoch_ms`, `epoch_s`, `YYYY-MM-DD`, or `YYYY-MM-DDTHH:MM`).
        #[arg(long)]
        before: Option<String>,
        /// Lower bound timestamp (`epoch_ms`, `epoch_s`, `YYYY-MM-DD`, or `YYYY-MM-DDTHH:MM`).
        #[arg(long)]
        after: Option<String>,
        /// Output machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Full-text search across indexed WhatsApp and Signal messages.
    Search {
        /// Search keywords or phrase.
        query: String,
        /// Filter by platform (`whatsapp` or `signal`).
        #[arg(long)]
        platform: Option<CliPlatform>,
        /// Restrict search to one contact or group chat.
        #[arg(long)]
        chat: Option<String>,
        /// Maximum search hits to return.
        #[arg(long, default_value_t = 30)]
        limit: usize,
        /// Output machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Request older WhatsApp message history on demand from the linked phone.
    FetchHistory {
        /// WhatsApp contact name, group title, E.164 phone number, or JID.
        chat: String,
        /// Number of older messages to request (`1..=200`).
        #[arg(long, default_value_t = 50)]
        count: usize,
    },
    /// Run a read-only incremental Signal Desktop SQLCipher history import now.
    SyncSignalDesktop {
        /// Output machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum PairPlatform {
    /// Link a read-only WhatsApp Web session via terminal QR code.
    Whatsapp,
    /// Link a read-only Signal secondary device via terminal QR code.
    Signal {
        /// Device label shown in Signal -> Linked Devices.
        #[arg(long, default_value = "hey-computer")]
        name: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("hey_computer=info,hey_computer_bridge=info,warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Daemon => {
            let config = BridgeConfig::load(cli.config.as_deref())?;
            let socket = default_socket_path(cli.socket.as_deref())?;
            bridge::run_daemon(config, &socket).await
        }
        Command::Status { json } => {
            let socket = default_socket_path(cli.socket.as_deref())?;
            let reply = bridge::query(&socket, &BridgeRequest::Status).await?;
            if json {
                println!("{}", serde_json::to_string(&reply)?);
            } else {
                match reply {
                    BridgeEvent::Status {
                        read_only,
                        whatsapp,
                        signal,
                        whatsapp_detail,
                        signal_detail,
                        tracked_chats,
                        stored_chats,
                        stored_messages,
                        whatsapp_sync_progress,
                        ..
                    } => {
                        println!("read_only: {read_only}");
                        match whatsapp_detail {
                            Some(d) => println!("whatsapp: {whatsapp} ({d})"),
                            None => println!("whatsapp: {whatsapp}"),
                        }
                        if let Some(pct) = whatsapp_sync_progress {
                            println!("whatsapp_sync_progress: {pct}%");
                        }
                        match signal_detail {
                            Some(d) => println!("signal: {signal} ({d})"),
                            None => println!("signal: {signal}"),
                        }
                        println!("tracked_chats: {tracked_chats}");
                        println!("stored_chats: {stored_chats}");
                        println!("stored_messages: {stored_messages}");
                    }
                    other => println!("{}", serde_json::to_string(&other)?),
                }
            }
            Ok(())
        }
        Command::Pair { platform } => {
            let config = BridgeConfig::load(cli.config.as_deref())?;
            match platform {
                PairPlatform::Whatsapp => bridge::whatsapp::pair_whatsapp(&config).await,
                PairPlatform::Signal { name } => bridge::signal::pair_signal(&config, &name).await,
            }
        }
        Command::Groups {
            platform,
            signal_desktop: explicit_desktop,
            limit,
            json,
        } => {
            let config = BridgeConfig::load(cli.config.as_deref())?;
            let plat_filter = platform.map(Platform::from);
            let mut groups = query_chats_with_offline_fallback(
                &config,
                cli.socket.as_deref(),
                plat_filter,
                true,
                false,
                limit,
            )
            .await?;

            // Pre-rebuild convenience: if Signal groups are requested and either
            // `--signal-desktop` was passed explicitly or `history.db` has no
            // Signal groups yet, inspect Signal Desktop's SQLCipher DB directly.
            let include_signal = plat_filter.is_none() || plat_filter == Some(Platform::Signal);
            let has_signal_groups = groups
                .iter()
                .any(|c| c.platform == Platform::Signal && c.is_group);
            if include_signal && (explicit_desktop.is_some() || !has_signal_groups) {
                let desktop_dir = explicit_desktop
                    .clone()
                    .or_else(|| config.resolved_signal_desktop_dir());
                if let Some(dir) = desktop_dir {
                    match signal_desktop::list_signal_desktop_groups(
                        &dir,
                        &config.signal.sqlcipher_command,
                    ) {
                        Ok(desktop_groups) => {
                            for dg in desktop_groups {
                                if !groups.iter().any(|existing| {
                                    existing.platform == dg.platform
                                        && existing.chat_id == dg.chat_id
                                }) {
                                    groups.push(dg);
                                }
                            }
                            groups.sort_by(|a, b| {
                                b.last_message_ms
                                    .cmp(&a.last_message_ms)
                                    .then_with(|| a.title.cmp(&b.title))
                            });
                            groups.truncate(limit);
                        }
                        Err(e) if explicit_desktop.is_some() => return Err(e),
                        Err(e) => {
                            tracing::debug!(
                                "skipping opportunistic Signal Desktop group scan: {e:#}"
                            );
                        }
                    }
                }
            }

            print_chats(&groups, json)?;
            Ok(())
        }
        Command::Chats {
            platform,
            groups_only,
            dms_only,
            limit,
            json,
        } => {
            let config = BridgeConfig::load(cli.config.as_deref())?;
            let chats = query_chats_with_offline_fallback(
                &config,
                cli.socket.as_deref(),
                platform.map(Platform::from),
                !dms_only,
                !groups_only,
                limit,
            )
            .await?;
            print_chats(&chats, json)?;
            Ok(())
        }
        Command::Messages {
            chat,
            platform,
            limit,
            before,
            after,
            json,
        } => {
            let before_ms = before
                .as_deref()
                .map(parse_timestamp_arg)
                .transpose()
                .context("invalid --before timestamp")?;
            let after_ms = after
                .as_deref()
                .map(parse_timestamp_arg)
                .transpose()
                .context("invalid --after timestamp")?;
            let config = BridgeConfig::load(cli.config.as_deref())?;
            let plat = platform.map(Platform::from);

            let (chat_summary, messages) = query_messages_with_offline_fallback(
                &config,
                cli.socket.as_deref(),
                plat,
                &chat,
                before_ms,
                after_ms,
                limit,
            )
            .await?;

            if json {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "chat": chat_summary,
                        "messages": messages,
                    }))?
                );
            } else {
                print_messages(chat_summary.as_ref(), &chat, &messages);
            }
            Ok(())
        }
        Command::Search {
            query,
            platform,
            chat,
            limit,
            json,
        } => {
            let config = BridgeConfig::load(cli.config.as_deref())?;
            let hits = query_search_with_offline_fallback(
                &config,
                cli.socket.as_deref(),
                &query,
                platform.map(Platform::from),
                chat.as_deref(),
                limit,
            )
            .await?;
            if json {
                println!("{}", serde_json::to_string(&hits)?);
            } else {
                print_search_hits(&hits);
            }
            Ok(())
        }
        Command::FetchHistory { chat, count } => {
            let socket = default_socket_path(cli.socket.as_deref())?;
            let reply = bridge::query(
                &socket,
                &BridgeRequest::FetchHistory {
                    platform: Platform::Whatsapp,
                    chat,
                    count: Some(count),
                },
            )
            .await?;
            match reply {
                BridgeEvent::HistoryRequested { chat_id, count } => {
                    println!(
                        "requested {count} older WhatsApp messages for {chat_id} (syncing asynchronously)"
                    );
                    Ok(())
                }
                BridgeEvent::Error { message } => bail!("{message}"),
                other => {
                    println!("{}", serde_json::to_string(&other)?);
                    Ok(())
                }
            }
        }
        Command::SyncSignalDesktop { json } => {
            let config = BridgeConfig::load(cli.config.as_deref())?;
            let summary = if let Ok(socket) = default_socket_path(cli.socket.as_deref())
                && let Ok(reply) = bridge::query(&socket, &BridgeRequest::SyncSignalDesktop).await
            {
                match reply {
                    BridgeEvent::SyncCompleted {
                        conversations,
                        messages_imported,
                    } => Some((conversations, messages_imported)),
                    _ => None,
                }
            } else {
                None
            };

            let (conversations, messages_imported) = match summary {
                Some(pair) => pair,
                None => {
                    let db_path = config.resolved_state_dir()?.join("history.db");
                    let mut store = BridgeStore::open(&db_path)?;
                    let res = signal_desktop::sync_signal_desktop(&config, &mut store)?;
                    (res.conversations, res.messages_imported)
                }
            };

            if json {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "conversations": conversations,
                        "messages_imported": messages_imported,
                    }))?
                );
            } else {
                println!("conversations: {conversations}\nmessages_imported: {messages_imported}");
            }
            Ok(())
        }
    }
}

fn open_offline_store(config: &BridgeConfig) -> Result<Option<BridgeStore>> {
    let db_path = config.resolved_state_dir()?.join("history.db");
    if !db_path.is_file() {
        return Ok(None);
    }
    Ok(Some(BridgeStore::open_read_only(&db_path)?))
}

async fn query_chats_with_offline_fallback(
    config: &BridgeConfig,
    explicit_socket: Option<&Path>,
    platform: Option<Platform>,
    include_groups: bool,
    include_dms: bool,
    limit: usize,
) -> Result<Vec<ChatSummary>> {
    if let Ok(socket) = default_socket_path(explicit_socket)
        && let Ok(BridgeEvent::Chats { chats }) = bridge::query(
            &socket,
            &BridgeRequest::ListChats {
                platform,
                include_groups,
                include_dms,
                limit: Some(limit),
            },
        )
        .await
    {
        return Ok(chats);
    }

    let Some(store) = open_offline_store(config)? else {
        return Ok(Vec::new());
    };
    store.list_chats(platform, include_groups, include_dms, limit)
}

async fn query_messages_with_offline_fallback(
    config: &BridgeConfig,
    explicit_socket: Option<&Path>,
    platform: Option<Platform>,
    chat: &str,
    before_ms: Option<i64>,
    after_ms: Option<i64>,
    limit: usize,
) -> Result<(Option<ChatSummary>, Vec<StoredMessage>)> {
    if let Ok(socket) = default_socket_path(explicit_socket)
        && let Ok(BridgeEvent::Messages { chat, messages }) = bridge::query(
            &socket,
            &BridgeRequest::GetMessages {
                platform,
                chat: chat.to_string(),
                before_ms,
                after_ms,
                limit: Some(limit),
            },
        )
        .await
    {
        return Ok((chat, messages));
    }

    let Some(store) = open_offline_store(config)? else {
        return Ok((None, Vec::new()));
    };
    store.get_messages(platform, chat, before_ms, after_ms, limit)
}

async fn query_search_with_offline_fallback(
    config: &BridgeConfig,
    explicit_socket: Option<&Path>,
    query: &str,
    platform: Option<Platform>,
    chat: Option<&str>,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    if let Ok(socket) = default_socket_path(explicit_socket)
        && let Ok(BridgeEvent::SearchResults { hits }) = bridge::query(
            &socket,
            &BridgeRequest::Search {
                query: query.to_string(),
                platform,
                chat: chat.map(ToString::to_string),
                limit: Some(limit),
            },
        )
        .await
    {
        return Ok(hits);
    }

    let Some(store) = open_offline_store(config)? else {
        return Ok(Vec::new());
    };
    store.search_messages(query, platform, chat, limit)
}

fn print_chats(chats: &[ChatSummary], json_out: bool) -> Result<()> {
    if json_out {
        println!("{}", serde_json::to_string(chats)?);
        return Ok(());
    }
    if chats.is_empty() {
        println!("no chats found");
        return Ok(());
    }
    for c in chats {
        let kind = if c.is_group {
            "group"
        } else if c.is_self_chat {
            "self"
        } else {
            "dm"
        };
        let label = if c.title.is_empty() {
            c.chat_id.as_str()
        } else {
            c.title.as_str()
        };
        let last_ts = if c.last_message_ms > 0 {
            format_local_ts(c.last_message_ms)
        } else {
            "-".to_string()
        };
        println!(
            "[{}|{}] {} ({}) — {} msgs, last {}",
            c.platform, kind, label, c.chat_id, c.message_count, last_ts
        );
    }
    Ok(())
}

fn print_messages(chat: Option<&ChatSummary>, raw_query: &str, messages: &[StoredMessage]) {
    let Some(chat) = chat else {
        println!("no chat matching '{raw_query}'");
        return;
    };
    let kind = if chat.is_group {
        "group"
    } else if chat.is_self_chat {
        "self"
    } else {
        "dm"
    };
    let title = if chat.title.is_empty() {
        &chat.chat_id
    } else {
        &chat.title
    };
    println!(
        "# [{}|{}] {} ({})",
        chat.platform, kind, title, chat.chat_id
    );
    if messages.is_empty() {
        println!("(no messages in selected window)");
        return;
    }
    for m in messages {
        let ts = format_local_ts(m.timestamp_ms);
        let speaker = if m.from_me {
            "Me"
        } else if !m.sender_name.is_empty() {
            m.sender_name.as_str()
        } else {
            m.sender_id.as_str()
        };
        let body = compose_line_body(
            &m.text,
            m.quoted_text.as_deref(),
            m.media_placeholder.as_deref(),
        );
        println!("[{ts}] {speaker}: {body}");
    }
}

fn print_search_hits(hits: &[SearchHit]) {
    if hits.is_empty() {
        println!("no matching messages");
        return;
    }
    for h in hits {
        let ts = format_local_ts(h.timestamp_ms);
        let chat_label = if h.chat_title.is_empty() {
            h.chat_id.as_str()
        } else {
            h.chat_title.as_str()
        };
        let speaker = if h.from_me {
            "Me"
        } else if !h.sender_name.is_empty() {
            h.sender_name.as_str()
        } else {
            h.sender_id.as_str()
        };
        let body = compose_line_body(
            &h.text,
            h.quoted_text.as_deref(),
            h.media_placeholder.as_deref(),
        );
        println!(
            "[{} | {} | {}] {}: {}",
            h.platform, chat_label, ts, speaker, body
        );
    }
}

fn compose_line_body(
    text: &str,
    quoted_text: Option<&str>,
    media_placeholder: Option<&str>,
) -> String {
    let mut out = String::new();
    if let Some(q) = quoted_text.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str(&format!("(replying to: {}) ", single_line(q)));
    }
    if let Some(med) = media_placeholder.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str(med);
        if !text.trim().is_empty() {
            out.push(' ');
        }
    }
    out.push_str(&single_line(text.trim()));
    out
}

fn single_line(s: &str) -> String {
    s.lines().map(str::trim).collect::<Vec<_>>().join(" / ")
}

fn format_local_ts(epoch_ms: i64) -> String {
    let secs = (epoch_ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    if !ok {
        return epoch_ms.to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

/// Parse CLI `--before` / `--after` values as epoch milliseconds, epoch seconds,
/// `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM[:SS]`, or RFC3339 (`...Z` / `...±HH:MM`).
fn parse_timestamp_arg(raw: &str) -> Result<i64> {
    let s = raw.trim();
    if s.is_empty() {
        bail!("timestamp cannot be empty");
    }
    if let Ok(n) = s.parse::<i64>() {
        return if n.abs() >= 1_000_000_000_000 {
            Ok(n)
        } else {
            Ok(n.saturating_mul(1000))
        };
    }

    if s.len() < 10 || s.as_bytes()[4] != b'-' || s.as_bytes()[7] != b'-' {
        bail!("unsupported timestamp '{s}' (expected epoch ms/s, YYYY-MM-DD, or YYYY-MM-DDTHH:MM)");
    }
    let year: i32 = s[0..4].parse().context("invalid year")?;
    let month: u32 = s[5..7].parse().context("invalid month")?;
    let day: u32 = s[8..10].parse().context("invalid day")?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        bail!("date out of range in '{s}'");
    }

    let rest = s[10..].trim();
    if rest.is_empty() {
        return local_civil_to_epoch_ms(year, month, day, 0, 0, 0);
    }

    let time_part = rest
        .strip_prefix('T')
        .or_else(|| rest.strip_prefix('t'))
        .or_else(|| rest.strip_prefix(' '))
        .unwrap_or(rest);

    // Split optional timezone suffix (`Z`, `+HH:MM`, `-HH:MM`).
    let (clock_str, tz_offset_secs) = split_tz_suffix(time_part)?;
    let (hour, min, sec, millis) = parse_clock_hms(clock_str)?;

    match tz_offset_secs {
        Some(offset_s) => {
            let days = days_from_civil(year, month, day);
            let utc_secs =
                days * 86_400 + i64::from(hour) * 3600 + i64::from(min) * 60 + i64::from(sec)
                    - offset_s;
            Ok(utc_secs * 1000 + millis)
        }
        None => Ok(local_civil_to_epoch_ms(year, month, day, hour, min, sec)? + millis),
    }
}

fn split_tz_suffix(s: &str) -> Result<(&str, Option<i64>)> {
    if let Some(core) = s.strip_suffix('Z').or_else(|| s.strip_suffix('z')) {
        return Ok((core, Some(0)));
    }
    for (idx, ch) in s.char_indices().skip(1) {
        if ch == '+' || ch == '-' {
            let core = &s[..idx];
            let off = &s[idx + 1..];
            let (oh, om) = match off.split_once(':') {
                Some((h, m)) => (h.parse::<i64>()?, m.parse::<i64>()?),
                None if off.len() == 4 => (off[0..2].parse::<i64>()?, off[2..4].parse::<i64>()?),
                None => bail!("invalid timezone offset in '{s}'"),
            };
            let sign = if ch == '-' { -1i64 } else { 1i64 };
            return Ok((core, Some(sign * (oh * 3600 + om * 60))));
        }
    }
    Ok((s, None))
}

fn parse_clock_hms(clock: &str) -> Result<(u32, u32, u32, i64)> {
    let (hms, frac) = match clock.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (clock, None),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    if parts.len() != 2 && parts.len() != 3 {
        bail!("invalid time '{clock}' (expected HH:MM or HH:MM:SS)");
    }
    let hour: u32 = parts[0].parse().context("invalid hour")?;
    let min: u32 = parts[1].parse().context("invalid minute")?;
    let sec: u32 = if parts.len() == 3 {
        parts[2].parse().context("invalid second")?
    } else {
        0
    };
    if hour > 23 || min > 59 || sec > 60 {
        bail!("time out of range in '{clock}'");
    }
    let millis = match frac {
        Some(f) if !f.is_empty() => {
            let digits: String = f.chars().take(3).collect();
            let mut val: i64 = digits.parse().context("invalid fractional seconds")?;
            for _ in digits.len()..3 {
                val *= 10;
            }
            val
        }
        _ => 0,
    };
    Ok((hour, min, sec, millis))
}

fn local_civil_to_epoch_ms(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    min: u32,
    sec: u32,
) -> Result<i64> {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = year - 1900;
    tm.tm_mon = (month as i32) - 1;
    tm.tm_mday = day as i32;
    tm.tm_hour = hour as i32;
    tm.tm_min = min as i32;
    tm.tm_sec = sec as i32;
    tm.tm_isdst = -1;
    // SAFETY: `tm` is a stack-owned valid `struct tm`.
    let t = unsafe { libc::mktime(&mut tm) };
    if t == -1 {
        bail!("cannot convert local date/time {year:04}-{month:02}-{day:02}");
    }
    Ok((t as i64) * 1000)
}

/// Howard Hinnant civil-from-days inverse: days since 1970-01-01 UTC.
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = i64::from(year) - if month <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
