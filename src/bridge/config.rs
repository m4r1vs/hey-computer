//! Configuration for `hey-computer-bridge` (`~/.config/hey-computer/bridge.toml`).
//!
//! Enforces `read_only = true` at load and validation time, supports runtime
//! secret files (`allow_from_file`, `account_file`) so personal phone numbers
//! never need to live in public configs or `/nix/store`, and avoids logging
//! phone numbers or allowlist contents above `debug`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Cap on runtime allowlist/account secret files so misconfigured paths cannot
/// consume unbounded memory.
const MAX_SECRET_FILE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BridgeConfig {
    /// Private state directory (`0700`). Empty defaults to
    /// `$XDG_STATE_HOME/hey-computer-bridge`.
    pub state_dir: PathBuf,
    /// Must remain `true`; setting `false` is rejected by `validate()`.
    pub read_only: bool,
    /// Rolling conversation context window retained per chat.
    pub context_messages: usize,
    /// Maximum chats retained in the in-memory LRU ring buffer.
    pub max_tracked_chats: usize,
    /// Burst coalescing window for incoming DMs in milliseconds.
    pub debounce_ms: u64,
    /// Burst coalescing window for verified phone self-chat in milliseconds.
    pub self_chat_debounce_ms: u64,
    /// Upper bound on coalescing delay for a burst in milliseconds.
    pub max_debounce_ms: u64,
    /// Age fence (seconds): older catch-up messages populate context only and
    /// never trigger live events.
    pub max_message_age_secs: u64,
    pub whatsapp: WhatsappConfig,
    pub signal: SignalConfig,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            state_dir: PathBuf::new(),
            read_only: true,
            context_messages: 10,
            max_tracked_chats: 64,
            debounce_ms: 1500,
            self_chat_debounce_ms: 800,
            max_debounce_ms: 5000,
            max_message_age_secs: 120,
            whatsapp: WhatsappConfig::default(),
            signal: SignalConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WhatsappConfig {
    pub enabled: bool,
    pub self_chat: bool,
    pub allow_from: Vec<String>,
    pub allow_from_file: Option<PathBuf>,
    pub groups_enabled: bool,
    /// Override for the Baileys reader binary (primarily for tests or custom installs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reader_command: Vec<String>,
}

impl Default for WhatsappConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            self_chat: true,
            allow_from: Vec::new(),
            allow_from_file: None,
            groups_enabled: false,
            reader_command: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SignalConfig {
    pub enabled: bool,
    pub self_chat: bool,
    pub account: Option<String>,
    pub account_file: Option<PathBuf>,
    pub allow_from: Vec<String>,
    pub allow_from_file: Option<PathBuf>,
    pub groups_enabled: bool,
    /// Override for the `signal-cli` command (primarily for tests or custom installs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
}

impl Default for SignalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            self_chat: true,
            account: None,
            account_file: None,
            allow_from: Vec::new(),
            allow_from_file: None,
            groups_enabled: false,
            command: Vec::new(),
        }
    }
}

impl BridgeConfig {
    /// Load from `path` (or `~/.config/hey-computer/bridge.toml`). A missing
    /// default config file yields `Default::default()`; an explicit missing
    /// path is an error.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None => default_config_path(),
        };
        let cfg = match std::fs::read_to_string(&path) {
            Ok(raw) => toml::from_str::<Self>(&raw)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && explicit.is_none() => {
                Self::default()
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// Parse TOML directly and validate.
    pub fn parse_toml(raw: &str) -> Result<Self> {
        let cfg: Self = toml::from_str(raw).context("parsing bridge.toml")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Enforce read-only invariants and sanity-check numeric bounds.
    pub fn validate(&self) -> Result<()> {
        if !self.read_only {
            bail!(
                "hey-computer-bridge is strictly read-only; setting read_only = false is rejected"
            );
        }
        if self.context_messages == 0 || self.context_messages > 100 {
            bail!("context_messages must be between 1 and 100");
        }
        if self.max_tracked_chats == 0 || self.max_tracked_chats > 1024 {
            bail!("max_tracked_chats must be between 1 and 1024");
        }
        if self.debounce_ms > 30_000 {
            bail!("debounce_ms must be at most 30000");
        }
        if self.self_chat_debounce_ms > 30_000 {
            bail!("self_chat_debounce_ms must be at most 30000");
        }
        if self.max_debounce_ms > 60_000 {
            bail!("max_debounce_ms must be at most 60000");
        }
        if self.max_message_age_secs == 0 || self.max_message_age_secs > 86_400 {
            bail!("max_message_age_secs must be between 1 and 86400");
        }
        if let Some(acct) = &self.signal.account
            && !acct.trim().is_empty()
            && normalize_e164(acct).is_none()
        {
            bail!("signal.account must be a valid E.164 phone number");
        }
        Ok(())
    }

    /// Resolve the bridge state directory (`$XDG_STATE_HOME/hey-computer-bridge`
    /// by default).
    pub fn resolved_state_dir(&self) -> Result<PathBuf> {
        if !self.state_dir.as_os_str().is_empty() {
            return Ok(self.state_dir.clone());
        }
        let base = dirs::state_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
            .context("cannot determine state_dir (set XDG_STATE_HOME or state_dir)")?;
        Ok(base.join("hey-computer-bridge"))
    }
}

/// Default config path: `$XDG_CONFIG_HOME/hey-computer/bridge.toml`.
pub fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hey-computer")
        .join("bridge.toml")
}

/// Resolve the bridge socket path: explicit `--socket`, then
/// `$HEY_COMPUTER_BRIDGE_SOCKET`, then `$XDG_RUNTIME_DIR/hey-computer-bridge.sock`.
pub fn default_socket_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("HEY_COMPUTER_BRIDGE_SOCKET").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    Ok(crate::ipc::runtime_dir()?.join("hey-computer-bridge.sock"))
}

/// Create a directory (if missing) and enforce `0700` permissions owned by the
/// current effective UID.
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    if let Some(parent) = dir.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
    }
    let meta =
        std::fs::symlink_metadata(dir).with_context(|| format!("checking {}", dir.display()))?;
    // SAFETY: geteuid cannot fail and touches no memory.
    let uid = unsafe { libc::geteuid() };
    if !meta.file_type().is_dir() || meta.uid() != uid {
        bail!(
            "{} must be a private directory owned by this user",
            dir.display()
        );
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 0700 {}", dir.display()))?;
    }
    Ok(())
}

/// Read a bounded regular secret file without logging its contents.
fn read_bounded_secret_file(path: &Path) -> Result<String> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("checking secret file {}", path.display()))?;
    if !meta.is_file() {
        bail!("secret path {} is not a regular file", path.display());
    }
    if meta.len() > MAX_SECRET_FILE_BYTES {
        bail!(
            "secret file {} exceeds {} bytes",
            path.display(),
            MAX_SECRET_FILE_BYTES
        );
    }
    std::fs::read_to_string(path).with_context(|| format!("reading secret file {}", path.display()))
}

/// Normalized allowlist supporting E.164 numbers, `@lid`, `uuid:<aci>`, group
/// IDs, and explicit `"*"` wildcard.
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    allow_all: bool,
    entries: HashSet<String>,
}

impl Allowlist {
    pub fn load(inline: &[String], file: Option<&Path>) -> Result<Self> {
        let mut raw_entries: Vec<String> = inline.to_vec();
        if let Some(path) = file {
            let contents = read_bounded_secret_file(path)?;
            for line in contents.lines() {
                let trimmed = line.split('#').next().unwrap_or("").trim();
                if trimmed.is_empty() {
                    continue;
                }
                for part in trimmed.split(',') {
                    let item = part.trim();
                    if !item.is_empty() {
                        raw_entries.push(item.to_string());
                    }
                }
            }
        }
        let mut list = Self::default();
        for entry in raw_entries {
            let trimmed = entry.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed == "*" {
                list.allow_all = true;
            } else {
                list.entries.insert(normalize_identifier(trimmed));
            }
        }
        Ok(list)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        !self.allow_all && self.entries.is_empty()
    }

    pub fn matches_any<'a>(&self, candidates: impl IntoIterator<Item = &'a str>) -> bool {
        if self.allow_all {
            return true;
        }
        if self.entries.is_empty() {
            return false;
        }
        candidates
            .into_iter()
            .filter(|c| !c.is_empty())
            .any(|c| self.entries.contains(&normalize_identifier(c)))
    }
}

/// Resolve the Signal account E.164 number from `account_file`, `account`, or
/// `<signal_cfg_dir>/data/accounts.json` written by `signal-cli link`.
/// Returns `Ok(None)` when no explicit account is configured and `signal-cli`
/// has not been linked yet.
pub fn resolve_signal_account(
    cfg: &SignalConfig,
    signal_cfg_dir: Option<&Path>,
) -> Result<Option<String>> {
    if let Some(path) = &cfg.account_file {
        let contents = read_bounded_secret_file(path)?;
        if let Some(line) = contents
            .lines()
            .map(|l| l.split('#').next().unwrap_or("").trim())
            .find(|l| !l.is_empty())
        {
            let e164 = normalize_e164(line).with_context(|| {
                format!(
                    "signal account in {} must be a valid E.164 phone number",
                    path.display()
                )
            })?;
            return Ok(Some(e164));
        }
    }
    if let Some(acct) = &cfg.account
        && !acct.trim().is_empty()
    {
        let e164 =
            normalize_e164(acct).context("signal account must be a valid E.164 phone number")?;
        return Ok(Some(e164));
    }
    if let Some(dir) = signal_cfg_dir
        && let Some(discovered) = discover_linked_signal_account(dir)
    {
        return Ok(Some(discovered));
    }
    Ok(None)
}

/// Inspect `<signal_cfg_dir>/data/accounts.json` created by `signal-cli link`
/// and return the first valid E.164 account number if present.
pub fn discover_linked_signal_account(signal_cfg_dir: &Path) -> Option<String> {
    let accounts_path = signal_cfg_dir.join("data").join("accounts.json");
    let raw = read_bounded_secret_file(&accounts_path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let accounts = parsed.get("accounts")?.as_array()?;
    for entry in accounts {
        if let Some(num) = entry.get("number").and_then(|v| v.as_str())
            && let Some(e164) = normalize_e164(num)
        {
            return Some(e164);
        }
    }
    None
}

/// RAII lockfile marker (`<dir>/.pairing.lock`) held during interactive QR
/// pairing so a running `hey-computer-messenger-bridge` daemon waits quietly
/// until pairing completes and then attaches automatically.
pub struct PairingLockGuard {
    path: PathBuf,
}

impl PairingLockGuard {
    pub fn acquire(dir: &Path) -> Self {
        let path = dir.join(".pairing.lock");
        let _ = std::fs::write(&path, format!("{}\n", std::process::id()));
        Self { path }
    }
}

impl Drop for PairingLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Whether `<dir>/.pairing.lock` currently belongs to a live pairing process.
pub fn is_pairing_in_progress(dir: &Path) -> bool {
    let lock_path = dir.join(".pairing.lock");
    let Ok(raw) = std::fs::read_to_string(&lock_path) else {
        return false;
    };
    if let Ok(pid) = raw.trim().parse::<libc::pid_t>()
        && pid > 0
    {
        // SAFETY: kill(pid, 0) checks process existence without sending a signal.
        let rc = unsafe { libc::kill(pid, 0) };
        if rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
            return true;
        }
    }
    let _ = std::fs::remove_file(&lock_path);
    false
}

/// Strip device suffix (`:NN`) from a WhatsApp JID (`1234:2@s.whatsapp.net` -> `1234@s.whatsapp.net`).
pub fn normalize_jid(jid: &str) -> String {
    let trimmed = jid.trim();
    let Some((user, domain)) = trimmed.split_once('@') else {
        return trimmed.to_string();
    };
    let bare_user = user.split(':').next().unwrap_or(user);
    format!("{bare_user}@{}", domain.to_ascii_lowercase())
}

/// Normalize a phone number or `@s.whatsapp.net` JID into canonical `+<digits>` E.164.
pub fn normalize_e164(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let candidate = if let Some((user, domain)) = trimmed.split_once('@') {
        if !domain.eq_ignore_ascii_case("s.whatsapp.net") {
            return None;
        }
        user.split(':').next().unwrap_or(user)
    } else {
        trimmed
    };

    // Reject UUIDs or alphabetic strings so they are not misread as phone numbers.
    if candidate.chars().any(|c| c.is_ascii_alphabetic()) {
        return None;
    }

    let mut digits = String::new();
    for ch in candidate.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        }
    }
    if let Some(stripped) = digits.strip_prefix("00")
        && !candidate.starts_with('+')
    {
        digits = stripped.to_string();
    }
    if !(7..=15).contains(&digits.len()) {
        return None;
    }
    Some(format!("+{digits}"))
}

/// Canonical identifier for allowlist matching and context lookups:
/// - E.164 phone numbers / `@s.whatsapp.net` JIDs -> `+<digits>`
/// - `@lid` JIDs -> `<bare_digits>@lid`
/// - `uuid:<aci>` or bare UUIDs -> `uuid:<lowercase>`
/// - `@g.us` / `group:<id>` -> `group:<id>`
/// - Other strings -> trimmed lowercase
pub fn normalize_identifier(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(e164) = normalize_e164(trimmed) {
        return e164;
    }
    if let Some(rest) = trimmed.strip_prefix("uuid:") {
        return format!("uuid:{}", rest.trim().to_ascii_lowercase());
    }
    if let Some(rest) = trimmed.strip_prefix("UUID:") {
        return format!("uuid:{}", rest.trim().to_ascii_lowercase());
    }
    if is_uuid_like(trimmed) {
        return format!("uuid:{}", trimmed.to_ascii_lowercase());
    }
    if let Some((_, domain)) = trimmed.split_once('@') {
        if domain.eq_ignore_ascii_case("lid") {
            return normalize_jid(trimmed);
        }
        if domain.eq_ignore_ascii_case("g.us") {
            return format!("group:{}", normalize_jid(trimmed));
        }
    }
    if let Some(rest) = trimmed.strip_prefix("group:") {
        return format!("group:{}", rest.trim());
    }
    trimmed.to_ascii_lowercase()
}

fn is_uuid_like(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (idx, &b) in bytes.iter().enumerate() {
        if matches!(idx, 8 | 13 | 18 | 23) {
            if b != b'-' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_read_only_false_and_unknown_fields() {
        assert!(BridgeConfig::parse_toml("").is_ok());
        assert!(BridgeConfig::parse_toml("read_only = true").is_ok());

        let err = BridgeConfig::parse_toml("read_only = false").unwrap_err();
        assert!(err.to_string().contains("strictly read-only"));

        assert!(BridgeConfig::parse_toml("allow_outbound = true").is_err());
    }

    #[test]
    fn normalizes_identifiers_and_allowlists() {
        assert_eq!(
            normalize_e164("+49 151-0000 0001").as_deref(),
            Some("+4915100000001")
        );
        assert_eq!(
            normalize_e164("4915100000001:4@s.whatsapp.net").as_deref(),
            Some("+4915100000001")
        );
        assert_eq!(normalize_e164("12345678901234@lid"), None);
        assert_eq!(
            normalize_identifier("12345678901234:7@lid"),
            "12345678901234@lid"
        );
        assert_eq!(
            normalize_identifier("550E8400-E29B-41D4-A716-446655440000"),
            "uuid:550e8400-e29b-41d4-a716-446655440000"
        );

        let list = Allowlist::load(
            &[
                "+49 151 0000 0001".to_string(),
                "uuid:550E8400-E29B-41D4-A716-446655440000".to_string(),
            ],
            None,
        )
        .unwrap();
        assert!(list.matches_any(["4915100000001@s.whatsapp.net"]));
        assert!(list.matches_any(["550e8400-e29b-41d4-a716-446655440000"]));
        assert!(!list.matches_any(["+4915100000099"]));
    }

    #[test]
    fn resolves_signal_account_from_commented_file_and_accounts_json() {
        let dir = std::env::temp_dir().join(format!("hc-sig-acct-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let data_dir = dir.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let acct_file = dir.join("signal-account");
        std::fs::write(&acct_file, "# leave commented to auto-discover\n\n").unwrap();

        let cfg = SignalConfig {
            enabled: true,
            account_file: Some(acct_file),
            ..SignalConfig::default()
        };
        assert_eq!(resolve_signal_account(&cfg, Some(&dir)).unwrap(), None);

        std::fs::write(
            data_dir.join("accounts.json"),
            r#"{"accounts":[{"number":"+4915100000042","uuid":"550e8400-e29b-41d4-a716-446655440000"}]}"#,
        )
        .unwrap();
        assert_eq!(
            resolve_signal_account(&cfg, Some(&dir)).unwrap().as_deref(),
            Some("+4915100000042")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
