//! Configuration loaded from `~/.config/hey-computer/config.toml`.
//!
//! Every key has a default, so a missing file is not an error; an unknown key
//! is, so a typo cannot silently leave a setting at its default.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Where the agent's private configuration directory and its workspace
    /// live. Empty means `$XDG_STATE_HOME/hey-computer`.
    pub state_dir: PathBuf,
    pub agent: Agent,
    /// Named model/effort/budget profiles selectable with `ask --profile <name>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, Profile>,
    /// MCP servers written to `<agent_dir>/config/mcp_config.json` and gated
    /// per turn by `LanePolicy` (using each profile's `mcp_servers` allowlist).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, McpServer>,
    pub conversation: Conversation,
    pub tasks: Tasks,
    pub scheduler: Scheduler,
    pub routing: Routing,
    pub permissions: Permissions,
    pub instructions: Instructions,
    pub context: Context,
    pub history: History,
    pub present: Present,
}

/// Retention settings for the local SQLite history database (`history.db`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct History {
    /// Delete terminal turns older than this many days. Zero disables age pruning.
    pub retention_days: f64,
    /// Maximum terminal turns retained in `history.db`. Zero disables the cap.
    pub max_entries: usize,
    /// Delete suppressed (e.g. `HEARTBEAT_OK`) turns older than this many days.
    /// Zero disables separate suppressed pruning.
    pub suppressed_days: f64,
}

impl Default for History {
    fn default() -> Self {
        Self {
            retention_days: 90.0,
            max_entries: 10_000,
            suppressed_days: 7.0,
        }
    }
}

/// Named profile overriding `[agent]` settings on a per-request basis.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    /// Overrides `agent.model` when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Overrides `agent.thought_level` when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_level: Option<String>,
    /// Overrides `agent.inactivity_timeout_secs` when set (must be positive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inactivity_timeout_secs: Option<f64>,
    /// Overrides `agent.max_turn_secs` when set (0 disables the cap).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turn_secs: Option<f64>,
    /// Overrides `agent.max_tool_calls` when set (0 disables the cap).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<usize>,
    /// Overrides `agent.max_repeat_tool_calls` when set (0 disables the guard).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_repeat_tool_calls: Option<usize>,
    /// Restricts which configured `[mcp.<name>]` servers this profile may call
    /// (enforced per turn by `LanePolicy` on `call_mcp_tool` / `mcp_*` calls).
    /// `None` allows all enabled servers; `Some([])` allows none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Vec<String>>,
    /// Custom progress notification summary while this profile is running
    /// (for example `"Pondering Hard…"`). Omit for `"Thinking… (<name>)"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_label: Option<String>,
}

/// Configuration for one MCP server written to `<agent_dir>/config/mcp_config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpServer {
    /// Disable a configured server without removing its section.
    pub enabled: bool,
    /// Stdio transport: program path or name on the agent's PATH.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Non-secret environment variables for stdio servers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Runtime-loaded secret environment variables (`KEY = "/abs/path"`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_files: BTreeMap<String, PathBuf>,
    /// HTTP/SSE transport endpoint (`https://…` or loopback `http://…`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// `"http"` (default when `url` is set) or `"sse"`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transport: String,
    /// Non-secret HTTP headers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Runtime-loaded secret HTTP headers (`Header = "/abs/path"`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub header_files: BTreeMap<String, PathBuf>,
    /// Optional Nerd Font icon shown in notification titles when this MCP server
    /// is called (`""` suppresses the server's default icon).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Optional per-tool icon overrides matched against the MCP tool name
    /// (`exact`, `prefix*`, `*suffix`, or `*substring*`; `""` suppresses).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_icons: BTreeMap<String, String>,
}

impl Default for McpServer {
    fn default() -> Self {
        Self {
            enabled: true,
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            env_files: BTreeMap::new(),
            url: String::new(),
            transport: String::new(),
            headers: BTreeMap::new(),
            header_files: BTreeMap::new(),
            icon: None,
            tool_icons: BTreeMap::new(),
        }
    }
}

/// Effective model, effort, budgets, and MCP server set after applying a
/// profile (if any) on top of `[agent]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProfile {
    /// Profile name, or `None` for base `[agent]` (`"default"`).
    pub name: Option<String>,
    pub model: String,
    pub thought_level: String,
    pub inactivity: Duration,
    pub max_turn: Option<Duration>,
    pub max_tool_calls: Option<usize>,
    pub max_repeat_tool_calls: Option<usize>,
    /// Sorted list of enabled `[mcp.<name>]` servers allowed for this profile.
    pub mcp_servers: Vec<String>,
    /// Optional custom progress notification summary for this profile.
    pub thinking_label: Option<String>,
}

/// Maximum length of a profile name in `[profiles.<name>]`.
pub const MAX_PROFILE_NAME_LEN: usize = 32;
/// Maximum character length of `profiles.<name>.thinking_label`.
pub const MAX_THINKING_LABEL_CHARS: usize = 64;
/// Maximum character length of a single tool/MCP icon string.
pub const MAX_TOOL_ICON_CHARS: usize = 16;

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
    /// Wait for the Freedesktop Secret Service (`org.freedesktop.secrets`
    /// `/org/freedesktop/secrets/aliases/default`) to report `Locked == false`
    /// before spawning the agent, avoiding a hidden `gcr-prompter` timeout
    /// behind a boot lock screen. Degrades cleanly when no Secret Service is
    /// present on the session bus.
    pub wait_for_keyring: bool,
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
    /// Abort a turn when the agent makes this many consecutive identical tool
    /// calls (same tool name/kind and canonical JSON arguments). Zero disables
    /// the guard.
    pub max_repeat_tool_calls: usize,
    /// First delay before restarting an agent that died; it doubles with
    /// every failure in a row, up to a minute.
    pub restart_delay_secs: f64,
    /// agy keeps every conversation (requests, answers, tool output) in its
    /// directory. Before each agent start, conversations and logs older than
    /// this many days are deleted. Zero keeps them all.
    pub history_days: f64,
    /// Optional path to an existing `mcp_oauth_tokens.json` (for example from
    /// an interactive `agy` install) symlinked into `<agent_dir>/antigravity-cli/`
    /// before agent start so OAuth-authenticated remote MCP servers share tokens.
    pub mcp_oauth_tokens_file: PathBuf,
    /// Optional default MCP server allowlist for `[agent]` (`default` profile).
    /// `None` allows all enabled `[mcp.*]` servers; `Some([])` allows none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Vec<String>>,
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

/// Concurrency and queueing settings for background tasks (`ask --background`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Tasks {
    /// Maximum background tasks running concurrently alongside the interactive
    /// lane.
    pub max_concurrent: usize,
    /// Maximum background tasks waiting in the FIFO queue.
    pub max_queued: usize,
    /// Default profile for background tasks when `--profile` is omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

impl Default for Tasks {
    fn default() -> Self {
        Self {
            max_concurrent: 1,
            max_queued: 32,
            profile: None,
        }
    }
}

/// Settings for the daemon's SQLite-backed hybrid scheduler (`hey-computer schedule`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Scheduler {
    /// Enable the background job scheduler loop inside the daemon.
    pub enabled: bool,
    /// Minimum allowed interval between recurring runs in seconds.
    pub min_interval_secs: f64,
    /// Timeout in seconds for `--script` pre-collector shell commands.
    pub script_timeout_secs: f64,
    /// Maximum number of scheduled jobs stored in `history.db`.
    pub max_jobs: usize,
    /// Declarative jobs reconciled into `history.db` (`managed = 1`) on daemon start.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub jobs: BTreeMap<String, ScheduledJobConfig>,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self {
            enabled: true,
            min_interval_secs: 60.0,
            script_timeout_secs: 30.0,
            max_jobs: 128,
            jobs: BTreeMap::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

/// Declarative scheduled job under `[scheduler.jobs.<name>]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScheduledJobConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub schedule: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub script: String,
    pub no_agent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppress_token: Option<String>,
    pub failure_deliver: crate::store::FailureDeliver,
}

impl Default for ScheduledJobConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            schedule: String::new(),
            prompt: String::new(),
            profile: String::new(),
            script: String::new(),
            no_agent: false,
            suppress_token: None,
            failure_deliver: crate::store::FailureDeliver::Notify,
        }
    }
}

/// Automatic model/effort profile and background-lane routing when `--profile`
/// and `--background` are omitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Routing {
    /// Enable automatic profile and lane routing.
    pub enabled: bool,
    /// Profile used for fast/simple interactive turns (`"default"` or a named profile).
    pub fast_profile: String,
    /// Profile used for complex/analytical turns or escalations.
    pub deep_profile: String,
    /// Automatically route long multi-step / delegation prompts on new non-waiting
    /// turns to the background lane.
    pub auto_background: bool,
    /// Retry once on `deep_profile` if an auto-routed `fast_profile` turn ends
    /// with an empty answer or retryable agent/tool failure.
    pub escalate_on_failure: bool,
    /// Run a bounded, tool-disabled LLM triage turn on `fast_profile` when
    /// heuristics are unsure — strictly for non-voice inputs (`source != voice`).
    pub triage_non_voice: bool,
    /// Timeout in seconds for the non-voice LLM triage turn.
    pub triage_timeout_secs: f64,
}

impl Default for Routing {
    fn default() -> Self {
        Self {
            enabled: false,
            fast_profile: "default".to_string(),
            deep_profile: "deep".to_string(),
            auto_background: true,
            escalate_on_failure: true,
            triage_non_voice: true,
            triage_timeout_secs: 4.0,
        }
    }
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
    /// `AGENTS.md` before each agent start and refreshed when idle outside
    /// active conversations. Empty disables; missing/empty files are ignored.
    pub memory_file: PathBuf,
    /// Additional user-owned Markdown memory files (absolute paths, outside the
    /// vault) appended after `memory_file` within the shared `memory_max_chars` cap.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory_files: Vec<PathBuf>,
    /// Maximum number of characters loaded across configured memory files.
    pub memory_max_chars: usize,
}

impl Default for Instructions {
    fn default() -> Self {
        Self {
            extra: String::new(),
            vault: PathBuf::new(),
            skills: Vec::new(),
            memory_file: PathBuf::new(),
            memory_files: Vec::new(),
            memory_max_chars: DEFAULT_MEMORY_MAX_CHARS,
        }
    }
}

/// Ephemeral per-turn ambient context prepended to the user prompt inside
/// `<untrusted_desktop_context>` delimiters (opt-in).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Context {
    /// Prepend local date, time, timezone offset and weekday to each user turn.
    pub time: bool,
    /// Prepend active Hyprland workspace and focused window class/title to each
    /// user turn (requires `hyprctl` on PATH; degrades silently when unavailable).
    pub desktop: bool,
}

impl Context {
    pub fn is_enabled(&self) -> bool {
        self.time || self.desktop
    }
}

/// Media volume behavior while spoken TTS audio is playing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaDuck {
    #[default]
    None,
    Pause,
    Lower,
}

/// Optional spoken-answer and earcon settings (`[present.speak]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Speak {
    /// Default enabled state when no runtime `<state_dir>/speak_enabled`
    /// toggle file has been written yet.
    pub enabled: bool,
    /// Speak only when the request arrived via voice (`source == "voice"`),
    /// keeping typed/CLI requests text-only.
    #[serde(default = "default_true")]
    pub voice_only: bool,
    /// Default TTS command fed one sentence on stdin (`{lang}` replaced by
    /// `"en"` or `"de"`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Optional English-specific TTS command override (fed sentence on stdin).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command_en: Vec<String>,
    /// Optional German-specific TTS command override (fed sentence on stdin).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command_de: Vec<String>,
    /// Optional subtle audio cue played when a voice turn starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub earcon_start: Vec<String>,
    /// Optional subtle audio cue played when a voice turn finishes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub earcon_done: Vec<String>,
    /// Optional subtle audio cue played when a turn or TTS playback is cancelled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub earcon_cancel: Vec<String>,
    /// Optional media ducking (`"none"`, `"pause"`, or `"lower"`) while speaking.
    pub media_duck: MediaDuck,
}

impl Default for Speak {
    fn default() -> Self {
        Self {
            enabled: false,
            voice_only: true,
            command: Vec::new(),
            command_en: Vec::new(),
            command_de: Vec::new(),
            earcon_start: Vec::new(),
            earcon_done: Vec::new(),
            earcon_cancel: Vec::new(),
            media_duck: MediaDuck::None,
        }
    }
}

/// Sequential Nerd Font tool/MCP icon settings (`[present.tool_icons]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolIcons {
    /// Append sequential Nerd Font icons for tools and MCP servers used during
    /// a turn to notification titles.
    pub enabled: bool,
    /// Maximum distinct icons shown in a notification title before appending `…`.
    pub max_icons: usize,
    /// Overrides for built-in tool kinds (`execute`, `read`, `edit`, `search`,
    /// `fetch`, `mcp_ui`, `vault`). Set a value to `""` to hide that kind.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub kinds: BTreeMap<String, String>,
    /// Overrides for `execute` command basenames (e.g. `gh`, `openhue`).
    /// Set a value to `""` to suppress that command's icon.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub commands: BTreeMap<String, String>,
}

impl Default for ToolIcons {
    fn default() -> Self {
        Self {
            enabled: false,
            max_icons: 8,
            kinds: BTreeMap::new(),
            commands: BTreeMap::new(),
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
    /// file (appended when no argument mentions it) and `{id}` is the turn ID.
    /// Empty shows the file's path in a notification instead.
    pub viewer: Vec<String>,
    /// Optional command launched when the user clicks `Reply` on a short-answer
    /// notification (`{id}` replaced by turn ID). When empty, opens the viewer
    /// with the follow-up composer focused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reply_command: Vec<String>,
    /// Copies an answer to the clipboard (`hey-computer last --copy`): program
    /// and arguments, fed the raw answer on stdin.
    pub clipboard: Vec<String>,
    /// How many answer files to keep.
    pub keep_answers: usize,
    /// Optional spoken answers and earcon hooks.
    #[serde(default)]
    pub speak: Speak,
    /// Sequential Nerd Font tool/MCP icons in notification titles.
    #[serde(default)]
    pub tool_icons: ToolIcons,
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
            wait_for_keyring: false,
            idle_stop_secs: 0.0,
            inactivity_timeout_secs: 45.0,
            max_turn_secs: 300.0,
            max_tool_calls: 20,
            max_repeat_tool_calls: 3,
            restart_delay_secs: 1.0,
            history_days: 7.0,
            mcp_oauth_tokens_file: PathBuf::new(),
            mcp_servers: None,
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
            reply_command: Vec::new(),
            clipboard: vec!["wl-copy".to_string()],
            keep_answers: 20,
            speak: Speak::default(),
            tool_icons: ToolIcons::default(),
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
        for (key, value) in [
            ("history.retention_days", self.history.retention_days),
            ("history.suppressed_days", self.history.suppressed_days),
        ] {
            if !value.is_finite() || !(0.0..=3650.0).contains(&value) {
                bail!("{key} must be between 0 and 3650, not {value}");
            }
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
        if !self.present.clipboard.is_empty() && self.present.clipboard[0].is_empty() {
            bail!("present.clipboard must name a program");
        }
        if !self.present.reply_command.is_empty() && self.present.reply_command[0].is_empty() {
            bail!("present.reply_command must name a program");
        }
        for (key, cmd) in [
            ("present.speak.command", &self.present.speak.command),
            ("present.speak.command_en", &self.present.speak.command_en),
            ("present.speak.command_de", &self.present.speak.command_de),
            (
                "present.speak.earcon_start",
                &self.present.speak.earcon_start,
            ),
            ("present.speak.earcon_done", &self.present.speak.earcon_done),
            (
                "present.speak.earcon_cancel",
                &self.present.speak.earcon_cancel,
            ),
        ] {
            if !cmd.is_empty() && cmd[0].is_empty() {
                bail!("{key} must name a program");
            }
        }
        if self.present.max_lines == 0 || self.present.max_chars == 0 {
            bail!("present.max_lines and present.max_chars must be positive");
        }
        if !(1..=16).contains(&self.present.tool_icons.max_icons) {
            bail!(
                "present.tool_icons.max_icons must be between 1 and 16, not {}",
                self.present.tool_icons.max_icons
            );
        }
        for (kind, icon) in &self.present.tool_icons.kinds {
            if !crate::icons::VALID_ICON_KINDS.contains(&kind.as_str()) {
                bail!(
                    "present.tool_icons.kinds.{kind}: unknown kind (expected one of: {})",
                    crate::icons::VALID_ICON_KINDS.join(", ")
                );
            }
            validate_icon_value(&format!("present.tool_icons.kinds.{kind}"), icon)?;
        }
        for (cmd, icon) in &self.present.tool_icons.commands {
            if cmd.is_empty()
                || cmd.contains('/')
                || cmd.chars().any(|c| c.is_whitespace() || c.is_control())
            {
                bail!(
                    "present.tool_icons.commands.{cmd:?}: command key must be a non-empty command basename without '/' or whitespace"
                );
            }
            validate_icon_value(&format!("present.tool_icons.commands.{cmd}"), icon)?;
        }
        if !self.agent.mcp_oauth_tokens_file.as_os_str().is_empty()
            && !self.agent.mcp_oauth_tokens_file.is_absolute()
        {
            bail!("agent.mcp_oauth_tokens_file must be an absolute path");
        }
        if !self.instructions.vault.as_os_str().is_empty() && !self.instructions.vault.is_absolute()
        {
            bail!("instructions.vault must be an absolute path");
        }
        for (label, mem_path) in std::iter::once((
            "instructions.memory_file".to_string(),
            &self.instructions.memory_file,
        ))
        .filter(|(_, p)| !p.as_os_str().is_empty())
        .chain(
            self.instructions
                .memory_files
                .iter()
                .enumerate()
                .map(|(i, p)| (format!("instructions.memory_files[{i}]"), p)),
        ) {
            if mem_path.as_os_str().is_empty() || !mem_path.is_absolute() {
                bail!("{label} must be an absolute path");
            }
            if !self.instructions.vault.as_os_str().is_empty() {
                let memory = normalize_lexical(mem_path);
                let vault = normalize_lexical(&self.instructions.vault);
                if memory.starts_with(&vault) {
                    bail!(
                        "{label} must not be inside instructions.vault (the vault is untrusted data)"
                    );
                }
            }
        }
        if let Some(servers) = &self.agent.mcp_servers {
            for server in servers {
                if !self.mcp.contains_key(server) {
                    bail!("agent.mcp_servers references unknown MCP server `{server}`");
                }
            }
        }
        if !(1..=MAX_MEMORY_MAX_CHARS).contains(&self.instructions.memory_max_chars) {
            bail!(
                "instructions.memory_max_chars must be between 1 and {MAX_MEMORY_MAX_CHARS}, not {}",
                self.instructions.memory_max_chars
            );
        }
        for (name, profile) in &self.profiles {
            if name.is_empty()
                || name.len() > MAX_PROFILE_NAME_LEN
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            {
                bail!(
                    "profiles.{name}: profile names must be 1..={MAX_PROFILE_NAME_LEN} ASCII letters, digits, '-' or '_'"
                );
            }
            if name.eq_ignore_ascii_case("default") {
                bail!("profiles.{name}: \"default\" is reserved for the base [agent] settings");
            }
            if let Some(v) = profile.inactivity_timeout_secs
                && (!v.is_finite() || v <= 0.0 || v > MAX_SECS)
            {
                bail!(
                    "profiles.{name}.inactivity_timeout_secs must be positive and at most {MAX_SECS} seconds, not {v}"
                );
            }
            if let Some(v) = profile.max_turn_secs
                && (!v.is_finite() || v < 0.0 || v > MAX_SECS)
            {
                bail!(
                    "profiles.{name}.max_turn_secs must be between 0 and {MAX_SECS} seconds, not {v}"
                );
            }
            if let Some(servers) = &profile.mcp_servers {
                for server in servers {
                    if !self.mcp.contains_key(server) {
                        bail!(
                            "profiles.{name}.mcp_servers references unknown MCP server `{server}`"
                        );
                    }
                }
            }
            if let Some(label) = &profile.thinking_label {
                let trimmed = label.trim();
                if trimmed.is_empty()
                    || trimmed.chars().count() > MAX_THINKING_LABEL_CHARS
                    || trimmed.chars().any(char::is_control)
                {
                    bail!(
                        "profiles.{name}.thinking_label must be 1..={MAX_THINKING_LABEL_CHARS} printable characters"
                    );
                }
            }
        }
        for (name, server) in &self.mcp {
            if name.is_empty()
                || name.len() > MAX_PROFILE_NAME_LEN
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            {
                bail!(
                    "mcp.{name}: server names must be 1..={MAX_PROFILE_NAME_LEN} ASCII letters, digits, '-' or '_'"
                );
            }
            if name.eq_ignore_ascii_case("mcp-ui") {
                bail!("mcp.{name}: \"mcp-ui\" is reserved for hey-computer's internal UI server");
            }
            let has_cmd = !server.command.is_empty();
            let has_url = !server.url.is_empty();
            if has_cmd == has_url {
                bail!("mcp.{name}: set exactly one of `command` (stdio) or `url` (http/sse)");
            }
            if has_cmd {
                if !server.transport.is_empty()
                    || !server.headers.is_empty()
                    || !server.header_files.is_empty()
                {
                    bail!(
                        "mcp.{name}: `transport`, `headers`, and `header_files` only apply to `url` servers"
                    );
                }
            } else {
                if !server.args.is_empty() || !server.env.is_empty() || !server.env_files.is_empty()
                {
                    bail!(
                        "mcp.{name}: `args`, `env`, and `env_files` only apply to `command` servers"
                    );
                }
                if !matches!(server.transport.as_str(), "" | "http" | "sse") {
                    bail!(
                        "mcp.{name}.transport must be \"http\" or \"sse\", not {:?}",
                        server.transport
                    );
                }
                if !is_allowed_mcp_url(&server.url) {
                    bail!(
                        "mcp.{name}.url must use https:// or loopback http:// (127.0.0.1, localhost, [::1])"
                    );
                }
            }
            for (k, path) in &server.env_files {
                if server.env.contains_key(k) {
                    bail!("mcp.{name}: `{k}` cannot be set in both `env` and `env_files`");
                }
                if !path.is_absolute() {
                    bail!("mcp.{name}.env_files.{k} must be an absolute path");
                }
            }
            for (k, path) in &server.header_files {
                if server.headers.contains_key(k) {
                    bail!("mcp.{name}: `{k}` cannot be set in both `headers` and `header_files`");
                }
                if !path.is_absolute() {
                    bail!("mcp.{name}.header_files.{k} must be an absolute path");
                }
            }
            if let Some(icon) = &server.icon {
                validate_icon_value(&format!("mcp.{name}.icon"), icon)?;
            }
            for (pattern, icon) in &server.tool_icons {
                if pattern.trim().is_empty() || pattern.chars().any(char::is_control) {
                    bail!(
                        "mcp.{name}.tool_icons.{pattern:?}: pattern must be non-empty printable characters"
                    );
                }
                validate_icon_value(&format!("mcp.{name}.tool_icons.{pattern}"), icon)?;
            }
        }
        if !(1..=8).contains(&self.tasks.max_concurrent) {
            bail!(
                "tasks.max_concurrent must be between 1 and 8, not {}",
                self.tasks.max_concurrent
            );
        }
        if !(1..=256).contains(&self.tasks.max_queued) {
            bail!(
                "tasks.max_queued must be between 1 and 256, not {}",
                self.tasks.max_queued
            );
        }
        if let Some(profile) = self.tasks.profile.as_deref()
            && let Err(msg) = self.resolve_profile(Some(profile))
        {
            bail!("tasks.profile: {msg}");
        }
        for (key, value) in [
            (
                "scheduler.min_interval_secs",
                self.scheduler.min_interval_secs,
            ),
            (
                "scheduler.script_timeout_secs",
                self.scheduler.script_timeout_secs,
            ),
            (
                "routing.triage_timeout_secs",
                self.routing.triage_timeout_secs,
            ),
        ] {
            if !value.is_finite() || value <= 0.0 || value > MAX_SECS {
                bail!("{key} must be positive and at most {MAX_SECS} seconds, not {value}");
            }
        }
        if !(1..=1024).contains(&self.scheduler.max_jobs) {
            bail!(
                "scheduler.max_jobs must be between 1 and 1024, not {}",
                self.scheduler.max_jobs
            );
        }
        if self.scheduler.jobs.len() > self.scheduler.max_jobs {
            bail!(
                "scheduler.jobs defines {} jobs, exceeding scheduler.max_jobs ({})",
                self.scheduler.jobs.len(),
                self.scheduler.max_jobs
            );
        }
        for (name, job) in &self.scheduler.jobs {
            if name.is_empty()
                || name.len() > 64
                || name.chars().all(|c| c.is_ascii_digit())
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            {
                bail!(
                    "scheduler.jobs.{name}: name must be 1..=64 ASCII letters, digits, '-' or '_' (not all digits)"
                );
            }
            let parsed = crate::scheduler::ScheduleSpec::parse(&job.schedule)
                .with_context(|| format!("scheduler.jobs.{name}.schedule"))?;
            if matches!(parsed, crate::scheduler::ScheduleSpec::InSecs(_)) {
                bail!("scheduler.jobs.{name}.schedule cannot use relative `in ...` in config.toml");
            }
            parsed
                .check_min_interval(self.scheduler.min_interval_secs)
                .with_context(|| format!("scheduler.jobs.{name}.schedule"))?;
            if !job.profile.is_empty()
                && let Err(msg) = self.resolve_profile(Some(&job.profile))
            {
                bail!("scheduler.jobs.{name}.profile: {msg}");
            }
            crate::scheduler::validate_script(&job.script, &self.permissions.deny)
                .with_context(|| format!("scheduler.jobs.{name}.script"))?;
            if job.prompt.trim().is_empty() && job.script.trim().is_empty() {
                bail!("scheduler.jobs.{name}: set `prompt`, `script`, or both");
            }
            if let Some(tok) = &job.suppress_token
                && tok.trim().is_empty()
            {
                bail!("scheduler.jobs.{name}.suppress_token must not be empty");
            }
        }
        if self.routing.enabled {
            if let Err(msg) = self.resolve_profile(Some(&self.routing.fast_profile)) {
                bail!("routing.fast_profile: {msg}");
            }
            if let Err(msg) = self.resolve_profile(Some(&self.routing.deep_profile)) {
                bail!("routing.deep_profile: {msg}");
            }
        }
        Ok(())
    }

    /// Resolve a profile name (`None` or `"default"` means base `[agent]`).
    pub fn resolve_profile(
        &self,
        name: Option<&str>,
    ) -> std::result::Result<ResolvedProfile, String> {
        let base_inactivity =
            secs(self.agent.inactivity_timeout_secs).unwrap_or(Duration::from_secs(45));
        let all_enabled_mcp: Vec<String> = self
            .mcp
            .iter()
            .filter(|(_, s)| s.enabled)
            .map(|(k, _)| k.clone())
            .collect();
        let Some(name) = name else {
            let default_mcp = match &self.agent.mcp_servers {
                None => all_enabled_mcp,
                Some(list) => {
                    let mut chosen: Vec<String> = list
                        .iter()
                        .filter(|s| self.mcp.get(*s).is_some_and(|m| m.enabled))
                        .cloned()
                        .collect();
                    chosen.sort();
                    chosen.dedup();
                    chosen
                }
            };
            return Ok(ResolvedProfile {
                name: None,
                model: self.agent.model.clone(),
                thought_level: self.agent.thought_level.clone(),
                inactivity: base_inactivity,
                max_turn: secs(self.agent.max_turn_secs),
                max_tool_calls: (self.agent.max_tool_calls > 0)
                    .then_some(self.agent.max_tool_calls),
                max_repeat_tool_calls: (self.agent.max_repeat_tool_calls > 0)
                    .then_some(self.agent.max_repeat_tool_calls),
                mcp_servers: default_mcp,
                thinking_label: None,
            });
        };
        if name == "default" {
            return self.resolve_profile(None);
        }
        let Some(profile) = self.profiles.get(name) else {
            let available = if self.profiles.is_empty() {
                "none configured".to_string()
            } else {
                self.profiles.keys().cloned().collect::<Vec<_>>().join(", ")
            };
            return Err(format!("unknown profile `{name}` (available: {available})"));
        };
        let max_tool_calls = profile.max_tool_calls.unwrap_or(self.agent.max_tool_calls);
        let max_repeat_tool_calls = profile
            .max_repeat_tool_calls
            .unwrap_or(self.agent.max_repeat_tool_calls);
        let mcp_servers = match &profile.mcp_servers {
            None => all_enabled_mcp,
            Some(list) => {
                let mut chosen: Vec<String> = list
                    .iter()
                    .filter(|s| self.mcp.get(*s).is_some_and(|m| m.enabled))
                    .cloned()
                    .collect();
                chosen.sort();
                chosen.dedup();
                chosen
            }
        };
        Ok(ResolvedProfile {
            name: Some(name.to_string()),
            model: profile
                .model
                .clone()
                .unwrap_or_else(|| self.agent.model.clone()),
            thought_level: profile
                .thought_level
                .clone()
                .unwrap_or_else(|| self.agent.thought_level.clone()),
            inactivity: profile
                .inactivity_timeout_secs
                .and_then(secs)
                .unwrap_or(base_inactivity),
            max_turn: secs(profile.max_turn_secs.unwrap_or(self.agent.max_turn_secs)),
            max_tool_calls: (max_tool_calls > 0).then_some(max_tool_calls),
            max_repeat_tool_calls: (max_repeat_tool_calls > 0).then_some(max_repeat_tool_calls),
            mcp_servers,
            thinking_label: profile
                .thinking_label
                .as_deref()
                .map(|s| s.trim().to_string()),
        })
    }

    /// `state_dir`, or `$HEY_COMPUTER_STATE_DIR`, or `$XDG_STATE_HOME/hey-computer`.
    pub fn state_dir(&self) -> Result<PathBuf> {
        if !self.state_dir.as_os_str().is_empty() {
            return Ok(self.state_dir.clone());
        }
        if let Some(dir) = std::env::var_os("HEY_COMPUTER_STATE_DIR").filter(|d| !d.is_empty()) {
            return Ok(PathBuf::from(dir));
        }
        dirs::state_dir()
            .map(|d| d.join("hey-computer"))
            .context("no state directory: set state_dir, HEY_COMPUTER_STATE_DIR or XDG_STATE_HOME")
    }

    /// Directory where user-saved widget templates and approved distilled
    /// `SKILL.md` skills live (`$HEY_COMPUTER_SKILLS_DIR`, or
    /// `$XDG_CONFIG_HOME/hey-computer/skills` when `state_dir` is not overridden
    /// to a scratch test directory).
    pub fn user_skills_dir(&self) -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("HEY_COMPUTER_SKILLS_DIR").filter(|d| !d.is_empty()) {
            return Some(PathBuf::from(dir));
        }
        if !self.state_dir.as_os_str().is_empty() {
            // Keep isolated/test configs with an explicit `state_dir` inside
            // `<state_dir>/user-skills` rather than touching `~/.config`.
            return Some(self.state_dir.join("user-skills"));
        }
        dirs::config_dir().map(|d| d.join("hey-computer").join("skills"))
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

fn validate_icon_value(key_path: &str, raw: &str) -> Result<()> {
    if raw.is_empty() {
        return Ok(());
    }
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || raw.chars().any(char::is_control)
        || trimmed.chars().count() > MAX_TOOL_ICON_CHARS
    {
        bail!(
            "{key_path} must be empty (to suppress) or 1..={MAX_TOOL_ICON_CHARS} printable characters"
        );
    }
    Ok(())
}

fn is_allowed_mcp_url(url: &str) -> bool {
    if let Some(rest) = url.strip_prefix("https://") {
        return !rest.is_empty() && !rest.starts_with('/');
    }
    for prefix in ["http://127.0.0.1", "http://localhost", "http://[::1]"] {
        if let Some(rest) = url.strip_prefix(prefix) {
            return rest.is_empty() || matches!(rest.as_bytes()[0], b':' | b'/' | b'?' | b'#');
        }
    }
    false
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

    #[test]
    fn context_config_parses_partially() {
        let default = Config::default();
        assert!(!default.context.time);
        assert!(!default.context.desktop);
        assert!(!default.context.is_enabled());

        let parsed: Config = toml::from_str("[context]\ntime = true\n").unwrap();
        assert!(parsed.context.time);
        assert!(!parsed.context.desktop);
        assert!(parsed.context.is_enabled());
        parsed.validate().unwrap();
    }

    #[test]
    fn clipboard_config_defaults_and_validates() {
        let default = Config::default();
        assert_eq!(default.present.clipboard, vec!["wl-copy"]);
        default.validate().unwrap();

        let mut empty = Config::default();
        empty.present.clipboard.clear();
        empty.validate().unwrap();

        let mut blank = Config::default();
        blank.present.clipboard = vec![String::new()];
        assert!(blank.validate().is_err());
    }

    #[test]
    fn profiles_parse_validate_and_resolve() {
        let parsed: Config = toml::from_str(
            r#"
            [agent]
            model = "fast-model"
            thought_level = "low"
            max_tool_calls = 20
            max_turn_secs = 300

            [profiles.deep]
            model = "deep-model"
            thought_level = "high"
            max_tool_calls = 40
            max_turn_secs = 600
            thinking_label = "Pondering Hard…"

            [profiles.quick]
            thought_level = "low"
            max_tool_calls = 0
            "#,
        )
        .unwrap();
        parsed.validate().unwrap();

        let base = parsed.resolve_profile(None).unwrap();
        assert_eq!(base.name, None);
        assert_eq!(base.model, "fast-model");
        assert_eq!(base.thought_level, "low");
        assert_eq!(base.max_tool_calls, Some(20));

        assert_eq!(parsed.resolve_profile(Some("default")).unwrap(), base);

        let deep = parsed.resolve_profile(Some("deep")).unwrap();
        assert_eq!(deep.name.as_deref(), Some("deep"));
        assert_eq!(deep.model, "deep-model");
        assert_eq!(deep.thought_level, "high");
        assert_eq!(deep.max_tool_calls, Some(40));
        assert_eq!(deep.max_turn, Some(Duration::from_secs(600)));
        assert_eq!(deep.thinking_label.as_deref(), Some("Pondering Hard…"));

        let quick = parsed.resolve_profile(Some("quick")).unwrap();
        assert_eq!(quick.model, "fast-model");
        assert_eq!(quick.max_tool_calls, None);
        assert_eq!(quick.thinking_label, None);

        let err = parsed.resolve_profile(Some("missing")).unwrap_err();
        assert!(err.contains("unknown profile `missing`"), "{err}");
        assert!(err.contains("deep"), "{err}");

        // Reserved "default", bad characters, bad durations, bad labels, and relative OAuth paths are rejected.
        for bad in [
            "[profiles.default]\nthought_level = \"high\"\n",
            "[profiles.\"bad name\"]\nthought_level = \"high\"\n",
            "[profiles.deep]\ninactivity_timeout_secs = 0\n",
            "[profiles.deep]\nmax_turn_secs = -5\n",
            "[profiles.deep]\nthinking_label = \"   \"\n",
            "[profiles.deep]\nthinking_label = \"line1\\nline2\"\n",
            "[agent]\nmcp_oauth_tokens_file = \"relative/tokens.json\"\n",
            "[scheduler]\nmin_interval_secs = 0\n",
            "[scheduler]\nscript_timeout_secs = -1\n",
            "[scheduler]\nmax_jobs = 0\n",
            "[routing]\nenabled = true\nfast_profile = \"missing\"\ndeep_profile = \"default\"\n",
            "[routing]\nenabled = true\nfast_profile = \"default\"\ndeep_profile = \"missing\"\n",
            "[routing]\ntriage_timeout_secs = 0\n",
        ] {
            let cfg: Config = toml::from_str(bad).unwrap();
            assert!(cfg.validate().is_err(), "{bad} should fail validation");
        }
    }

    #[test]
    fn scheduler_routing_and_repeat_guard_defaults_and_validation() {
        let cfg = Config::default();
        assert_eq!(cfg.agent.max_repeat_tool_calls, 3);
        assert!(cfg.scheduler.enabled);
        assert_eq!(cfg.scheduler.min_interval_secs, 60.0);
        assert_eq!(cfg.scheduler.script_timeout_secs, 30.0);
        assert_eq!(cfg.scheduler.max_jobs, 128);
        assert!(!cfg.routing.enabled);
        assert_eq!(cfg.routing.fast_profile, "default");
        assert_eq!(cfg.routing.deep_profile, "deep");
        cfg.validate().unwrap();

        let parsed: Config = toml::from_str(
            r#"
            [agent]
            max_repeat_tool_calls = 4
            [profiles.fast]
            thought_level = "low"
            max_repeat_tool_calls = 0
            [profiles.deep]
            thought_level = "high"
            [routing]
            enabled = true
            fast_profile = "fast"
            deep_profile = "deep"
            triage_timeout_secs = 3.5
            [scheduler]
            enabled = true
            min_interval_secs = 30
            script_timeout_secs = 15
            max_jobs = 64
            "#,
        )
        .unwrap();
        parsed.validate().unwrap();
        assert_eq!(
            parsed.resolve_profile(None).unwrap().max_repeat_tool_calls,
            Some(4)
        );
        assert_eq!(
            parsed
                .resolve_profile(Some("fast"))
                .unwrap()
                .max_repeat_tool_calls,
            None
        );
        assert_eq!(
            parsed
                .resolve_profile(Some("deep"))
                .unwrap()
                .max_repeat_tool_calls,
            Some(4)
        );

        for ok_url in [
            "https://mcp.example.com/mcp",
            "http://localhost",
            "http://localhost:8080/mcp",
            "http://127.0.0.1:3000",
            "http://[::1]:9000/sse",
        ] {
            let cfg: Config = toml::from_str(&format!("[mcp.t]\nurl = {ok_url:?}\n")).unwrap();
            cfg.validate().unwrap_or_else(|e| panic!("{ok_url}: {e}"));
        }
        for bad_url in [
            "http://localhost.evil.example/mcp",
            "http://127.0.0.1.nip.io/mcp",
            "http://localhost@evil.example/mcp",
            "https://",
            "http://example.com/mcp",
        ] {
            let cfg: Config = toml::from_str(&format!("[mcp.t]\nurl = {bad_url:?}\n")).unwrap();
            assert!(cfg.validate().is_err(), "{bad_url} should be rejected");
        }
    }

    #[test]
    fn speak_reply_command_and_reserved_mcp_ui_validate() {
        let cfg = Config::default();
        assert!(!cfg.present.speak.enabled);
        assert!(cfg.present.speak.voice_only);
        assert_eq!(cfg.present.speak.media_duck, MediaDuck::None);
        cfg.validate().unwrap();

        let parsed: Config = toml::from_str(
            r#"
            [present]
            reply_command = ["rofi-ask", "--reply-to", "{id}"]
            [present.speak]
            enabled = true
            voice_only = true
            command_en = ["piper-speak", "en"]
            command_de = ["piper-speak", "de"]
            earcon_start = ["pw-play", "/tmp/start.oga"]
            media_duck = "pause"
            "#,
        )
        .unwrap();
        parsed.validate().unwrap();
        assert_eq!(parsed.present.speak.media_duck, MediaDuck::Pause);

        for bad in [
            "[mcp.mcp-ui]\ncommand = \"/bin/true\"\n",
            "[present]\nreply_command = [\"\"]\n",
            "[present.speak]\ncommand = [\"\"]\n",
            "[present.speak]\nearcon_cancel = [\"\"]\n",
        ] {
            let cfg: Config = toml::from_str(bad).unwrap();
            assert!(cfg.validate().is_err(), "{bad} should fail validation");
        }
    }

    #[test]
    fn keyring_wait_and_tool_icons_defaults_and_validation() {
        let cfg = Config::default();
        assert!(!cfg.agent.wait_for_keyring);
        assert!(!cfg.present.tool_icons.enabled);
        assert_eq!(cfg.present.tool_icons.max_icons, 8);
        assert!(cfg.present.tool_icons.kinds.is_empty());
        assert!(cfg.present.tool_icons.commands.is_empty());
        cfg.validate().unwrap();

        let parsed: Config = toml::from_str(
            r#"
            [agent]
            wait_for_keyring = true

            [mcp.slack]
            command = "/bin/true"
            icon = "\ue8a4"

            [mcp.google-workspace]
            command = "/bin/true"
            icon = ""
            [mcp.google-workspace.tool_icons]
            "*gmail*" = "\uf42f"
            "exact_tool" = ""

            [present.tool_icons]
            enabled = true
            max_icons = 12
            [present.tool_icons.kinds]
            fetch = "\U000f059f"
            read = ""
            [present.tool_icons.commands]
            gh = "\ue709"
            openhue = ""
            "#,
        )
        .unwrap();
        assert!(parsed.agent.wait_for_keyring);
        assert!(parsed.present.tool_icons.enabled);
        assert_eq!(parsed.present.tool_icons.max_icons, 12);
        parsed.validate().unwrap();

        for bad in [
            "[present.tool_icons]\nmax_icons = 0\n",
            "[present.tool_icons]\nmax_icons = 17\n",
            "[present.tool_icons.kinds]\nbogus = \"x\"\n",
            "[present.tool_icons.kinds]\nread = \"a\\nb\"\n",
            "[present.tool_icons.kinds]\nread = \"   \"\n",
            "[present.tool_icons.kinds]\nread = \"12345678901234567\"\n",
            "[present.tool_icons.commands]\n\"/usr/bin/gh\" = \"x\"\n",
            "[present.tool_icons.commands]\n\"gh pr\" = \"x\"\n",
            "[mcp.s]\ncommand = \"/bin/true\"\nicon = \"a\\nb\"\n",
            "[mcp.s]\ncommand = \"/bin/true\"\n[mcp.s.tool_icons]\n\"   \" = \"x\"\n",
            "[mcp.s]\ncommand = \"/bin/true\"\n[mcp.s.tool_icons]\n\"*a*\" = \"12345678901234567\"\n",
        ] {
            let cfg: Config = toml::from_str(bad).unwrap();
            assert!(cfg.validate().is_err(), "{bad} should fail validation");
        }
    }
}
