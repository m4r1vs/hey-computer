//! The deny list: which commands the agent is refused.
//!
//! This is a courtesy, not a boundary. A model that wants to can always find a
//! spelling this does not catch (a variable, base64, a script it writes
//! first). What keeps privileged commands from working is the operating
//! system: the service runs with `NoNewPrivileges`, so no setuid program can
//! gain anything. The list exists so the usual attempt gets a clean refusal the
//! daemon can explain, instead of a password prompt that hangs or a confusing
//! failure.

use std::path::Path;

/// Programs that only run another command, so the word after them (past their
/// options) is the command that matters.
const WRAPPERS: &[&str] = &[
    "env", "command", "exec", "nohup", "time", "nice", "ionice", "stdbuf", "timeout", "xargs",
    "setsid", "chrt", "taskset", "unbuffer", "builtin",
];

/// Shells whose `-c` argument is itself a command line.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "fish", "ksh"];

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
                i += 1;
            }
        }
        let Some(word) = words.get(i) else {
            continue;
        };
        let name = base_name(word);
        if SHELLS.contains(&name.as_str())
            && let Some(pos) = words[i + 1..].iter().position(|w| w == "-c")
            && let Some(script) = words.get(i + 2 + pos)
        {
            found.extend(command_words(script, depth + 1));
        }
        found.push(name);
    }
    for inner in substitutions(line) {
        found.extend(command_words(&inner, depth + 1));
    }
    found
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

/// Split at `;`, `&`, `|`, newlines and parentheses outside quotes.
fn split_commands(line: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
                current.push(c);
            }
            (Some('"'), '\\') => {
                current.push(c);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            (Some(_), c) => current.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                current.push(c);
            }
            (None, '\\') => {
                current.push(c);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            (None, ';' | '&' | '|' | '\n' | '(' | ')' | '{' | '}' | '`') => {
                segments.push(std::mem::take(&mut current));
            }
            (None, c) => current.push(c),
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
    let mut quote: Option<char> = None;
    let mut chars = segment.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            (Some(_), c) => current.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
                in_word = true;
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

/// The insides of `$( … )`, which `split_commands` only cut apart.
fn substitutions(line: &str) -> Vec<String> {
    let mut found = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'$' && bytes[i + 1] == b'(' {
            let start = i + 2;
            let mut depth = 1;
            let mut j = start;
            while j < bytes.len() && depth > 0 {
                match bytes[j] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                j += 1;
            }
            let end = if depth == 0 { j - 1 } else { j };
            found.push(line[start..end].to_string());
            i = j;
        } else {
            i += 1;
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
        ] {
            assert_eq!(p.denied_word(line), None, "{line}");
        }
    }

    #[test]
    fn an_empty_list_denies_nothing() {
        assert_eq!(Policy::new(&[]).denied_word("sudo ls"), None);
    }

    #[test]
    fn words_in_command_position() {
        assert_eq!(
            command_words("A=1 env B=2 /usr/bin/hyprctl dispatch exec firefox", 0),
            ["env", "hyprctl"]
        );
        assert_eq!(command_words("ls -l | wc -l", 0), ["ls", "wc"]);
    }
}
