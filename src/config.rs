//! Configuration loaded from `~/.config/hey-computer/config.toml`.
//!
//! Every key has a default, so a missing file is not an error; an unknown key
//! is, so a typo cannot silently leave a setting at its default.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Where the agent's private configuration directory and its workspace
    /// live. Empty means `$XDG_STATE_HOME/hey-computer`.
    pub state_dir: PathBuf,
    pub agent: Agent,
    pub conversation: Conversation,
    pub permissions: Permissions,
    pub instructions: Instructions,
    pub present: Present,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Agent {
    /// The ACP agent: program and arguments, never a shell line.
    /// `{agent_dir}` is replaced by the agent's private directory.
    pub command: Vec<String>,
    /// Set on every new session; empty leaves the agent's own choice.
    pub model: String,
    /// The agent's thinking effort, set on every new session; empty leaves
    /// the agent's own choice.
    pub thought_level: String,
    /// Start the agent with the daemon, so the first request does not pay
    /// for its start (about a second). Off starts it on the first request.
    pub warm_start: bool,
    /// Stop the agent after this long without a request. Zero keeps it up.
    pub idle_stop_secs: f64,
    /// A request is cancelled when the agent sends nothing for this long.
    /// It is not a cap on the whole request: one shell command alone can
    /// take ten seconds.
    pub inactivity_timeout_secs: f64,
    /// Hard wall-clock cap on a single turn, even while updates keep arriving.
    /// Zero disables the cap.
    pub max_turn_secs: f64,
    /// Maximum distinct tool calls allowed in a single turn. Zero disables the
    /// cap.
    pub max_tool_calls: usize,
    /// First delay before restarting an agent that died; it doubles with
    /// every failure in a row, up to a minute.
    pub restart_delay_secs: f64,
    /// agy keeps every conversation (requests, answers, tool output) in its
    /// directory. Before each agent start, conversations and logs older than
    /// this many days are deleted. Zero keeps them all.
    pub history_days: f64,
    /// Merged over the agent's `settings.json` that the daemon writes.
    pub settings: toml::Table,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Conversation {
    /// A request this soon after the previous answer continues that
    /// conversation; a later one starts a new one.
    pub follow_up_secs: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Permissions {
    /// Command words the agent is refused, so that it gets a clean answer it
    /// can report. A courtesy, not a boundary: string matching can always be
    /// got round, and see the README's permission model for what the service
    /// itself does and does not stop.
    pub deny: Vec<String>,
}

/// Default character cap for `instructions.memory_file` (~750–1,000 tokens).
pub const DEFAULT_MEMORY_MAX_CHARS: usize = 4_000;
/// Hard upper bound for `instructions.memory_max_chars`.
pub const MAX_MEMORY_MAX_CHARS: usize = 32_768;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Instructions {
    /// Appended to the built-in instructions in the agent's `AGENTS.md`.
    pub extra: String,
    /// An Obsidian vault the agent may read and write. Empty means none.
    pub vault: PathBuf,
    /// Skill directories made available to the agent: either one skill (a
    /// directory with a `SKILL.md`) or a directory of such skills.
    pub skills: Vec<PathBuf>,
    /// Optional user-owned Markdown memory file (absolute path), loaded into
    /// `AGENTS.md` before each agent start and frozen until the next start.
    /// Empty disables memory loading; a missing or empty file is ignored.
    pub memory_file: PathBuf,
    /// Maximum number of characters loaded from `memory_file`.
    pub memory_max_chars: usize,
}

impl Default for Instructions {
    fn default() -> Self {
        Self {
            extra: String::new(),
            vault: PathBuf::new(),
            skills: Vec::new(),
            memory_file: PathBuf::new(),
            memory_max_chars: DEFAULT_MEMORY_MAX_CHARS,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Present {
    /// Master switch for every notification, errors included.
    pub notifications: bool,
    /// A "Thinking…" notification while a request runs (updated with tool
    /// progress), replaced in place by the answer.
    pub progress: bool,
    /// An answer longer than this many characters goes to the viewer.
    pub max_chars: usize,
    /// An answer with more lines than this goes to the viewer.
    pub max_lines: usize,
    /// The notification program, called with notify-send's options.
    pub notifier: Vec<String>,
    /// Opens a long answer: program and arguments, `{file}` is the Markdown
    /// file (appended when no argument mentions it). Empty shows the file's
    /// path in a notification instead.
    pub viewer: Vec<String>,
    /// How many answer files to keep.
    pub keep_answers: usize,
}

impl Default for Agent {
    fn default() -> Self {
        Self {
            command: ["agy", "acp", "--gemini-dir", "{agent_dir}"]
                .map(String::from)
                .to_vec(),
            model: "gemini-4-hydrogen".to_string(),
            thought_level: "low".to_string(),
            warm_start: true,
            idle_stop_secs: 0.0,
            inactivity_timeout_secs: 45.0,
            max_turn_secs: 300.0,
            max_tool_calls: 20,
            restart_delay_secs: 1.0,
            history_days: 7.0,
            settings: toml::Table::new(),
        }
    }
}

impl Default for Conversation {
    fn default() -> Self {
        Self {
            follow_up_secs: 90.0,
        }
    }
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            deny: ["sudo", "su", "doas", "pkexec", "run0"]
                .map(String::from)
                .to_vec(),
        }
    }
}

impl Default for Present {
    fn default() -> Self {
        Self {
            notifications: true,
            progress: true,
            max_chars: 230,
            max_lines: 4,
            notifier: vec!["notify-send".to_string()],
            viewer: Vec::new(),
            keep_answers: 20,
        }
    }
}

/// The longest duration any setting may have: a year.
pub const MAX_SECS: f64 = 365.0 * 24.0 * 3600.0;
/// Restarts back off up to this.
pub const MAX_RESTART_DELAY_SECS: f64 = 60.0;

/// Seconds from the config as a `Duration`; zero or less means "never".
pub fn secs(value: f64) -> Option<Duration> {
    (value > 0.0 && value.is_finite()).then(|| Duration::from_secs_f64(value))
}

impl Config {
    /// `~/.config/hey-computer/config.toml`.
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("hey-computer").join("config.toml"))
    }

    /// Load from `path`, or from the default location when `path` is `None`.
    /// A missing default file yields the defaults; a missing explicit one, a
    /// malformed one or one that cannot work is an error.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let explicit = path.is_some();
        let Some(path) = path.map(Path::to_path_buf).or_else(Self::default_path) else {
            return Ok(Self::default());
        };

        let config: Self = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("parsing config file {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => Self::default(),
            Err(e) => {
                return Err(e).with_context(|| format!("reading config file {}", path.display()));
            }
        };
        config
            .validate()
            .with_context(|| format!("in config file {}", path.display()))?;
        Ok(config)
    }

    /// What serde cannot check.
    pub fn validate(&self) -> Result<()> {
        if self.agent.command.is_empty() || self.agent.command[0].is_empty() {
            bail!("agent.command must name a program");
        }
        for (key, value) in [
            ("agent.idle_stop_secs", self.agent.idle_stop_secs),
            (
                "agent.inactivity_timeout_secs",
                self.agent.inactivity_timeout_secs,
            ),
            ("agent.max_turn_secs", self.agent.max_turn_secs),
            ("agent.restart_delay_secs", self.agent.restart_delay_secs),
            (
                "conversation.follow_up_secs",
                self.conversation.follow_up_secs,
            ),
        ] {
            if !value.is_finite() || value < 0.0 {
                bail!("{key} must be zero or a positive number of seconds, not {value}");
            }
            // Far beyond any sensible setting, and well inside what a
            // Duration added to an Instant can hold.
            if value > MAX_SECS {
                bail!("{key} must be at most {MAX_SECS} seconds (a year), not {value}");
            }
        }
        let days = self.agent.history_days;
        if !days.is_finite() || !(0.0..=3650.0).contains(&days) {
            bail!("agent.history_days must be between 0 and 3650, not {days}");
        }
        if self.agent.restart_delay_secs > MAX_RESTART_DELAY_SECS {
            bail!(
                "agent.restart_delay_secs must be at most {MAX_RESTART_DELAY_SECS}, the longest the back-off waits"
            );
        }
        if self.agent.inactivity_timeout_secs == 0.0 {
            bail!(
                "agent.inactivity_timeout_secs must be positive: a stuck agent would hang forever"
            );
        }
        if !self.present.notifier.is_empty() && self.present.notifier[0].is_empty() {
            bail!("present.notifier must name a program");
        }
        if self.present.max_lines == 0 || self.present.max_chars == 0 {
            bail!("present.max_lines and present.max_chars must be positive");
        }
        if !self.instructions.vault.as_os_str().is_empty() && !self.instructions.vault.is_absolute()
        {
            bail!("instructions.vault must be an absolute path");
        }
        if !self.instructions.memory_file.as_os_str().is_empty() {
            if !self.instructions.memory_file.is_absolute() {
                bail!("instructions.memory_file must be an absolute path");
            }
            if !self.instructions.vault.as_os_str().is_empty() {
                let memory = normalize_lexical(&self.instructions.memory_file);
                let vault = normalize_lexical(&self.instructions.vault);
                if memory.starts_with(&vault) {
                    bail!(
                        "instructions.memory_file must not be inside instructions.vault (the vault is untrusted data)"
                    );
                }
            }
        }
        if !(1..=MAX_MEMORY_MAX_CHARS).contains(&self.instructions.memory_max_chars) {
            bail!(
                "instructions.memory_max_chars must be between 1 and {MAX_MEMORY_MAX_CHARS}, not {}",
                self.instructions.memory_max_chars
            );
        }
        Ok(())
    }

    /// `state_dir`, or `$XDG_STATE_HOME/hey-computer`.
    pub fn state_dir(&self) -> Result<PathBuf> {
        if !self.state_dir.as_os_str().is_empty() {
            return Ok(self.state_dir.clone());
        }
        dirs::state_dir()
            .map(|d| d.join("hey-computer"))
            .context("no state directory: set state_dir or XDG_STATE_HOME")
    }
}

/// Collapse `.` and `..` components without touching the filesystem.
pub(crate) fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_toml() {
        let text = toml::to_string(&Config::default()).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.agent.model, "gemini-4-hydrogen");
        assert_eq!(parsed.agent.max_turn_secs, 300.0);
        assert_eq!(parsed.agent.max_tool_calls, 20);
        assert_eq!(parsed.present.max_chars, 230);
        assert!(parsed.instructions.memory_file.as_os_str().is_empty());
        assert_eq!(
            parsed.instructions.memory_max_chars,
            DEFAULT_MEMORY_MAX_CHARS
        );
        parsed.validate().unwrap();
    }

    #[test]
    fn partial_config_keeps_defaults() {
        let parsed: Config =
            toml::from_str("[agent]\nthought_level = \"medium\"\n[present]\nmax_lines = 6\n")
                .unwrap();
        assert_eq!(parsed.agent.thought_level, "medium");
        assert_eq!(parsed.agent.inactivity_timeout_secs, 45.0);
        assert_eq!(parsed.present.max_lines, 6);
        assert_eq!(parsed.conversation.follow_up_secs, 90.0);
        assert_eq!(parsed.permissions.deny[0], "sudo");
        assert!(parsed.instructions.memory_file.as_os_str().is_empty());
        assert_eq!(
            parsed.instructions.memory_max_chars,
            DEFAULT_MEMORY_MAX_CHARS
        );
    }

    #[test]
    fn whole_seconds_and_fractions_both_parse() {
        let parsed: Config = toml::from_str(
            "[agent]\ninactivity_timeout_secs = 30\n[conversation]\nfollow_up_secs = 0.5\n",
        )
        .unwrap();
        assert_eq!(parsed.agent.inactivity_timeout_secs, 30.0);
        assert_eq!(
            secs(parsed.conversation.follow_up_secs)
                .unwrap()
                .as_millis(),
            500
        );
        assert_eq!(secs(0.0), None);
    }

    #[test]
    fn unknown_keys_are_errors() {
        assert!(toml::from_str::<Config>("[agent]\nmodle = \"x\"\n").is_err());
        assert!(toml::from_str::<Config>("colour = 1\n").is_err());
    }

    #[test]
    fn free_form_agent_settings_are_kept() {
        let parsed: Config =
            toml::from_str("[agent.settings]\nshowTips = false\n[agent.settings.permissions]\nallow = [\"read_url(example.com)\"]\n")
                .unwrap();
        assert_eq!(
            parsed.agent.settings["showTips"],
            toml::Value::Boolean(false)
        );
    }

    #[test]
    fn nonsense_values_are_refused() {
        let mut config = Config::default();
        config.agent.command.clear();
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.agent.inactivity_timeout_secs = 0.0;
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.instructions.vault = PathBuf::from("Notes");
        assert!(config.validate().is_err());
    }

    #[test]
    fn durations_beyond_a_year_are_refused() {
        // 1e19 s does not fit a Duration added to an Instant; it panicked the
        // conductor after passing validation.
        let mut config = Config::default();
        config.agent.idle_stop_secs = 1e19;
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.conversation.follow_up_secs = 1e19;
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.agent.inactivity_timeout_secs = MAX_SECS + 1.0;
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.agent.restart_delay_secs = 61.0;
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.agent.idle_stop_secs = MAX_SECS;
        config.conversation.follow_up_secs = MAX_SECS;
        config.agent.restart_delay_secs = 60.0;
        config.validate().unwrap();
    }

    #[test]
    fn memory_config_is_validated() {
        let mut config = Config::default();
        config.instructions.memory_file = PathBuf::from("memory.md");
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.instructions.memory_max_chars = 0;
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.instructions.memory_max_chars = MAX_MEMORY_MAX_CHARS + 1;
        assert!(config.validate().is_err());

        for bad in [
            "/tmp/vault/memory.md",
            "/tmp/vault/sub/memory.md",
            "/tmp/other/../vault/memory.md",
        ] {
            let mut config = Config::default();
            config.instructions.vault = PathBuf::from("/tmp/vault");
            config.instructions.memory_file = PathBuf::from(bad);
            assert!(config.validate().is_err(), "{bad} should be refused");
        }

        for good in ["/tmp/vault-memory.md", "/tmp/config/memory.md"] {
            let mut config = Config::default();
            config.instructions.vault = PathBuf::from("/tmp/vault");
            config.instructions.memory_file = PathBuf::from(good);
            config.validate().unwrap();
        }
    }

    #[test]
    fn turn_budget_config_is_validated() {
        let parsed: Config =
            toml::from_str("[agent]\nmax_turn_secs = 0\nmax_tool_calls = 0\n").unwrap();
        assert_eq!(parsed.agent.max_turn_secs, 0.0);
        assert_eq!(parsed.agent.max_tool_calls, 0);
        parsed.validate().unwrap();

        let parsed: Config =
            toml::from_str("[agent]\nmax_turn_secs = 12.5\nmax_tool_calls = 15\n").unwrap();
        assert_eq!(parsed.agent.max_turn_secs, 12.5);
        assert_eq!(parsed.agent.max_tool_calls, 15);
        parsed.validate().unwrap();

        for bad in [-1.0, f64::NAN, f64::INFINITY, MAX_SECS + 1.0] {
            let mut config = Config::default();
            config.agent.max_turn_secs = bad;
            assert!(config.validate().is_err(), "{bad} should be refused");
        }

        let mut config = Config::default();
        config.agent.max_turn_secs = MAX_SECS;
        config.validate().unwrap();
    }
}
