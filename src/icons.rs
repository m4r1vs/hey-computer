//! Sequential Nerd Font v3 tool and MCP server icons for notification titles.
//!
//! Resolves ACP `tool_call`, `tool_call_update`, and `request_permission`
//! objects into deduplicated, first-use-ordered Nerd Font glyphs while holding
//! argument-less `agy` stubs in a `Pending` state until arguments arrive and
//! suppressing internal `<agent_dir>/.../mcp/` and `SKILL.md` reads.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::acp;
use crate::config::{Config, normalize_lexical};
use crate::mcp::INTERNAL_UI_MCP_SERVER;
use crate::policy;

/// Valid keys under `[present.tool_icons.kinds]`.
pub const VALID_ICON_KINDS: &[&str] = &[
    "execute", "read", "edit", "search", "fetch", "mcp_ui", "vault",
];

/// Default Nerd Font v3 glyphs for known MCP server names.
pub const DEFAULT_MCP_ICONS: &[(&str, &str)] = &[
    ("slack", "\u{e8a4}"),            // nf-dev-slack
    ("linear", "\u{f145}"),           // nf-fa-ticket
    ("gitlab", "\u{e7eb}"),           // nf-dev-gitlab
    ("github", "\u{e709}"),           // nf-dev-github
    ("nixos", "\u{f313}"),            // nf-linux-nixos
    ("context7", "\u{f405}"),         // nf-oct-book
    ("google-workspace", "\u{f1a0}"), // nf-fa-google
];

/// Default per-tool patterns for `google-workspace` MCP tools.
pub const GOOGLE_WORKSPACE_TOOL_ICONS: &[(&str, &str)] = &[
    ("*gmail*", "\u{f42f}"),              // nf-oct-mail
    ("*drive*", "\u{f2df}"),              // nf-fa-google_drive
    ("import_to_google_*", "\u{f2df}"),   // nf-fa-google_drive
    ("*doc*", "\u{f2df}"),                // nf-fa-google_drive
    ("*sheet*", "\u{f2df}"),              // nf-fa-google_drive
    ("*slide*", "\u{f2df}"),              // nf-fa-google_drive
    ("*calendar*", "\u{f455}"),           // nf-oct-calendar
    ("*event*", "\u{f455}"),              // nf-oct-calendar
    ("query_freebusy", "\u{f455}"),       // nf-oct-calendar
    ("manage_focus_time", "\u{f455}"),    // nf-oct-calendar
    ("manage_out_of_office", "\u{f455}"), // nf-oct-calendar
    ("*task*", "\u{f4a0}"),               // nf-oct-tasklist
    ("*contact*", "\u{f415}"),            // nf-oct-person
];

/// Default Nerd Font v3 glyphs for recognized shell command basenames.
pub const DEFAULT_COMMAND_ICONS: &[(&str, &str)] = &[
    ("gh", "\u{e709}"),            // nf-dev-github
    ("obsidian", "\u{f082e}"),     // nf-md-notebook
    ("openhue", "\u{f0eb}"),       // nf-fa-lightbulb_o
    ("hue-lights", "\u{f0eb}"),    // nf-fa-lightbulb_o
    ("transit", "\u{f052c}"),      // nf-md-train
    ("yt-dlp", "\u{f16a}"),        // nf-fa-youtube_play
    ("gcalcli", "\u{f455}"),       // nf-oct-calendar
    ("swaync-client", "\u{f0f3}"), // nf-fa-bell
];

/// Default Nerd Font v3 glyphs for built-in tool kinds.
pub const DEFAULT_KIND_ICONS: &[(&str, &str)] = &[
    ("fetch", "\u{f059f}"),  // nf-md-web
    ("search", "\u{f002}"),  // nf-fa-search
    ("execute", "\u{f489}"), // nf-oct-terminal
    ("read", "\u{f4a5}"),    // nf-oct-file
    ("edit", "\u{f040}"),    // nf-fa-pencil
    ("vault", "\u{f082e}"),  // nf-md-notebook
    ("mcp_ui", "\u{f1355}"), // nf-md-widgets_outline
];

/// Resolved icon lookup table built from [`Config`] and the active `agent_dir`.
#[derive(Debug, Clone)]
pub struct IconResolver {
    enabled: bool,
    vault: Option<PathBuf>,
    agent_dir: Option<PathBuf>,
    skill_roots: Vec<PathBuf>,
    known_mcp_servers: Vec<String>,
    mcp_icons: BTreeMap<String, String>,
    user_mcp_tool_rules: BTreeMap<String, Vec<(String, String)>>,
    default_mcp_tool_rules: BTreeMap<String, Vec<(String, String)>>,
    kind_icons: BTreeMap<String, String>,
    command_icons: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    Mcp,
    Execute,
    Read,
    Edit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Resolution {
    Resolved(Vec<String>),
    Pending(PendingKind),
}

impl IconResolver {
    pub fn from_config(config: &Config, agent_dir: Option<&Path>) -> Self {
        let vault = (!config.instructions.vault.as_os_str().is_empty())
            .then(|| normalize_lexical(&config.instructions.vault));
        let agent_dir = agent_dir.map(normalize_lexical);
        let mut skill_roots: Vec<PathBuf> = config
            .instructions
            .skills
            .iter()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| normalize_lexical(p))
            .collect();
        if let Some(user_skills) = config.user_skills_dir() {
            skill_roots.push(normalize_lexical(&user_skills));
        }

        let known_mcp_servers: Vec<String> = config.mcp.keys().cloned().collect();

        let mut mcp_icons: BTreeMap<String, String> = DEFAULT_MCP_ICONS
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        let mut user_mcp_tool_rules: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for (name, server) in &config.mcp {
            if let Some(icon) = &server.icon {
                mcp_icons.insert(name.clone(), icon.trim().to_string());
            }
            if !server.tool_icons.is_empty() {
                let rules = server
                    .tool_icons
                    .iter()
                    .map(|(pat, glyph)| (pat.trim().to_string(), glyph.trim().to_string()))
                    .collect();
                user_mcp_tool_rules.insert(name.clone(), rules);
            }
        }

        let mut default_mcp_tool_rules: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        default_mcp_tool_rules.insert(
            "google-workspace".to_string(),
            GOOGLE_WORKSPACE_TOOL_ICONS
                .iter()
                .map(|(pat, glyph)| ((*pat).to_string(), (*glyph).to_string()))
                .collect(),
        );

        let mut kind_icons: BTreeMap<String, String> = DEFAULT_KIND_ICONS
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        for (k, v) in &config.present.tool_icons.kinds {
            kind_icons.insert(k.clone(), v.trim().to_string());
        }

        let mut command_icons: BTreeMap<String, String> = DEFAULT_COMMAND_ICONS
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        for (k, v) in &config.present.tool_icons.commands {
            command_icons.insert(k.clone(), v.trim().to_string());
        }

        Self {
            enabled: config.present.tool_icons.enabled,
            vault,
            agent_dir,
            skill_roots,
            known_mcp_servers,
            mcp_icons,
            user_mcp_tool_rules,
            default_mcp_tool_rules,
            kind_icons,
            command_icons,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn kind_icon(&self, kind: &str) -> Vec<String> {
        match self.kind_icons.get(kind).map(String::as_str) {
            Some("") | None => Vec::new(),
            Some(glyph) => vec![glyph.to_string()],
        }
    }

    fn resolve_mcp(&self, server: &str, tool: Option<&str>) -> Vec<String> {
        if server == INTERNAL_UI_MCP_SERVER {
            return self.kind_icon("mcp_ui");
        }
        if let Some(tool_name) = tool {
            if let Some(rules) = self.user_mcp_tool_rules.get(server)
                && let Some(glyph) = match_tool_icon(rules, tool_name)
            {
                return if glyph.is_empty() {
                    Vec::new()
                } else {
                    vec![glyph.to_string()]
                };
            }
            if let Some(rules) = self.default_mcp_tool_rules.get(server)
                && let Some(glyph) = match_tool_icon(rules, tool_name)
            {
                return if glyph.is_empty() {
                    Vec::new()
                } else {
                    vec![glyph.to_string()]
                };
            }
        }
        match self.mcp_icons.get(server).map(String::as_str) {
            Some("") | None => Vec::new(),
            Some(glyph) => vec![glyph.to_string()],
        }
    }

    fn resolve_command(&self, cmd: &str) -> Vec<String> {
        let words = policy::command_words(cmd, 0);
        let mut matched_any = false;
        let mut icons = Vec::new();
        for word in words {
            if let Some(glyph) = self.command_icons.get(&word) {
                matched_any = true;
                if !glyph.is_empty() && !icons.contains(glyph) {
                    icons.push(glyph.clone());
                }
            }
        }
        if matched_any {
            icons
        } else {
            self.kind_icon("execute")
        }
    }

    fn resolve_path(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            normalize_lexical(path)
        } else if let Some(ad) = &self.agent_dir {
            normalize_lexical(&ad.join("workspace").join(path))
        } else {
            normalize_lexical(path)
        }
    }

    /// Suppresses reads of internal `<agent_dir>/.../mcp/` or `skills/` files
    /// (outside `<agent_dir>/workspace`) and `SKILL.md` files under configured
    /// skill roots.
    pub fn is_internal_instruction_read(&self, path: &Path) -> bool {
        let norm = self.resolve_path(path);
        if let Some(agent_dir) = &self.agent_dir
            && let Ok(rel) = norm.strip_prefix(agent_dir)
        {
            let mut comps = rel.components();
            let first = comps.next().and_then(|c| c.as_os_str().to_str());
            if first != Some("workspace") {
                if norm.file_name().and_then(|f| f.to_str()) == Some("SKILL.md") {
                    return true;
                }
                if rel
                    .components()
                    .any(|c| matches!(c.as_os_str().to_str(), Some("mcp" | "mcp-ui" | "skills")))
                {
                    return true;
                }
            }
        }
        if norm.file_name().and_then(|f| f.to_str()) == Some("SKILL.md")
            && self.skill_roots.iter().any(|root| norm.starts_with(root))
        {
            return true;
        }
        false
    }

    /// Whether `path` lies inside configured `instructions.vault`.
    pub fn is_vault_path(&self, path: &Path) -> bool {
        let Some(vault) = &self.vault else {
            return false;
        };
        self.resolve_path(path).starts_with(vault)
    }

    fn resolve_call(&self, call: &Value) -> Resolution {
        if !self.enabled {
            return Resolution::Resolved(Vec::new());
        }
        let name = call.get("name").and_then(Value::as_str);
        let kind = call.get("kind").and_then(Value::as_str);

        if acp::is_lazy_mcp_tool(name) {
            return match acp::resolve_mcp_call(call, &self.known_mcp_servers) {
                Some((server, tool)) => {
                    Resolution::Resolved(self.resolve_mcp(&server, tool.as_deref()))
                }
                None => Resolution::Pending(PendingKind::Mcp),
            };
        }

        if name.is_some_and(|n| n.starts_with("mcp_")) {
            return match acp::resolve_mcp_call(call, &self.known_mcp_servers) {
                Some((server, tool)) => {
                    Resolution::Resolved(self.resolve_mcp(&server, tool.as_deref()))
                }
                None => Resolution::Resolved(Vec::new()),
            };
        }

        if kind == Some("fetch") || name == Some("read_url_content") {
            return Resolution::Resolved(self.kind_icon("fetch"));
        }

        if kind == Some("search") || name == Some("search_web") {
            return Resolution::Resolved(self.kind_icon("search"));
        }

        if kind == Some("think") {
            return Resolution::Resolved(Vec::new());
        }

        if kind == Some("execute") || name == Some("run_command") {
            if let Some(cmd) = extract_non_stub_command(call) {
                return Resolution::Resolved(self.resolve_command(&cmd));
            }
            return Resolution::Pending(PendingKind::Execute);
        }

        if kind == Some("read") || matches!(name, Some("read_file" | "list_dir")) {
            if let Some(path) = extract_file_path(call) {
                if self.is_internal_instruction_read(&path) {
                    return Resolution::Resolved(Vec::new());
                }
                if self.is_vault_path(&path) {
                    return Resolution::Resolved(self.kind_icon("vault"));
                }
                return Resolution::Resolved(self.kind_icon("read"));
            }
            return Resolution::Pending(PendingKind::Read);
        }

        if matches!(kind, Some("edit" | "delete" | "move"))
            || matches!(name, Some("write_file" | "edit_file"))
        {
            if let Some(path) = extract_file_path(call) {
                if self.is_vault_path(&path) {
                    return Resolution::Resolved(self.kind_icon("vault"));
                }
                return Resolution::Resolved(self.kind_icon("edit"));
            }
            return Resolution::Pending(PendingKind::Edit);
        }

        Resolution::Resolved(Vec::new())
    }

    fn resolve_pending(&self, pending: PendingKind, call: &Value) -> Option<Vec<String>> {
        match pending {
            PendingKind::Mcp => {
                let server = extract_raw_field(call, &["ServerName", "serverName"])?;
                let tool = extract_raw_field(call, &["ToolName", "toolName"]);
                Some(self.resolve_mcp(&server, tool.as_deref()))
            }
            PendingKind::Execute => {
                let cmd = extract_command_any(call)?;
                Some(self.resolve_command(&cmd))
            }
            PendingKind::Read => {
                let path = extract_file_path(call)?;
                if self.is_internal_instruction_read(&path) {
                    Some(Vec::new())
                } else if self.is_vault_path(&path) {
                    Some(self.kind_icon("vault"))
                } else {
                    Some(self.kind_icon("read"))
                }
            }
            PendingKind::Edit => {
                let path = extract_file_path(call)?;
                if self.is_vault_path(&path) {
                    Some(self.kind_icon("vault"))
                } else {
                    Some(self.kind_icon("edit"))
                }
            }
        }
    }

    fn finalize_pending(&self, pending: PendingKind) -> Vec<String> {
        match pending {
            PendingKind::Mcp => Vec::new(),
            PendingKind::Execute => self.kind_icon("execute"),
            PendingKind::Read => self.kind_icon("read"),
            PendingKind::Edit => self.kind_icon("edit"),
        }
    }
}

/// Per-turn icon tracker that deduplicates icons in first-use order and holds
/// argument-less `agy` `tool_call` stubs until `request_permission` or
/// `tool_call_update` supplies concrete arguments.
#[derive(Debug, Default)]
pub struct IconTracker {
    icons: Vec<String>,
    seen_icons: HashSet<String>,
    pending_calls: HashMap<String, PendingKind>,
    resolved_calls: HashSet<String>,
}

impl IconTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe an initial `tool_call` update. Returns `true` if a new icon was added.
    pub fn observe_update(&mut self, resolver: &IconResolver, call: &Value) -> bool {
        self.observe(resolver, call, false)
    }

    /// Observe a `tool_call_update` follow-up. Returns `true` if a new icon was added.
    pub fn observe_followup(&mut self, resolver: &IconResolver, call: &Value) -> bool {
        self.observe(resolver, call, true)
    }

    /// Observe an allowed `session/request_permission` `toolCall`. Returns `true`
    /// if a new icon was added.
    pub fn observe_permission(&mut self, resolver: &IconResolver, call: Option<&Value>) -> bool {
        let Some(call) = call else {
            return false;
        };
        self.observe(resolver, call, false)
    }

    pub fn icons(&self) -> &[String] {
        &self.icons
    }

    pub fn into_icons(self) -> Vec<String> {
        self.icons
    }

    fn observe(&mut self, resolver: &IconResolver, call: &Value, is_followup: bool) -> bool {
        if !resolver.is_enabled() {
            return false;
        }
        let tc_id = acp::tool_call_id(call);
        if let Some(id) = tc_id
            && self.resolved_calls.contains(id)
        {
            return false;
        }

        let status = call.get("status").and_then(Value::as_str);
        let is_completed = status == Some("completed");
        let is_failed = matches!(status, Some("failed" | "cancelled"));

        // If this `toolCallId` was held pending from an earlier stub, try resolving
        // directly from any newly supplied arguments even when `tool_call_update`
        // omits `name` / `kind`.
        if let Some(id) = tc_id
            && let Some(&pending) = self.pending_calls.get(id)
        {
            if let Some(glyphs) = resolver.resolve_pending(pending, call) {
                self.pending_calls.remove(id);
                self.resolved_calls.insert(id.to_string());
                return self.record_glyphs(glyphs);
            }
            if is_completed {
                self.pending_calls.remove(id);
                self.resolved_calls.insert(id.to_string());
                let fallback = resolver.finalize_pending(pending);
                return self.record_glyphs(fallback);
            }
            if is_failed {
                self.pending_calls.remove(id);
                self.resolved_calls.insert(id.to_string());
                return false;
            }
        }

        match resolver.resolve_call(call) {
            Resolution::Resolved(glyphs) => {
                // Ignore empty follow-up updates for untracked calls so we don't
                // mark an unrelated ID resolved prematurely.
                if is_followup
                    && glyphs.is_empty()
                    && call.get("name").is_none()
                    && call.get("kind").is_none()
                {
                    return false;
                }
                if let Some(id) = tc_id {
                    self.pending_calls.remove(id);
                    self.resolved_calls.insert(id.to_string());
                }
                self.record_glyphs(glyphs)
            }
            Resolution::Pending(pending) => {
                let Some(id) = tc_id else {
                    if is_failed {
                        return false;
                    }
                    let fallback = resolver.finalize_pending(pending);
                    return self.record_glyphs(fallback);
                };
                if is_completed {
                    self.pending_calls.remove(id);
                    self.resolved_calls.insert(id.to_string());
                    let fallback = resolver.finalize_pending(pending);
                    self.record_glyphs(fallback)
                } else if is_failed {
                    self.pending_calls.remove(id);
                    self.resolved_calls.insert(id.to_string());
                    false
                } else {
                    self.pending_calls.insert(id.to_string(), pending);
                    false
                }
            }
        }
    }

    fn record_glyphs(&mut self, glyphs: Vec<String>) -> bool {
        let mut changed = false;
        for glyph in glyphs {
            if !glyph.is_empty() && self.seen_icons.insert(glyph.clone()) {
                self.icons.push(glyph);
                changed = true;
            }
        }
        changed
    }
}

/// Match an MCP tool name against glob patterns (`exact`, `prefix*`, `*suffix`,
/// `*substring*`), returning the highest-specificity match's glyph.
pub fn match_tool_icon<'a>(rules: &'a [(String, String)], tool_name: &str) -> Option<&'a str> {
    let mut best: Option<((u8, usize), &'a str)> = None;
    for (pattern, glyph) in rules {
        let Some(score) = pattern_match_score(pattern, tool_name) else {
            continue;
        };
        if best.is_none_or(|(prev, _)| score > prev) {
            best = Some((score, glyph.as_str()));
        }
    }
    best.map(|(_, glyph)| glyph)
}

/// Returns `Some((tier, literal_len))` if `pattern` matches `tool_name`:
/// - tier 3: exact match
/// - tier 2: prefix (`foo*`) or suffix (`*bar`)
/// - tier 1: substring (`*mid*`)
pub fn matches_pattern(pattern: &str, tool_name: &str) -> bool {
    pattern_match_score(pattern, tool_name).is_some()
}

fn pattern_match_score(pattern: &str, tool_name: &str) -> Option<(u8, usize)> {
    let pat = pattern.trim().to_ascii_lowercase();
    if pat.is_empty() {
        return None;
    }
    let name = tool_name.to_ascii_lowercase();
    let starts_star = pat.starts_with('*');
    let ends_star = pat.ends_with('*');
    match (starts_star, ends_star) {
        (false, false) => (name == pat).then_some((3, pat.len())),
        (false, true) => {
            let literal = pat.trim_end_matches('*');
            name.starts_with(literal).then_some((2, literal.len()))
        }
        (true, false) => {
            let literal = pat.trim_start_matches('*');
            name.ends_with(literal).then_some((2, literal.len()))
        }
        (true, true) => {
            let literal = pat.trim_matches('*');
            name.contains(literal).then_some((1, literal.len()))
        }
    }
}

fn extract_raw_field(call: &Value, names: &[&str]) -> Option<String> {
    for container in ["rawInput", "args"] {
        let Some(input) = call.get(container) else {
            continue;
        };
        for field in names {
            if let Some(s) = input.get(*field).and_then(Value::as_str)
                && !s.is_empty()
            {
                return Some(s.to_string());
            }
        }
    }
    None
}

fn extract_non_stub_command(call: &Value) -> Option<String> {
    let cmd = acp::tool_command(call)?;
    let trimmed = cmd.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("execute")
        || trimmed.eq_ignore_ascii_case("run_command")
        || trimmed.eq_ignore_ascii_case("running command…")
    {
        return None;
    }
    Some(trimmed.to_string())
}

fn extract_command_any(call: &Value) -> Option<String> {
    if let Some(cmd) = extract_non_stub_command(call) {
        return Some(cmd);
    }
    for key in ["rawInput", "args"] {
        if let Some(line) = call
            .get(key)
            .and_then(|input| input.get("CommandLine").or_else(|| input.get("command")))
        {
            match line {
                Value::String(s) if !s.trim().is_empty() => return Some(s.trim().to_string()),
                Value::Array(words) => {
                    let joined: Vec<&str> = words.iter().filter_map(Value::as_str).collect();
                    if !joined.is_empty() {
                        return Some(joined.join(" "));
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn extract_file_path(call: &Value) -> Option<PathBuf> {
    const PATH_KEYS: &[&str] = &[
        "Path",
        "path",
        "AbsolutePath",
        "absolutePath",
        "TargetFile",
        "targetFile",
        "FilePath",
        "filePath",
        "DirectoryPath",
        "directoryPath",
        "SearchPath",
        "searchPath",
        "SourcePath",
        "sourcePath",
        "DestPath",
        "destPath",
    ];
    if let Some(p) = extract_raw_field(call, PATH_KEYS) {
        return Some(PathBuf::from(p));
    }
    if let Some(locs) = call.get("locations").and_then(Value::as_array) {
        for loc in locs {
            if let Some(p) = loc.get("path").and_then(Value::as_str)
                && !p.is_empty()
            {
                return Some(PathBuf::from(p));
            }
        }
    }
    if let Some(title) = call.get("title").and_then(Value::as_str) {
        let trimmed = title.trim();
        if trimmed.starts_with('/') {
            return Some(PathBuf::from(trimmed));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::config::McpServer;

    fn enabled_config() -> Config {
        let mut cfg = Config::default();
        cfg.present.tool_icons.enabled = true;
        cfg.instructions.vault = PathBuf::from("/home/user/Vault");
        cfg.instructions.skills = vec![PathBuf::from("/home/user/skills")];
        cfg.mcp.insert(
            "slack".to_string(),
            McpServer {
                command: "/bin/true".to_string(),
                ..McpServer::default()
            },
        );
        cfg.mcp.insert(
            "google-workspace".to_string(),
            McpServer {
                command: "/bin/true".to_string(),
                ..McpServer::default()
            },
        );
        cfg.mcp.insert(
            "linear".to_string(),
            McpServer {
                command: "/bin/true".to_string(),
                ..McpServer::default()
            },
        );
        cfg.mcp.insert(
            "gitlab".to_string(),
            McpServer {
                command: "/bin/true".to_string(),
                ..McpServer::default()
            },
        );
        cfg
    }

    #[test]
    fn maps_exact_user_requested_glyphs_and_google_workspace_subtools() {
        let cfg = enabled_config();
        let resolver = IconResolver::from_config(&cfg, Some(Path::new("/tmp/agent")));
        let mut tracker = IconTracker::new();

        // Slack -> Linear -> GitLab -> Gmail -> Drive -> Web fetch
        assert!(tracker.observe_update(
            &resolver,
            &json!({
                "toolCallId": "c1",
                "name": "call_mcp_tool",
                "kind": "execute",
                "rawInput": {"ServerName": "slack", "ToolName": "conversations_history"}
            })
        ));
        // Duplicate Slack call is deduplicated.
        assert!(!tracker.observe_update(
            &resolver,
            &json!({
                "toolCallId": "c2",
                "name": "call_mcp_tool",
                "kind": "execute",
                "rawInput": {"ServerName": "slack", "ToolName": "channels_list"}
            })
        ));
        assert!(tracker.observe_update(
            &resolver,
            &json!({
                "toolCallId": "c3",
                "name": "call_mcp_tool",
                "kind": "execute",
                "rawInput": {"ServerName": "linear", "ToolName": "list_issues"}
            })
        ));
        assert!(tracker.observe_update(
            &resolver,
            &json!({
                "toolCallId": "c4",
                "name": "call_mcp_tool",
                "kind": "execute",
                "rawInput": {"ServerName": "gitlab", "ToolName": "list_merge_requests"}
            })
        ));
        assert!(tracker.observe_update(
            &resolver,
            &json!({
                "toolCallId": "c5",
                "name": "call_mcp_tool",
                "kind": "execute",
                "rawInput": {"ServerName": "google-workspace", "ToolName": "search_gmail_messages"}
            })
        ));
        assert!(tracker.observe_update(
            &resolver,
            &json!({
                "toolCallId": "c6",
                "name": "call_mcp_tool",
                "kind": "execute",
                "rawInput": {"ServerName": "google-workspace", "ToolName": "import_to_google_doc"}
            })
        ));
        assert!(tracker.observe_update(
            &resolver,
            &json!({
                "toolCallId": "c7",
                "name": "read_url_content",
                "kind": "fetch"
            })
        ));
        assert!(tracker.observe_update(
            &resolver,
            &json!({
                "toolCallId": "c8",
                "name": "run_command",
                "kind": "execute",
                "rawInput": {"CommandLine": "swaync-client -sw --latest-notification"}
            })
        ));

        assert_eq!(
            tracker.icons(),
            &[
                "\u{e8a4}",  // slack
                "\u{f145}",  // linear
                "\u{e7eb}",  // gitlab
                "\u{f42f}",  // gmail
                "\u{f2df}",  // google drive
                "\u{f059f}", // web fetch
                "\u{f0f3}",  // swaync-client bell
            ]
        );

        // Remaining Google Workspace groups & fallback.
        let mut g_tracker = IconTracker::new();
        for (id, tool) in [
            ("g1", "query_freebusy"),
            ("g2", "list_tasks"),
            ("g3", "search_contacts"),
            ("g4", "start_google_auth"),
        ] {
            g_tracker.observe_update(
                &resolver,
                &json!({
                    "toolCallId": id,
                    "name": "call_mcp_tool",
                    "kind": "execute",
                    "rawInput": {"ServerName": "google-workspace", "ToolName": tool}
                }),
            );
        }
        assert_eq!(
            g_tracker.icons(),
            &["\u{f455}", "\u{f4a0}", "\u{f415}", "\u{f1a0}"]
        );
    }

    #[test]
    fn holds_argumentless_stubs_until_permission_or_update_arrives() {
        let cfg = enabled_config();
        let resolver = IconResolver::from_config(&cfg, Some(Path::new("/tmp/agent")));
        let mut tracker = IconTracker::new();

        // 1. Argument-less run_command stub -> must NOT emit generic terminal icon yet.
        assert!(!tracker.observe_update(
            &resolver,
            &json!({"toolCallId": "cmd1", "name": "run_command", "kind": "execute"})
        ));
        assert!(tracker.icons().is_empty());

        // Permission arrives with `gh pr list` -> resolves directly to GitHub icon only.
        assert!(tracker.observe_permission(
            &resolver,
            Some(&json!({
                "toolCallId": "cmd1",
                "name": "run_command",
                "kind": "execute",
                "rawInput": {"CommandLine": "env FOO=1 gh pr list"}
            }))
        ));
        assert_eq!(tracker.icons(), &["\u{e709}"]);

        // 2. Argument-less call_mcp_tool stub -> tool_call_update without name supplies ServerName.
        assert!(!tracker.observe_update(
            &resolver,
            &json!({"toolCallId": "mcp1", "name": "call_mcp_tool", "kind": "execute"})
        ));
        assert!(tracker.observe_followup(
            &resolver,
            &json!({
                "toolCallId": "mcp1",
                "rawInput": {"ServerName": "slack", "ToolName": "post_message"}
            })
        ));
        assert_eq!(tracker.icons(), &["\u{e709}", "\u{e8a4}"]);

        // 3. Argument-less execute stub that completes without ever giving a command -> falls back to terminal icon.
        assert!(!tracker.observe_update(
            &resolver,
            &json!({"toolCallId": "cmd2", "name": "run_command", "kind": "execute"})
        ));
        assert!(tracker.observe_followup(
            &resolver,
            &json!({"toolCallId": "cmd2", "status": "completed"})
        ));
        assert_eq!(tracker.icons(), &["\u{e709}", "\u{e8a4}", "\u{f489}"]);

        // 4. Failed stub emits nothing.
        assert!(!tracker.observe_update(
            &resolver,
            &json!({"toolCallId": "read1", "name": "read_file", "kind": "read"})
        ));
        assert!(!tracker.observe_followup(
            &resolver,
            &json!({"toolCallId": "read1", "status": "failed"})
        ));
        assert_eq!(tracker.icons(), &["\u{e709}", "\u{e8a4}", "\u{f489}"]);
    }

    #[test]
    fn suppresses_internal_reads_and_maps_vault_paths() {
        let cfg = enabled_config();
        let resolver = IconResolver::from_config(&cfg, Some(Path::new("/tmp/agent")));
        let mut tracker = IconTracker::new();

        // Suppressed: <agent_dir>/antigravity-cli/mcp/... schema read.
        assert!(!tracker.observe_update(
            &resolver,
            &json!({
                "toolCallId": "r1",
                "name": "read_file",
                "kind": "read",
                "rawInput": {"Path": "/tmp/agent/antigravity-cli/mcp/slack.json"}
            })
        ));
        // Suppressed: configured skill SKILL.md read.
        assert!(!tracker.observe_update(
            &resolver,
            &json!({
                "toolCallId": "r2",
                "name": "read_file",
                "kind": "read",
                "rawInput": {"Path": "/home/user/skills/transit/SKILL.md"}
            })
        ));
        assert!(tracker.icons().is_empty());

        // Vault read -> vault icon (nf-md-notebook), NOT generic file read.
        assert!(tracker.observe_update(
            &resolver,
            &json!({
                "toolCallId": "r3",
                "name": "read_file",
                "kind": "read",
                "rawInput": {"Path": "/home/user/Vault/Journal/2026-11-25.md"}
            })
        ));
        assert_eq!(tracker.icons(), &["\u{f082e}"]);

        // External file read & edit.
        assert!(tracker.observe_update(
            &resolver,
            &json!({
                "toolCallId": "r4",
                "name": "read_file",
                "kind": "read",
                "rawInput": {"Path": "/etc/hostname"}
            })
        ));
        assert!(tracker.observe_update(
            &resolver,
            &json!({
                "toolCallId": "e1",
                "name": "edit_file",
                "kind": "edit",
                "rawInput": {"Path": "/tmp/notes.txt"}
            })
        ));
        assert_eq!(tracker.icons(), &["\u{f082e}", "\u{f4a5}", "\u{f040}"]);
    }

    #[test]
    fn respects_user_overrides_specificity_and_empty_suppression() {
        let mut cfg = enabled_config();
        cfg.mcp
            .get_mut("google-workspace")
            .unwrap()
            .tool_icons
            .insert("search_gmail_messages".to_string(), "X".to_string());
        cfg.present
            .tool_icons
            .commands
            .insert("openhue".to_string(), String::new());
        cfg.present
            .tool_icons
            .kinds
            .insert("mcp_ui".to_string(), "W".to_string());

        let resolver = IconResolver::from_config(&cfg, None);
        let mut tracker = IconTracker::new();

        // Exact user override beats built-in `*gmail*`.
        assert!(tracker.observe_update(
            &resolver,
            &json!({
                "toolCallId": "m1",
                "name": "call_mcp_tool",
                "kind": "execute",
                "rawInput": {"ServerName": "google-workspace", "ToolName": "search_gmail_messages"}
            })
        ));
        // Explicitly suppressed command emits neither openhue nor generic execute.
        assert!(!tracker.observe_update(
            &resolver,
            &json!({
                "toolCallId": "c1",
                "name": "run_command",
                "kind": "execute",
                "rawInput": {"CommandLine": "openhue get light"}
            })
        ));
        // Eager mcp-ui respects `kinds.mcp_ui` override.
        assert!(tracker.observe_update(
            &resolver,
            &json!({
                "toolCallId": "u1",
                "name": "mcp_mcp-ui_show_widget",
                "kind": "other"
            })
        ));
        assert_eq!(tracker.icons(), &["X", "W"]);
    }
}
