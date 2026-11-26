//! The deny list: which commands the agent is refused.
//!
//! This is a courtesy, not a boundary. A model that wants to can always find a
//! spelling this does not catch (a variable, base64, a script it writes
//! first). The service's `NoNewPrivileges` stops a setuid program the agent
//! runs itself, but not one it has another process start for it (the systemd
//! user manager, the compositor); see "Permission model" in the README. The
//! list exists so the usual attempt gets a clean refusal the daemon can
//! explain, instead of a password prompt that hangs or a confusing failure.

use std::path::Path;

/// Programs that only run another command, so the word after them (past their
/// options) is the command that matters. `systemd-run` is one: through the
/// user manager its command runs outside the service, where `NoNewPrivileges`
/// no longer holds.
const WRAPPERS: &[&str] = &[
    "env",
    "command",
    "exec",
    "nohup",
    "time",
    "nice",
    "ionice",
    "stdbuf",
    "timeout",
    "xargs",
    "setsid",
    "chrt",
    "taskset",
    "unbuffer",
    "builtin",
    "systemd-run",
];

/// Wrapper options whose value is the next word (`env -u NAME`,
/// `timeout -s KILL 5`, `systemd-run -u NAME`), so that value is not taken
/// for the command.
fn takes_value(wrapper: &str, option: &str) -> bool {
    let options: &[&str] = match wrapper {
        "env" => &["-u", "--unset", "-C", "--chdir"],
        "timeout" => &["-s", "--signal", "-k", "--kill-after"],
        "nice" => &["-n", "--adjustment"],
        "ionice" => &["-c", "--class", "-n", "--classdata", "-p", "--pid"],
        "stdbuf" => &["-i", "-o", "-e"],
        "xargs" => &[
            "-a",
            "-d",
            "-E",
            "-I",
            "-L",
            "-n",
            "-P",
            "-s",
            "--arg-file",
            "--delimiter",
        ],
        "systemd-run" => &[
            "-u",
            "--unit",
            "-p",
            "--property",
            "-E",
            "--setenv",
            "-H",
            "--host",
            "-M",
            "--machine",
            "--description",
            "--slice",
            "--uid",
            "--gid",
            "--nice",
            "--working-directory",
            "--service-type",
            "--on-active",
            "--on-boot",
            "--on-startup",
            "--on-unit-active",
            "--on-unit-inactive",
            "--on-calendar",
            "--timer-property",
            "--path-property",
            "--socket-property",
            "--json",
        ],
        _ => &[],
    };
    options.contains(&option)
}

/// Shells whose `-c` argument is itself a command line.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "fish", "ksh"];

#[derive(Debug, Clone)]
pub struct Policy {
    deny: Vec<String>,
}

impl Policy {
    pub fn new(deny: &[String]) -> Self {
        Self {
            deny: deny.iter().filter(|w| !w.is_empty()).cloned().collect(),
        }
    }

    /// The denied command word `line` uses, if any.
    pub fn denied_word(&self, line: &str) -> Option<String> {
        if self.deny.is_empty() {
            return None;
        }
        command_words(line, 0)
            .into_iter()
            .find(|word| self.deny.iter().any(|d| d == word))
    }
}

/// Decision made by a [`LanePolicy`] for one `session/request_permission`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny {
        /// Short category/word safe to log at `info`.
        tag: String,
        /// Command or tool name included in the refusal report.
        detail: String,
    },
}

/// Tool kinds unconditionally denied in [`LanePolicy::restricted`].
///
/// Only `"read"` and `"think"` remain permitted by kind (further gated by
/// [`RESTRICTED_DENY_NAMES`] and per-turn `max_tool_calls`).
pub const RESTRICTED_DENY_KINDS: &[&str] = &[
    "execute", "edit", "delete", "move", "fetch", "search", "other",
];

/// Built-in tool names unconditionally denied in [`LanePolicy::restricted`],
/// even if a future agent release ever reports their `kind` as `"read"`.
pub const RESTRICTED_DENY_NAMES: &[&str] = &[
    "read_url_content",
    "search_web",
    "open_browser_url",
    "browser_subagent",
    "invoke_subagent",
    "define_subagent",
    "manage_subagents",
    "run_workflow",
    "swarm",
    "schedule",
    "send_message",
    "manage_task",
    "run_command",
    "write_file",
    "edit_file",
];

/// Built-in agent orchestration/subagent tool names unconditionally denied in
/// [`LanePolicy::subagent`] to enforce strictly depth <= 1 and prevent `agy`
/// built-in subagent hangs.
pub const SUBAGENT_DENY_NAMES: &[&str] = &[
    "invoke_subagent",
    "define_subagent",
    "manage_subagents",
    "run_workflow",
    "swarm",
    "send_message",
];

/// Per-lane permission profile hook.
///
/// Interactive and background lanes enforce the configured shell deny list and
/// allow configured MCP servers; restricted lanes (for Phase 6 reactive tasks)
/// additionally refuse every mutating, network, search, subagent, workflow, and
/// unknown tool kind/name and disable MCP; subagent lanes preserve main-agent
/// read/write/shell/MCP parity while blocking `mcp-ui`, `subagents`, and
/// built-in orchestration tools.
#[derive(Debug, Clone)]
pub struct LanePolicy {
    pub deny_commands: Policy,
    pub deny_tool_kinds: Vec<String>,
    pub deny_tool_names: Vec<String>,
    pub allow_mcp: bool,
    /// Whether the internal `mcp-ui` server (`show_widget`, `propose_widget_template`)
    /// is permitted on this lane. Checked before the internal-server early-allow.
    pub allow_mcp_ui: bool,
    /// Whether the internal `subagents` server (`delegate`) is permitted on this lane.
    /// Checked before the internal-server early-allow (`false` on restricted and
    /// subagent lanes so recursion depth is strictly <= 1).
    pub allow_subagents: bool,
    /// Optional per-turn allowlist of MCP server names (`None` allows all
    /// configured servers when `allow_mcp` is true).
    pub allowed_mcp_servers: Option<Vec<String>>,
    /// Optional explicit `"server/tool"` allowlist (used by Stage 2 restricted
    /// notification enrichment to allow strictly read-only calendar/contact
    /// lookups while still denying every unlisted MCP tool and `mcp-ui`).
    pub allowed_mcp_tools: Vec<String>,
    /// Configured MCP server names, used to disambiguate `mcp_<server>_<tool>`
    /// prefixes when server names themselves contain underscores.
    pub known_mcp_servers: Vec<String>,
}

impl LanePolicy {
    pub fn standard(deny: &[String]) -> Self {
        Self {
            deny_commands: Policy::new(deny),
            deny_tool_kinds: Vec::new(),
            deny_tool_names: Vec::new(),
            allow_mcp: true,
            allow_mcp_ui: true,
            allow_subagents: true,
            allowed_mcp_servers: None,
            allowed_mcp_tools: Vec::new(),
            known_mcp_servers: Vec::new(),
        }
    }

    /// Restricted read-only lane: refuses mutating, network, search, and
    /// unknown tool kinds (`RESTRICTED_DENY_KINDS`), blocks risky tool names
    /// (`RESTRICTED_DENY_NAMES`) even if reported as `"read"`, and disables all
    /// MCP servers (including internal `mcp-ui` and `subagents`) unless specific
    /// read-only `"server/tool"` entries are added via [`Self::with_allowed_mcp_tools`].
    pub fn restricted() -> Self {
        Self {
            deny_commands: Policy::new(&[]),
            deny_tool_kinds: RESTRICTED_DENY_KINDS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            deny_tool_names: RESTRICTED_DENY_NAMES
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            allow_mcp: false,
            allow_mcp_ui: false,
            allow_subagents: false,
            allowed_mcp_servers: None,
            allowed_mcp_tools: Vec::new(),
            known_mcp_servers: Vec::new(),
        }
    }

    /// Subagent worker lane: same read/write file, shell (`execute`), web, and
    /// MCP capabilities as the main agent (enforcing `base_deny`), while
    /// structurally blocking `mcp-ui` (`allow_mcp_ui = false`), `subagents`
    /// (`allow_subagents = false`, depth <= 1), and built-in orchestration tools
    /// (`SUBAGENT_DENY_NAMES`).
    ///
    /// When `caller_allowed_mcp_servers` is `Some`, it is intersected with
    /// `profile_mcp_servers` so callers may narrow — never widen — the child
    /// profile's MCP permissions.
    pub fn subagent(
        base_deny: &[String],
        profile_mcp_servers: &[String],
        caller_allowed_mcp_servers: Option<&[String]>,
        known_mcp_servers: Vec<String>,
    ) -> Self {
        let effective_mcp = match caller_allowed_mcp_servers {
            Some(requested) => profile_mcp_servers
                .iter()
                .filter(|s| requested.iter().any(|r| r == *s))
                .cloned()
                .collect(),
            None => profile_mcp_servers.to_vec(),
        };
        Self {
            deny_commands: Policy::new(base_deny),
            deny_tool_kinds: Vec::new(),
            deny_tool_names: SUBAGENT_DENY_NAMES
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            allow_mcp: true,
            allow_mcp_ui: false,
            allow_subagents: false,
            allowed_mcp_servers: Some(effective_mcp),
            allowed_mcp_tools: Vec::new(),
            known_mcp_servers,
        }
    }

    /// Control whether this lane may call the internal `subagents` MCP server.
    pub fn with_subagents(mut self, allow: bool) -> Self {
        self.allow_subagents = allow;
        self
    }

    /// Return a copy of this policy scoped to a specific MCP server allowlist.
    pub fn with_mcp_servers(&self, allowed: Option<Vec<String>>) -> Self {
        let mut copy = self.clone();
        copy.allowed_mcp_servers = allowed;
        copy
    }

    /// Permit an explicit `"server/tool"` MCP subset (e.g. strictly read-only
    /// calendar lookups on a restricted enrichment turn).
    pub fn with_allowed_mcp_tools(mut self, tools: Vec<String>) -> Self {
        if !tools.is_empty() {
            self.allow_mcp = true;
        }
        self.allowed_mcp_tools = tools;
        self
    }

    /// Attach the full set of configured MCP server names for longest-prefix
    /// disambiguation on eager `mcp_<server>_<tool>` tool names.
    pub fn with_known_mcp_servers(mut self, known: Vec<String>) -> Self {
        self.known_mcp_servers = known;
        self
    }

    pub fn evaluate_permission(&self, params: &serde_json::Value) -> PermissionDecision {
        let Some(tool_call) = params.get("toolCall") else {
            return PermissionDecision::Allow;
        };
        self.evaluate_tool_call(tool_call)
    }

    pub fn will_deny_update(&self, update: &serde_json::Value) -> bool {
        matches!(
            self.evaluate_tool_call(update),
            PermissionDecision::Deny { .. }
        )
    }

    /// Return `(tag, detail)` if `update` represents a tool call denied by this policy.
    pub fn denied_update_detail(&self, update: &serde_json::Value) -> Option<(String, String)> {
        match self.evaluate_tool_call(update) {
            PermissionDecision::Deny { tag, detail } => Some((tag, detail)),
            PermissionDecision::Allow => None,
        }
    }

    fn evaluate_tool_call(&self, tool_call: &serde_json::Value) -> PermissionDecision {
        let name = tool_call.get("name").and_then(serde_json::Value::as_str);
        // Handle lazy MCP dispatch tools (`call_mcp_tool`, `list_resources`,
        // `read_resource`) before `deny_tool_kinds` / shell deny-list checks.
        if crate::acp::is_lazy_mcp_tool(name) {
            let server = crate::acp::mcp_server_name(tool_call);
            let tool = crate::acp::mcp_tool_name(tool_call);
            let fallback = name.unwrap_or("call_mcp_tool");
            let detail = match (&server, &tool) {
                (Some(s), Some(t)) => format!("mcp:{s}/{t}"),
                (Some(s), None) if fallback == "call_mcp_tool" => format!("mcp:{s}"),
                (Some(s), None) => format!("mcp:{s}/{fallback}"),
                _ => fallback.to_string(),
            };
            if !self.allow_mcp {
                return PermissionDecision::Deny {
                    tag: "mcp".to_string(),
                    detail,
                };
            }
            if !self.allowed_mcp_tools.is_empty() {
                let allowed_exact = name == Some("call_mcp_tool")
                    && match (&server, &tool) {
                        (Some(s), Some(t)) => {
                            let candidate = format!("{s}/{t}");
                            self.allowed_mcp_tools
                                .iter()
                                .any(|entry| entry == &candidate)
                        }
                        _ => false,
                    };
                if !allowed_exact {
                    return PermissionDecision::Deny {
                        tag: "mcp".to_string(),
                        detail,
                    };
                }
            }
            if server.as_deref() == Some(crate::mcp::INTERNAL_UI_MCP_SERVER) {
                return if self.allow_mcp_ui {
                    PermissionDecision::Allow
                } else {
                    PermissionDecision::Deny {
                        tag: "mcp".to_string(),
                        detail,
                    }
                };
            }
            if server.as_deref() == Some(crate::mcp::INTERNAL_SUBAGENT_MCP_SERVER) {
                return if self.allow_subagents {
                    PermissionDecision::Allow
                } else {
                    PermissionDecision::Deny {
                        tag: "mcp".to_string(),
                        detail,
                    }
                };
            }
            if let Some(allowed) = &self.allowed_mcp_servers
                && !server
                    .as_deref()
                    .is_some_and(|s| allowed.iter().any(|a| a == s))
            {
                return PermissionDecision::Deny {
                    tag: "mcp".to_string(),
                    detail,
                };
            }
            return PermissionDecision::Allow;
        }
        if let Some(mcp_name) = name.filter(|n| n.starts_with("mcp_")) {
            if !self.allow_mcp {
                return PermissionDecision::Deny {
                    tag: "mcp".to_string(),
                    detail: mcp_name.to_string(),
                };
            }
            if !self.allowed_mcp_tools.is_empty() {
                let matched = self.allowed_mcp_tools.iter().any(|entry| {
                    entry
                        .split_once('/')
                        .is_some_and(|(s, t)| mcp_name == format!("mcp_{s}_{t}"))
                });
                if !matched {
                    return PermissionDecision::Deny {
                        tag: "mcp".to_string(),
                        detail: mcp_name.to_string(),
                    };
                }
            }
            if mcp_name.starts_with(&format!("mcp_{}_", crate::mcp::INTERNAL_UI_MCP_SERVER)) {
                return if self.allow_mcp_ui {
                    PermissionDecision::Allow
                } else {
                    PermissionDecision::Deny {
                        tag: "mcp".to_string(),
                        detail: mcp_name.to_string(),
                    }
                };
            }
            if mcp_name.starts_with(&format!(
                "mcp_{}_",
                crate::mcp::INTERNAL_SUBAGENT_MCP_SERVER
            )) {
                return if self.allow_subagents {
                    PermissionDecision::Allow
                } else {
                    PermissionDecision::Deny {
                        tag: "mcp".to_string(),
                        detail: mcp_name.to_string(),
                    }
                };
            }
            if let Some(allowed) = &self.allowed_mcp_servers {
                let is_allowed = if !self.known_mcp_servers.is_empty() {
                    let matched_server = self
                        .known_mcp_servers
                        .iter()
                        .filter(|s| mcp_name.starts_with(&format!("mcp_{s}_")))
                        .max_by_key(|s| s.len());
                    matched_server.is_some_and(|srv| allowed.iter().any(|a| a == srv))
                } else {
                    allowed
                        .iter()
                        .any(|s| mcp_name.starts_with(&format!("mcp_{s}_")))
                };
                if !is_allowed {
                    return PermissionDecision::Deny {
                        tag: "mcp".to_string(),
                        detail: mcp_name.to_string(),
                    };
                }
            }
            return PermissionDecision::Allow;
        }

        let kind = tool_call
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("other");
        let command = crate::acp::tool_command(tool_call);
        if self.deny_tool_kinds.iter().any(|k| k == kind) {
            let detail = command
                .or_else(|| {
                    tool_call
                        .get("title")
                        .and_then(serde_json::Value::as_str)
                        .map(String::from)
                })
                .or_else(|| name.map(String::from))
                .unwrap_or_else(|| kind.to_string());
            return PermissionDecision::Deny {
                tag: kind.to_string(),
                detail,
            };
        }
        if let Some(tool_name) = name
            && self
                .deny_tool_names
                .iter()
                .any(|denied| denied.eq_ignore_ascii_case(tool_name))
        {
            let detail = command
                .or_else(|| {
                    tool_call
                        .get("title")
                        .and_then(serde_json::Value::as_str)
                        .map(String::from)
                })
                .unwrap_or_else(|| tool_name.to_string());
            return PermissionDecision::Deny {
                tag: tool_name.to_string(),
                detail,
            };
        }
        if let Some(cmd) = command
            && let Some(word) = self.deny_commands.denied_word(&cmd)
        {
            return PermissionDecision::Deny {
                tag: word,
                detail: cmd,
            };
        }
        PermissionDecision::Allow
    }
}

/// The words in command position in a shell line: the first word of every
/// simple command, past variable assignments and wrappers, as base names.
/// Recurses into `sh -c '…'` and `$( … )`, a few levels deep.
pub fn command_words(line: &str, depth: usize) -> Vec<String> {
    let mut found = Vec::new();
    if depth > 4 {
        return found;
    }
    for segment in split_commands(line) {
        let words = split_words(&segment);
        let mut i = 0;
        // Assignments before the command: `FOO=1 sudo …`.
        while i < words.len() && is_assignment(&words[i]) {
            i += 1;
        }
        // Wrappers, with their options and (for timeout, nice -n) arguments.
        while i < words.len() {
            let name = base_name(&words[i]);
            if !WRAPPERS.contains(&name.as_str()) {
                break;
            }
            found.push(name.clone());
            i += 1;
            // Options, assignments (`env`) and numbers (`timeout 5`,
            // `nice -n 5`) belong to the wrapper.
            while i < words.len()
                && (words[i].starts_with('-')
                    || is_assignment(&words[i])
                    || looks_like_duration(&words[i]))
            {
                if words[i] == "--" {
                    i += 1;
                    break;
                }
                if name == "env" {
                    if words[i] == "-S" || words[i] == "--split-string" {
                        if let Some(split_arg) = words.get(i + 1) {
                            found.extend(command_words(split_arg, depth + 1));
                        }
                        i += 2;
                        continue;
                    }
                    if let Some(rest) = words[i]
                        .strip_prefix("--split-string=")
                        .or_else(|| words[i].strip_prefix("-S").filter(|r| !r.is_empty()))
                    {
                        found.extend(command_words(rest, depth + 1));
                        i += 1;
                        continue;
                    }
                }
                if takes_value(&name, &words[i]) {
                    i += 1;
                }
                i += 1;
            }
        }
        let Some(word) = words.get(i) else {
            continue;
        };
        let name = base_name(word);
        if SHELLS.contains(&name.as_str()) {
            for script in shell_scripts(&name, &words[i + 1..]) {
                found.extend(command_words(script, depth + 1));
            }
        }
        found.push(name);
    }
    for inner in substitutions(line) {
        found.extend(command_words(&inner, depth + 1));
    }
    found
}

/// Command strings passed to a shell invocation via `-c` (including combined
/// short flags such as `-lc`, `-ec`, `-ce`, `-xec`), and for `fish` also
/// `--command`, `-C`, and `--init-command`.
fn shell_scripts<'a>(shell: &str, args: &'a [String]) -> Vec<&'a str> {
    if shell == "fish" {
        return fish_scripts(args);
    }
    let mut saw_c = false;
    let mut j = 0;
    while j < args.len() {
        let arg = args[j].as_str();
        if arg == "--" || arg == "-" {
            j += 1;
            break;
        }
        if arg.starts_with("--") {
            if matches!(arg, "--rcfile" | "--init-file") {
                j += 1;
            }
            j += 1;
            continue;
        }
        if (arg.starts_with('-') || arg.starts_with('+')) && arg.len() > 1 {
            let flags = &arg[1..];
            if arg.starts_with('-') && flags.contains('c') {
                saw_c = true;
            }
            // `-o OPT` / `+o OPT` (and bash's `-O OPT` / `+O OPT`) consume the
            // next word as the option name; leave it alone if it looks like
            // another flag (e.g. `sh -oerrexit -c …`).
            let takes_opt_name = flags.contains('o') || (shell == "bash" && flags.contains('O'));
            if takes_opt_name
                && let Some(next) = args.get(j + 1)
                && !next.starts_with(['-', '+'])
            {
                j += 1;
            }
            j += 1;
            continue;
        }
        break;
    }
    if saw_c && let Some(script) = args.get(j) {
        vec![script.as_str()]
    } else {
        Vec::new()
    }
}

fn fish_scripts(args: &[String]) -> Vec<&str> {
    let mut scripts = Vec::new();
    let mut j = 0;
    while j < args.len() {
        let arg = args[j].as_str();
        if arg == "--" || arg == "-" {
            break;
        }
        if let Some(script) = arg
            .strip_prefix("--command=")
            .or_else(|| arg.strip_prefix("--init-command="))
        {
            scripts.push(script);
            j += 1;
            continue;
        }
        if matches!(arg, "--command" | "--init-command") {
            if let Some(script) = args.get(j + 1) {
                scripts.push(script.as_str());
            }
            j += 2;
            continue;
        }
        if arg.starts_with("--") {
            if matches!(
                arg,
                "--features" | "--profile" | "--profile-startup" | "--debug" | "--debug-output"
            ) {
                j += 1;
            }
            j += 1;
            continue;
        }
        if let Some(flags) = arg.strip_prefix('-')
            && !flags.is_empty()
        {
            let mut consumed_next = false;
            for (idx, ch) in flags.char_indices() {
                let rest = &flags[idx + ch.len_utf8()..];
                match ch {
                    'c' | 'C' => {
                        if !rest.is_empty() {
                            scripts.push(rest);
                        } else if let Some(script) = args.get(j + 1) {
                            scripts.push(script.as_str());
                            consumed_next = true;
                        }
                        break;
                    }
                    'f' | 'p' | 'd' | 'o' => {
                        if rest.is_empty() {
                            consumed_next = true;
                        }
                        break;
                    }
                    _ => {}
                }
            }
            j += if consumed_next { 2 } else { 1 };
            continue;
        }
        break;
    }
    scripts
}

fn is_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !name.starts_with(|c: char| c.is_ascii_digit())
        }
        None => false,
    }
}

fn looks_like_duration(word: &str) -> bool {
    let digits = word.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(word);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn base_name(word: &str) -> String {
    Path::new(word)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| word.to_string())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SubContext {
    CommandSub { parens: usize },
    Double,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    Single,
    Double,
    AnsiC,
}

fn next_char(line: &str, pos: usize) -> usize {
    line[pos..]
        .chars()
        .next()
        .map_or(line.len(), |c| pos + c.len_utf8())
}

fn skip_escaped(line: &str, backslash_pos: usize) -> usize {
    let after = backslash_pos + 1;
    if after < line.len() {
        next_char(line, after)
    } else {
        line.len()
    }
}

/// Skip past the closing `'` of a single-quoted string (`start` is after the
/// opening `'`). Backslashes are not special inside `'…'`.
fn skip_single_quote(line: &str, start: usize) -> usize {
    match line.as_bytes()[start..].iter().position(|&b| b == b'\'') {
        Some(rel) => start + rel + 1,
        None => line.len(),
    }
}

/// Skip past the closing `'` of an ANSI-C `$'…'` string (`start` is after `$'`).
fn skip_ansi_c_quote(line: &str, start: usize) -> usize {
    let bytes = line.as_bytes();
    let mut pos = start;
    while pos < bytes.len() {
        match bytes[pos] {
            b'\\' => pos = skip_escaped(line, pos),
            b'\'' => return pos + 1,
            _ => pos = next_char(line, pos),
        }
    }
    line.len()
}

/// Scan from right after an opening `` ` `` to its matching unescaped `` ` ``
/// (or EOF), returning the one-level-unescaped command text and the byte index
/// after the closing `` ` ``.
fn scan_backtick(line: &str, start: usize, in_double: bool) -> (String, usize) {
    let mut inner = String::new();
    let bytes = line.as_bytes();
    let mut pos = start;
    while pos < bytes.len() {
        match bytes[pos] {
            b'`' => return (inner, pos + 1),
            b'\\' if pos + 1 < bytes.len() => {
                let next = bytes[pos + 1];
                if matches!(next, b'$' | b'`' | b'\\') || (in_double && next == b'"') {
                    inner.push(next as char);
                    pos += 2;
                } else {
                    inner.push('\\');
                    pos += 1;
                }
            }
            _ => {
                let Some(ch) = line[pos..].chars().next() else {
                    break;
                };
                inner.push(ch);
                pos += ch.len_utf8();
            }
        }
    }
    (inner, pos)
}

/// Scan from right after `$(` to its matching `)` (respecting nested quotes,
/// `$'…'`, backticks, escapes, subshells, and nested `$(…)`). Returns
/// `(content_end, next_pos)`.
fn scan_command_sub(line: &str, start: usize) -> (usize, usize) {
    let bytes = line.as_bytes();
    let mut stack = vec![SubContext::CommandSub { parens: 1 }];
    let mut pos = start;
    while pos < bytes.len() {
        let Some(top) = stack.last_mut() else {
            break;
        };
        match top {
            SubContext::Double => match bytes[pos] {
                b'"' => {
                    stack.pop();
                    pos += 1;
                }
                b'\\' => pos = skip_escaped(line, pos),
                b'$' if bytes.get(pos + 1) == Some(&b'(') => {
                    stack.push(SubContext::CommandSub { parens: 1 });
                    pos += 2;
                }
                b'`' => {
                    let (_, next) = scan_backtick(line, pos + 1, true);
                    pos = next;
                }
                _ => pos = next_char(line, pos),
            },
            SubContext::CommandSub { parens } => match bytes[pos] {
                b'\\' => pos = skip_escaped(line, pos),
                b'\'' => pos = skip_single_quote(line, pos + 1),
                b'$' if bytes.get(pos + 1) == Some(&b'\'') => {
                    pos = skip_ansi_c_quote(line, pos + 2);
                }
                b'$' if bytes.get(pos + 1) == Some(&b'(') => {
                    *parens += 1;
                    pos += 2;
                }
                b'"' => {
                    stack.push(SubContext::Double);
                    pos += 1;
                }
                b'`' => {
                    let (_, next) = scan_backtick(line, pos + 1, false);
                    pos = next;
                }
                b'(' => {
                    *parens += 1;
                    pos += 1;
                }
                b')' => {
                    *parens -= 1;
                    if *parens == 0 {
                        stack.pop();
                        if stack.is_empty() {
                            return (pos, pos + 1);
                        }
                    }
                    pos += 1;
                }
                _ => pos = next_char(line, pos),
            },
        }
    }
    (line.len(), line.len())
}

/// Split at `;`, `&`, `|`, newlines and parentheses outside quotes.
fn split_commands(line: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut in_double = false;
    let bytes = line.as_bytes();
    let mut pos = 0;
    while pos < bytes.len() {
        if in_double {
            match bytes[pos] {
                b'"' => {
                    in_double = false;
                    current.push('"');
                    pos += 1;
                }
                b'\\' => {
                    let next = skip_escaped(line, pos);
                    current.push_str(&line[pos..next]);
                    pos = next;
                }
                b'$' if bytes.get(pos + 1) == Some(&b'(') => {
                    let (_, next) = scan_command_sub(line, pos + 2);
                    current.push_str(&line[pos..next]);
                    pos = next;
                }
                b'`' => {
                    let (_, next) = scan_backtick(line, pos + 1, true);
                    current.push_str(&line[pos..next]);
                    pos = next;
                }
                _ => {
                    let next = next_char(line, pos);
                    current.push_str(&line[pos..next]);
                    pos = next;
                }
            }
        } else {
            match bytes[pos] {
                b'\'' => {
                    let next = skip_single_quote(line, pos + 1);
                    current.push_str(&line[pos..next]);
                    pos = next;
                }
                b'$' if bytes.get(pos + 1) == Some(&b'\'') => {
                    let next = skip_ansi_c_quote(line, pos + 2);
                    current.push_str(&line[pos..next]);
                    pos = next;
                }
                b'"' => {
                    in_double = true;
                    current.push('"');
                    pos += 1;
                }
                b'\\' => {
                    let next = skip_escaped(line, pos);
                    current.push_str(&line[pos..next]);
                    pos = next;
                }
                b'$' if bytes.get(pos + 1) == Some(&b'(') => {
                    let (_, next) = scan_command_sub(line, pos + 2);
                    current.push_str(&line[pos..next]);
                    pos = next;
                }
                b'`' => {
                    let (_, next) = scan_backtick(line, pos + 1, false);
                    current.push_str(&line[pos..next]);
                    pos = next;
                }
                b';' | b'&' | b'|' | b'\n' | b'(' | b')' | b'{' | b'}' => {
                    segments.push(std::mem::take(&mut current));
                    pos += 1;
                }
                _ => {
                    let next = next_char(line, pos);
                    current.push_str(&line[pos..next]);
                    pos = next;
                }
            }
        }
    }
    segments.push(current);
    segments.retain(|s| !s.trim().is_empty());
    segments
}

/// Words of one simple command, with quotes removed.
fn split_words(segment: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quote: Option<Quote> = None;
    let mut chars = segment.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(Quote::Single), '\'') => quote = None,
            (Some(Quote::Single), c) => current.push(c),
            (Some(Quote::AnsiC), '\'') => quote = None,
            (Some(Quote::AnsiC), '\\') => {
                if let Some(next) = chars.next() {
                    match next {
                        'n' => current.push('\n'),
                        'r' => current.push('\r'),
                        't' => current.push('\t'),
                        '\'' => current.push('\''),
                        '"' => current.push('"'),
                        '\\' => current.push('\\'),
                        other => {
                            current.push('\\');
                            current.push(other);
                        }
                    }
                }
            }
            (Some(Quote::AnsiC), c) => current.push(c),
            (Some(Quote::Double), '"') => quote = None,
            (Some(Quote::Double), '\\') => {
                if let Some(next) = chars.next() {
                    match next {
                        '$' | '`' | '"' | '\\' => current.push(next),
                        '\n' => {}
                        other => {
                            current.push('\\');
                            current.push(other);
                        }
                    }
                }
            }
            (Some(Quote::Double), c) => current.push(c),
            (None, '$') if chars.peek() == Some(&'\'') => {
                chars.next();
                quote = Some(Quote::AnsiC);
                in_word = true;
            }
            (None, '$') if chars.peek() == Some(&'"') => {
                chars.next();
                quote = Some(Quote::Double);
                in_word = true;
            }
            (None, '\'') => {
                quote = Some(Quote::Single);
                in_word = true;
            }
            (None, '"') => {
                quote = Some(Quote::Double);
                in_word = true;
            }
            (None, '\\') => {
                if let Some(next) = chars.next() {
                    if next != '\n' {
                        current.push(next);
                        in_word = true;
                    }
                } else {
                    in_word = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            (None, c) => {
                current.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(current);
    }
    words
}

/// The insides of `$( … )` and backtick substitutions outside single quotes.
fn substitutions(line: &str) -> Vec<String> {
    let mut found = Vec::new();
    let bytes = line.as_bytes();
    let mut in_double = false;
    let mut pos = 0;
    while pos < bytes.len() {
        if in_double {
            match bytes[pos] {
                b'"' => {
                    in_double = false;
                    pos += 1;
                }
                b'\\' => pos = skip_escaped(line, pos),
                b'$' if bytes.get(pos + 1) == Some(&b'(') => {
                    let start = pos + 2;
                    let (end, next) = scan_command_sub(line, start);
                    found.push(line[start..end].to_string());
                    pos = next;
                }
                b'`' => {
                    let (inner, next) = scan_backtick(line, pos + 1, true);
                    found.push(inner);
                    pos = next;
                }
                _ => pos = next_char(line, pos),
            }
        } else {
            match bytes[pos] {
                b'\\' => pos = skip_escaped(line, pos),
                b'\'' => pos = skip_single_quote(line, pos + 1),
                b'$' if bytes.get(pos + 1) == Some(&b'\'') => {
                    pos = skip_ansi_c_quote(line, pos + 2);
                }
                b'"' => {
                    in_double = true;
                    pos += 1;
                }
                b'$' if bytes.get(pos + 1) == Some(&b'(') => {
                    let start = pos + 2;
                    let (end, next) = scan_command_sub(line, start);
                    found.push(line[start..end].to_string());
                    pos = next;
                }
                b'`' => {
                    let (inner, next) = scan_backtick(line, pos + 1, false);
                    found.push(inner);
                    pos = next;
                }
                _ => pos = next_char(line, pos),
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy::new(&["sudo", "su", "doas", "pkexec", "run0"].map(String::from))
    }

    #[test]
    fn refuses_privilege_escalation_in_command_position() {
        let p = policy();
        for line in [
            "sudo -n true",
            "/run/wrappers/bin/sudo systemctl restart foo",
            "FOO=1 sudo ls",
            "env sudo ls",
            "env -i PATH=/bin sudo ls",
            "timeout 5 sudo ls",
            "nice -n 5 pkexec true",
            "true && sudo ls",
            "ls | sudo tee /etc/x",
            "echo hi; su -c id",
            "bash -c 'sudo ls'",
            "sh -c \"doas id\"",
            "echo $(sudo cat /etc/shadow)",
            "(run0 id)",
            "`sudo id`",
            // Out of the service through the user manager, where
            // NoNewPrivileges does not hold.
            "systemd-run --user --pipe --wait sudo -n true",
            "systemd-run --user --wait --pipe --quiet --collect -u probe sudo -n true",
            "systemd-run --user -p NoNewPrivileges=no --unit=x -- sudo id",
            "systemd-run --user --on-active=10m sh -c 'sudo reboot'",
            "env -u HOME sudo ls",
            "timeout -s KILL 5 sudo ls",
        ] {
            assert!(p.denied_word(line).is_some(), "{line}");
        }
    }

    #[test]
    fn allows_ordinary_commands_that_mention_the_words() {
        let p = policy();
        for line in [
            "hyprctl version",
            "man sudo",
            "echo 'sudo is not available here'",
            "grep -r su notes/",
            "obsidian-cli search query=sudoku",
            "playerctl play-pause",
            "cat ~/notes/sudo.md",
            "git log --author=su",
            "systemd-run --user --on-active=10m notify-send Tea",
            "systemd-run --user -u su-reminder --on-active=5m notify-send Tea",
        ] {
            assert_eq!(p.denied_word(line), None, "{line}");
        }
    }

    #[test]
    fn an_empty_list_denies_nothing() {
        assert_eq!(Policy::new(&[]).denied_word("sudo ls"), None);
    }

    #[test]
    fn refuses_combined_and_complex_shell_c_flags() {
        let p = policy();
        for line in [
            "bash -lc 'sudo id'",
            "sh -ec 'sudo id'",
            "sh -ce 'sudo id'",
            "bash -xec 'sudo id'",
            "bash -euc 'sudo id'",
            "zsh -lc 'sudo id'",
            "dash -ec 'sudo id'",
            "ksh -lc 'sudo id'",
            "/bin/bash -lc 'sudo id'",
            "env -i bash -lc 'sudo id'",
            "systemd-run --user --wait -- bash -lc 'sudo id'",
            "bash --login -c 'sudo id'",
            "bash --norc --noprofile -lc 'sudo id'",
            "bash -o errexit -c 'sudo id'",
            "bash -euo pipefail -c 'sudo id'",
            "bash +o nounset -c 'sudo id'",
            "bash -O extglob -c 'sudo id'",
            "bash +O extglob -lc 'sudo id'",
            "bash -co errexit 'sudo id'",
            "bash -oc errexit 'sudo id'",
            "sh -oerrexit -c 'sudo id'",
            "bash --rcfile /dev/null -c 'sudo id'",
            "bash --init-file /dev/null -lc 'sudo id'",
            "bash -c -e 'sudo id'",
            "bash -c -- 'sudo id'",
            "bash -lc -- 'sudo id'",
            "sh -c - 'sudo id'",
            "bash -lc \"sh -ec 'sudo id'\"",
            "bash -c $'sudo id'",
            "bash -c $'true\\nsudo id'",
            "bash -c \"echo \\' && sudo id\"",
            "fish -lc 'sudo id'",
            "fish -c'sudo id'",
            "fish --command 'sudo id'",
            "fish --command='sudo id'",
            "fish -C 'sudo id' -c 'echo ok'",
            "fish --init-command='sudo id'",
        ] {
            assert!(p.denied_word(line).is_some(), "{line}");
        }
    }

    #[test]
    fn allows_shell_invocations_without_denied_c_script() {
        let p = policy();
        for line in [
            "bash -- -c 'sudo id'",
            "sh - -c 'sudo id'",
            "bash script.sh -c 'sudo id'",
            "bash script.sh -lc 'sudo id'",
            "fish -- -c 'sudo id'",
            "fish script.fish -c 'sudo id'",
            "bash -c 'echo ok' -- 'sudo id'",
            "bash -c ''",
            "bash -c",
        ] {
            assert_eq!(p.denied_word(line), None, "{line}");
        }
    }

    #[test]
    fn refuses_substitutions_across_quotes_and_escapes() {
        let p = policy();
        for line in [
            "echo \"$(sudo id)\"",
            "echo \"foo'bar $(sudo id)\"",
            "echo \"foo\\\"'$(sudo id)'\"",
            "echo \"\\\\$(sudo id)\"",
            "echo '\\\\'$(sudo id)",
            "echo \\'$(sudo id)\\'",
            "bash -c \"echo '$(sudo id)'\"",
            "echo \"$(echo ')'; sudo id)\"",
            "echo \"$(echo \")\"; sudo id)\"",
            "echo \"$(echo \\); sudo id)\"",
            "echo \"$(echo '\"')\"; sudo id",
            "echo \"$(echo \"a $(echo \")\")\"; sudo id)\"",
            "echo \"$( (echo ok); sudo id )\"",
            "echo \"`sudo id`\"",
            "echo \"\\\\`sudo id`\"",
            "echo \"`bash -lc \\\"sudo id\\\"`\"",
            "echo \"`echo \\`sudo id\\``\"",
            "echo $'a\\'b' && sudo id",
            "echo $'a\\'b' $(sudo id)",
            "echo $'a\\\\' $(sudo id)",
            "echo \\ä $(sudo id)",
            "echo \"Grüße $(sudo id)\"",
        ] {
            assert!(p.denied_word(line).is_some(), "{line}");
        }
    }

    #[test]
    fn allows_literal_substitutions_in_quotes_or_escaped() {
        let p = policy();
        for line in [
            "echo '$(sudo id)'",
            "echo '`sudo id`'",
            "echo $'$(sudo id)'",
            "echo $'`sudo id`'",
            "echo \"\\$(sudo id)\"",
            "echo \"\\`sudo id\\`\"",
            "echo \\`sudo id\\`",
            "echo \"\\\\\"'$(sudo id)'",
            "echo \"$(echo '$(sudo id)')\"",
            "bash -c \"echo '\\$(sudo id)'\"",
            "bash -lc 'echo '\\''$(sudo id)'\\'''",
            "echo 'Grüße $(sudo id)'",
        ] {
            assert_eq!(p.denied_word(line), None, "{line}");
        }
    }

    #[test]
    fn words_in_command_position() {
        assert_eq!(
            command_words("A=1 env B=2 /usr/bin/hyprctl dispatch exec firefox", 0),
            ["env", "hyprctl"]
        );
        assert_eq!(command_words("ls -l | wc -l", 0), ["ls", "wc"]);
        assert_eq!(
            command_words("systemd-run --user --scope -u v -- env A=1 marker f", 0),
            ["systemd-run", "env", "marker"]
        );
    }

    #[test]
    fn lane_policy_restricted_blocks_mutating_kinds_and_mcp_only() {
        use serde_json::json;

        let std_lane = LanePolicy::standard(&["sudo".to_string()]);
        assert_eq!(
            std_lane.evaluate_permission(&json!({
                "toolCall": {"kind": "edit", "title": "Edit note.md"}
            })),
            PermissionDecision::Allow
        );
        assert!(matches!(
            std_lane.evaluate_permission(&json!({
                "toolCall": {"kind": "execute", "rawInput": {"CommandLine": "sudo id"}}
            })),
            PermissionDecision::Deny { ref tag, .. } if tag == "sudo"
        ));

        let restricted = LanePolicy::restricted();
        assert!(!restricted.allow_mcp);
        for blocked in [
            json!({"toolCall": {"kind": "execute", "rawInput": {"CommandLine": "ls"}}}),
            json!({"toolCall": {"kind": "edit", "title": "notes.md"}}),
            json!({"toolCall": {"kind": "delete", "title": "notes.md"}}),
            json!({"toolCall": {"kind": "move", "title": "notes.md"}}),
            json!({"toolCall": {"kind": "fetch", "title": "Fetch https://example.com"}}),
            json!({"toolCall": {"kind": "search", "title": "grep"}}),
            json!({"toolCall": {"kind": "other", "title": "Unknown action"}}),
            json!({"toolCall": {"title": "Missing kind defaults to other"}}),
            json!({"toolCall": {"kind": "read", "name": "read_url_content", "title": "Fetch URL"}}),
            json!({"toolCall": {"kind": "read", "name": "search_web", "title": "Search web"}}),
            json!({"toolCall": {"kind": "read", "name": "open_browser_url", "title": "Open URL"}}),
            json!({"toolCall": {"kind": "read", "name": "browser_subagent"}}),
            json!({"toolCall": {"kind": "read", "name": "invoke_subagent"}}),
            json!({"toolCall": {"kind": "read", "name": "define_subagent"}}),
            json!({"toolCall": {"kind": "read", "name": "manage_subagents"}}),
            json!({"toolCall": {"kind": "read", "name": "run_workflow"}}),
            json!({"toolCall": {"kind": "read", "name": "swarm"}}),
            json!({"toolCall": {"kind": "read", "name": "schedule"}}),
            json!({"toolCall": {"kind": "read", "name": "send_message"}}),
            json!({"toolCall": {"kind": "read", "name": "manage_task"}}),
            json!({"toolCall": {"kind": "read", "name": "run_command"}}),
            json!({"toolCall": {"kind": "read", "name": "write_file"}}),
            json!({"toolCall": {"kind": "read", "name": "edit_file"}}),
            json!({"toolCall": {"kind": "other", "name": "mcp_slack_post"}}),
            json!({"toolCall": {
                "kind": "execute",
                "name": "call_mcp_tool",
                "rawInput": {"ServerName": "slack", "ToolName": "conversations_history"}
            }}),
        ] {
            assert!(
                matches!(
                    restricted.evaluate_permission(&blocked),
                    PermissionDecision::Deny { .. }
                ),
                "{blocked} should be denied in restricted lane"
            );
            assert!(
                restricted.will_deny_update(&blocked["toolCall"]),
                "will_deny_update must deny {blocked} in restricted lane"
            );
        }
        // Per-profile MCP allowlists gate `call_mcp_tool` by ServerName.
        let scoped = std_lane.with_mcp_servers(Some(vec!["nixos".to_string()]));
        assert_eq!(
            scoped.evaluate_permission(&json!({"toolCall": {
                "kind": "execute",
                "name": "call_mcp_tool",
                "rawInput": {"ServerName": "nixos", "ToolName": "nix"}
            }})),
            PermissionDecision::Allow
        );
        assert!(matches!(
            scoped.evaluate_permission(&json!({"toolCall": {
                "kind": "execute",
                "name": "call_mcp_tool",
                "rawInput": {"ServerName": "slack", "ToolName": "conversations_history"}
            }})),
            PermissionDecision::Deny { ref tag, ref detail }
                if tag == "mcp" && detail == "mcp:slack/conversations_history"
        ));
        for allowed in [
            json!({"toolCall": {"kind": "read", "name": "read_file", "title": "notes.md"}}),
            json!({"toolCall": {"kind": "think", "title": "reasoning"}}),
        ] {
            assert_eq!(
                restricted.evaluate_permission(&allowed),
                PermissionDecision::Allow
            );
            assert!(!restricted.will_deny_update(&allowed["toolCall"]));
        }

        // Lazy MCP resource reads (`list_resources`, `read_resource`) are gated
        // both in restricted lanes and by per-profile `mcp_servers`.
        for res_tool in ["list_resources", "read_resource"] {
            assert!(matches!(
                restricted.evaluate_permission(&json!({"toolCall": {
                    "kind": "read",
                    "name": res_tool,
                    "rawInput": {"ServerName": "nixos"}
                }})),
                PermissionDecision::Deny { ref tag, .. } if tag == "mcp"
            ));
            assert_eq!(
                scoped.evaluate_permission(&json!({"toolCall": {
                    "kind": "read",
                    "name": res_tool,
                    "rawInput": {"ServerName": "nixos"}
                }})),
                PermissionDecision::Allow
            );
            assert!(matches!(
                scoped.evaluate_permission(&json!({"toolCall": {
                    "kind": "read",
                    "name": res_tool,
                    "rawInput": {"ServerName": "slack"}
                }})),
                PermissionDecision::Deny { ref tag, .. } if tag == "mcp"
            ));
        }

        // Longest-prefix disambiguation prevents `git` from allowing `git_admin`.
        let known = std_lane
            .with_known_mcp_servers(vec!["git".to_string(), "git_admin".to_string()])
            .with_mcp_servers(Some(vec!["git".to_string()]));
        assert_eq!(
            known.evaluate_permission(&json!({
                "toolCall": {"kind": "other", "name": "mcp_git_status"}
            })),
            PermissionDecision::Allow
        );
        assert!(matches!(
            known.evaluate_permission(&json!({
                "toolCall": {"kind": "other", "name": "mcp_git_admin_drop"}
            })),
            PermissionDecision::Deny { ref tag, .. } if tag == "mcp"
        ));

        // Internal `mcp-ui` server is always allowed in non-restricted lanes (even
        // when a profile sets `mcp_servers = []` or scopes to another server),
        // and always denied in restricted lanes (`allow_mcp == false`).
        let std_lane_fresh = LanePolicy::standard(&["sudo".to_string()]);
        let empty_mcp = std_lane_fresh.with_mcp_servers(Some(vec![]));
        for ui_call in [
            json!({"toolCall": {
                "kind": "execute",
                "name": "call_mcp_tool",
                "rawInput": {"ServerName": "mcp-ui", "ToolName": "show_widget"}
            }}),
            json!({"toolCall": {
                "kind": "other",
                "name": "mcp_mcp-ui_show_widget",
                "rawInput": {"kind": "transit_board", "title": "Departures", "data": {}}
            }}),
            json!({"toolCall": {
                "kind": "other",
                "name": "mcp_mcp-ui_propose_widget_template",
                "rawInput": {"skill_name": "widget-demo"}
            }}),
        ] {
            assert_eq!(
                std_lane_fresh.evaluate_permission(&ui_call),
                PermissionDecision::Allow
            );
            assert_eq!(
                scoped.evaluate_permission(&ui_call),
                PermissionDecision::Allow
            );
            assert_eq!(
                empty_mcp.evaluate_permission(&ui_call),
                PermissionDecision::Allow
            );
            assert!(
                matches!(
                    restricted.evaluate_permission(&ui_call),
                    PermissionDecision::Deny { ref tag, .. } if tag == "mcp"
                ),
                "{ui_call} must be denied in restricted lane"
            );
        }

        // `env -S` / `--split-string` inner commands are checked, and arguments
        // after unquoted substitutions are not misparsed as command words.
        let p = Policy::new(&["sudo".to_string()]);
        assert_eq!(p.denied_word("env -S 'sudo id'").as_deref(), Some("sudo"));
        assert_eq!(p.denied_word("env -S sudo id").as_deref(), Some("sudo"));
        assert_eq!(
            p.denied_word("env --split-string='sudo id'").as_deref(),
            Some("sudo")
        );
        assert_eq!(p.denied_word("echo $(date) sudo"), None);
        assert_eq!(p.denied_word("echo `date` sudo"), None);
        assert_eq!(p.denied_word("echo $(sudo id)").as_deref(), Some("sudo"));

        // Restricted lane with explicit `allowed_mcp_tools` allows ONLY exact read-only tools.
        let restricted_cal = LanePolicy::restricted()
            .with_known_mcp_servers(vec!["google-workspace".to_string()])
            .with_allowed_mcp_tools(vec!["google-workspace/get_events".to_string()]);
        assert_eq!(
            restricted_cal.evaluate_permission(&json!({"toolCall": {
                "kind": "other",
                "name": "call_mcp_tool",
                "rawInput": {"ServerName": "google-workspace", "ToolName": "get_events"}
            }})),
            PermissionDecision::Allow
        );
        assert_eq!(
            restricted_cal.evaluate_permission(&json!({"toolCall": {
                "kind": "other",
                "name": "mcp_google-workspace_get_events"
            }})),
            PermissionDecision::Allow
        );
        for blocked in [
            json!({"toolCall": {
                "kind": "other",
                "name": "call_mcp_tool",
                "rawInput": {"ServerName": "google-workspace", "ToolName": "send_gmail_message"}
            }}),
            json!({"toolCall": {
                "kind": "execute",
                "name": "call_mcp_tool",
                "rawInput": {"ServerName": "mcp-ui", "ToolName": "show_widget"}
            }}),
            json!({"toolCall": {
                "kind": "execute",
                "name": "call_mcp_tool",
                "rawInput": {"ServerName": "subagents", "ToolName": "delegate"}
            }}),
            json!({"toolCall": {
                "kind": "read",
                "name": "list_resources",
                "rawInput": {"ServerName": "google-workspace"}
            }}),
            json!({"toolCall": {
                "kind": "execute",
                "name": "run_command",
                "rawInput": {"CommandLine": "id"}
            }}),
        ] {
            assert!(
                matches!(
                    restricted_cal.evaluate_permission(&blocked),
                    PermissionDecision::Deny { .. }
                ),
                "expected {blocked} to be denied"
            );
        }
    }

    #[test]
    fn lane_policy_subagent_has_rw_parity_blocks_ui_and_recursion_and_intersects_mcp() {
        use serde_json::json;

        // Standard lane allows `subagents` even with an explicit `mcp_servers` allowlist,
        // and blocks it when `with_subagents(false)` is set.
        let std_scoped = LanePolicy::standard(&["sudo".to_string()])
            .with_subagents(true)
            .with_mcp_servers(Some(vec!["nixos".to_string()]));
        let std_disabled = LanePolicy::standard(&["sudo".to_string()])
            .with_subagents(false)
            .with_mcp_servers(Some(vec!["nixos".to_string()]));
        for sub_call in [
            json!({"toolCall": {
                "kind": "execute",
                "name": "call_mcp_tool",
                "rawInput": {"ServerName": "subagents", "ToolName": "delegate"}
            }}),
            json!({"toolCall": {
                "kind": "other",
                "name": "mcp_subagents_delegate"
            }}),
        ] {
            assert_eq!(
                std_scoped.evaluate_permission(&sub_call),
                PermissionDecision::Allow,
                "standard lane with subagents enabled should allow {sub_call}"
            );
            assert!(
                matches!(
                    std_disabled.evaluate_permission(&sub_call),
                    PermissionDecision::Deny { ref tag, .. } if tag == "mcp"
                ),
                "standard lane with subagents disabled must deny {sub_call}"
            );
        }

        // Subagent lane: intersects caller allowlist with profile allowlist.
        let profile_mcp = vec![
            "google-workspace".to_string(),
            "slack".to_string(),
            "linear".to_string(),
        ];
        let caller_mcp = vec![
            "google-workspace".to_string(),
            "unconfigured-extra".to_string(),
        ];
        let sub_policy = LanePolicy::subagent(
            &["sudo".to_string(), "pkexec".to_string()],
            &profile_mcp,
            Some(&caller_mcp),
            vec![
                "google-workspace".to_string(),
                "slack".to_string(),
                "linear".to_string(),
                "mcp-ui".to_string(),
                "subagents".to_string(),
            ],
        );
        assert!(sub_policy.allow_mcp);
        assert!(!sub_policy.allow_mcp_ui);
        assert!(!sub_policy.allow_subagents);
        assert_eq!(
            sub_policy.allowed_mcp_servers.as_deref(),
            Some(&["google-workspace".to_string()][..])
        );

        // Allowed in subagent lane: read/write/edit/execute/web + intersected MCP server.
        for allowed in [
            json!({"toolCall": {"kind": "read", "name": "read_file", "title": "note.md"}}),
            json!({"toolCall": {"kind": "edit", "name": "write_file", "title": "note.md"}}),
            json!({"toolCall": {"kind": "edit", "name": "edit_file", "title": "note.md"}}),
            json!({"toolCall": {"kind": "execute", "name": "run_command", "rawInput": {"CommandLine": "rg TODO notes/"}}}),
            json!({"toolCall": {"kind": "fetch", "name": "read_url_content"}}),
            json!({"toolCall": {"kind": "search", "name": "search_web"}}),
            json!({"toolCall": {
                "kind": "execute",
                "name": "call_mcp_tool",
                "rawInput": {"ServerName": "google-workspace", "ToolName": "get_events"}
            }}),
            json!({"toolCall": {
                "kind": "other",
                "name": "mcp_google-workspace_get_events"
            }}),
        ] {
            assert_eq!(
                sub_policy.evaluate_permission(&allowed),
                PermissionDecision::Allow,
                "expected subagent policy to allow {allowed}"
            );
        }

        // Denied in subagent lane: permissions.deny, mcp-ui, subagents, built-in orchestration,
        // and MCP servers outside the intersection.
        for blocked in [
            json!({"toolCall": {"kind": "execute", "name": "run_command", "rawInput": {"CommandLine": "sudo id"}}}),
            json!({"toolCall": {"kind": "execute", "name": "run_command", "rawInput": {"CommandLine": "pkexec sh"}}}),
            json!({"toolCall": {"kind": "execute", "name": "call_mcp_tool", "rawInput": {"ServerName": "mcp-ui", "ToolName": "show_widget"}}}),
            json!({"toolCall": {"kind": "other", "name": "mcp_mcp-ui_show_widget"}}),
            json!({"toolCall": {"kind": "execute", "name": "call_mcp_tool", "rawInput": {"ServerName": "subagents", "ToolName": "delegate"}}}),
            json!({"toolCall": {"kind": "other", "name": "mcp_subagents_delegate"}}),
            json!({"toolCall": {"kind": "other", "name": "invoke_subagent"}}),
            json!({"toolCall": {"kind": "other", "name": "define_subagent"}}),
            json!({"toolCall": {"kind": "other", "name": "manage_subagents"}}),
            json!({"toolCall": {"kind": "other", "name": "run_workflow"}}),
            json!({"toolCall": {"kind": "other", "name": "swarm"}}),
            json!({"toolCall": {"kind": "other", "name": "send_message"}}),
            json!({"toolCall": {"kind": "execute", "name": "call_mcp_tool", "rawInput": {"ServerName": "slack", "ToolName": "conversations_history"}}}),
            json!({"toolCall": {"kind": "other", "name": "mcp_slack_conversations_history"}}),
            json!({"toolCall": {"kind": "execute", "name": "call_mcp_tool", "rawInput": {"ServerName": "unconfigured-extra", "ToolName": "ping"}}}),
        ] {
            assert!(
                matches!(
                    sub_policy.evaluate_permission(&blocked),
                    PermissionDecision::Deny { .. }
                ),
                "expected subagent policy to deny {blocked}"
            );
        }
    }
}
