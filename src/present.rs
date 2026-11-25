//! Showing answers: a notification when the answer is short and plain, a
//! Markdown viewer when it is long or rich.
//!
//! Everything goes through [`Presenter`], so the daemon can be tested with a
//! [`Recorder`] and never touch the desktop. The real one, [`Desktop`], runs
//! the notifier as a program (notify-send by default) rather than talking
//! D-Bus itself: an argument vector can be swapped for a script, which is how
//! the daemon is tested by hand without popping anything up.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinSet;

use crate::config::Present as PresentConfig;
use crate::ipc;
use crate::protocol::FailReason;

static LOG_COUNTER: AtomicU64 = AtomicU64::new(0);

const APP_NAME: &str = "hey-computer";

/// The notifier gets this long before it is given up on.
const NOTIFIER_TIMEOUT: Duration = Duration::from_secs(5);
/// The clipboard program gets this long to accept stdin and exit (or fork).
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a failure stays up.
const FAILURE_EXPIRE_MS: u32 = 8_000;
/// "Thinking…" is replaced by the answer; this only bounds a popup whose
/// daemon died before it could replace it.
const PROGRESS_EXPIRE_MS: u32 = 120_000;
const CANCELLED_EXPIRE_MS: u32 = 2_000;
/// The request echoed in the progress popup is cut here.
const REQUEST_PREVIEW_CHARS: usize = 160;
/// Minimum interval between live progress notification replacements.
const PROGRESS_UPDATE_INTERVAL: Duration = Duration::from_millis(400);
/// A viewer that exits with an error within this long is reported as
/// failed; one that is still running then has its window up.
const VIEWER_GRACE: Duration = Duration::from_millis(1500);
/// How much of a failed viewer's stderr goes into the message.
const VIEWER_STDERR_KEPT: usize = 400;
/// Maximum concurrent waiting action-notification tasks (`notify-send -A`).
const MAX_ACTION_WAITERS: usize = 16;
/// How long an unclicked "Open" action waiter lives before giving up.
const ACTION_WAIT_TIMEOUT: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, PartialEq)]
pub enum Presentation {
    /// An interactive request has started.
    Progress {
        id: u64,
        request: String,
        profile: Option<String>,
        /// Optional custom progress summary overriding `"Thinking…"` / `"Thinking… (<profile>)"`.
        thinking_label: Option<String>,
    },
    /// Mid-turn status update for an in-flight interactive request.
    ProgressUpdate { id: u64, status: String },
    /// Quietly dismiss any active interactive progress popup (e.g. suppressed turn).
    DismissProgress { id: u64 },
    Answer {
        id: u64,
        text: String,
        /// Commands the agent was refused on the way.
        denied: Vec<String>,
        profile: Option<String>,
    },
    /// A background task finished with a non-suppressed answer.
    TaskComplete {
        id: u64,
        request: String,
        text: String,
        denied: Vec<String>,
        profile: Option<String>,
    },
    Failure {
        id: u64,
        reason: FailReason,
        message: String,
        profile: Option<String>,
        background: bool,
    },
    /// A standalone notification (e.g. scheduled reminder, split failure alert,
    /// or background-delegation notice). Replaces an active progress popup only
    /// when `replaces_id` matches that request's id.
    Notice {
        summary: String,
        body: String,
        urgency: Urgency,
        replaces_id: Option<u64>,
    },
}

pub trait Presenter: Send + Sync + 'static {
    /// Queue something to show. Must not block: the daemon calls it from the
    /// task that talks to the agent.
    fn present(&self, item: Presentation);
}

/// The limits that decide between a notification and the viewer.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_chars: usize,
    pub max_lines: usize,
}

impl From<&PresentConfig> for Limits {
    fn from(config: &PresentConfig) -> Self {
        Self {
            max_chars: config.max_chars,
            max_lines: config.max_lines,
        }
    }
}

/// Whether an answer fits a notification: short, few lines, and nothing a
/// notification cannot render (a heading, a table, a code block, math).
///
/// Blank lines do not count: models often put each of three sentences in a
/// paragraph of its own, and that still fits a notification.
pub fn is_short(text: &str, limits: Limits) -> bool {
    let text = text.trim();
    text.chars().count() <= limits.max_chars
        && text.lines().filter(|l| !l.trim().is_empty()).count() <= limits.max_lines
        && !is_rich(text)
}

/// Markdown that only a real renderer shows properly.
pub fn is_rich(text: &str) -> bool {
    if text.contains("$$") || text.contains("\\[") || text.contains("\\(") {
        return true;
    }
    if has_latex_inline_math(text) {
        return true;
    }
    text.lines().any(|line| {
        let indent = line.len() - line.trim_start_matches(' ').len();
        let line = line.trim();
        if indent > 3 {
            return false;
        }
        let hashes = line.chars().take_while(|&c| c == '#').count();
        let heading = (1..=6).contains(&hashes) && line[hashes..].starts_with([' ', '\t']);
        let fence = line.starts_with("```") || line.starts_with("~~~");
        let table = line.starts_with('|') && line.matches('|').count() >= 2;
        heading || fence || table
    })
}

/// Inline `$…$` containing LaTeX syntax (`\`, `^`, `_`).
///
/// Follows Pandoc's `tex_math_dollars` spacing rules (no whitespace right
/// inside either `$`) and rejects a closing `$` immediately followed by an
/// alphanumeric character, `{`, or `\`, so currency ("$5 and $10",
/// "$5_000-$10_000") and shell variables ("$FOO_BAR and $BAZ_QUX",
/// "$HOME/$USER_DIR", "${A_B}/${C_D}") stay in notifications while inline math
/// like `$x^2 + y^2 = z^2$` and `$a_i < b_i$` goes to the viewer. Inline code
/// spans are skipped so shell examples in backticks never trigger math.
fn has_latex_inline_math(text: &str) -> bool {
    text.lines().any(line_has_latex_inline_math)
}

fn line_has_latex_inline_math(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut open: Option<usize> = None;

    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                // Skip escaped `\`, `$`, and `` ` `` so `\$` is never a
                // delimiter, `\\$` still closes math, and `` \` `` does not
                // start a code span.
                if matches!(bytes.get(i + 1), Some(b'\\' | b'$' | b'`')) {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            b'`' => {
                let run = bytes[i..].iter().take_while(|&&b| b == b'`').count();
                if let Some(end) = find_closing_backticks(&bytes[i + run..], run) {
                    // Code spans take precedence over `$…$`; an earlier
                    // unclosed `$` was plain text (e.g. currency).
                    open = None;
                    i += run + end;
                } else {
                    i += run;
                }
            }
            b'$' => {
                let run = bytes[i..].iter().take_while(|&&b| b == b'$').count();
                if run > 1 {
                    // Display math `$$` is handled by `is_rich`; never split a
                    // multi-dollar run into inline delimiters.
                    open = None;
                    i += run;
                    continue;
                }
                let before = line[..i].chars().next_back();
                let after = line[i + 1..].chars().next();
                let valid_open = after.is_some_and(|c| !c.is_whitespace());
                if let Some(start) = open {
                    let valid_close = before.is_some_and(|c| !c.is_whitespace())
                        && after.is_none_or(|c| !c.is_alphanumeric() && c != '{' && c != '\\');
                    if valid_close {
                        if line[start..i].contains(['\\', '^', '_']) {
                            return true;
                        }
                        open = None;
                    } else {
                        // A lone `$` (e.g. "$5 for $x^2$") failed to close;
                        // let this `$` try as a fresh opener instead.
                        open = valid_open.then_some(i + 1);
                    }
                } else {
                    open = valid_open.then_some(i + 1);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    false
}

/// Byte offset immediately after the first run of exactly `run` backticks in
/// `bytes`, or `None` when the backtick run is unclosed.
fn find_closing_backticks(bytes: &[u8], run: usize) -> Option<usize> {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let n = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            if n == run {
                return Some(i + n);
            }
            i += n;
        } else {
            i += 1;
        }
    }
    None
}

/// Escape text for Pango markup, as `g_markup_escape_text` does.
pub fn escape_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            // Control characters other than newline and tab are invalid in
            // markup and make the whole body fail to parse.
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}

/// A short answer as notification markup: escaped, with `**bold**` and
/// `` `code` `` rendered, since models use them even when asked not to.
///
/// Code spans are found first and their content is left alone, so `**`
/// inside one (`2**8`, `**/*.rs`) cannot open a bold tag that would close
/// inside another; Pango refuses the whole body when tags do not nest.
pub fn to_markup(text: &str) -> String {
    escape_markup(text.trim())
        .split('\n')
        .map(|line| {
            let mut out = String::new();
            for (code, piece) in spans(line, "`") {
                if code {
                    out.push_str("<tt>");
                    out.push_str(piece);
                    out.push_str("</tt>");
                } else {
                    for (bold, piece) in spans(piece, "**") {
                        if bold {
                            out.push_str("<b>");
                            out.push_str(piece);
                            out.push_str("</b>");
                        } else {
                            out.push_str(piece);
                        }
                    }
                }
            }
            out
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Cut `text` at `marker` pairs: (true, inside) for each paired span,
/// (false, outside) for the rest. An unpaired last marker stays in the
/// text as it is.
fn spans<'a>(text: &'a str, marker: &str) -> Vec<(bool, &'a str)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find(marker) {
        let after = &rest[open + marker.len()..];
        let Some(close) = after.find(marker) else {
            break;
        };
        out.push((false, &rest[..open]));
        out.push((true, &after[..close]));
        rest = &after[close + marker.len()..];
    }
    out.push((false, rest));
    out
}

/// Tracks whether the current line sits inside a CommonMark fenced code block.
#[derive(Default)]
struct FenceState {
    open: Option<String>,
}

impl FenceState {
    /// Advance by one line. Returns `true` for a fence delimiter or a line
    /// inside an open fence.
    fn step(&mut self, line: &str, in_math: bool) -> bool {
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        let marker: String = trimmed
            .chars()
            .take_while(|&c| c == '`' || c == '~')
            .collect();
        let is_fence = !in_math
            && indent <= 3
            && marker.len() >= 3
            && marker
                .chars()
                .all(|c| c == marker.chars().next().unwrap_or('`'));
        match &self.open {
            Some(open) => {
                if is_fence
                    && marker.starts_with(open.as_str())
                    && trimmed[marker.len()..].trim().is_empty()
                {
                    self.open = None;
                }
                true
            }
            None if is_fence => {
                // CommonMark §4.5: backtick code fences cannot contain backticks
                // in their info string (` ```code``` ` is an inline code span).
                if marker.starts_with('`') && trimmed[marker.len()..].contains('`') {
                    return false;
                }
                self.open = Some(marker);
                true
            }
            None => false,
        }
    }
}

/// The answer with raw HTML made harmless for the viewer: no `<` is left
/// outside code.
///
/// The model's answer can quote a web page or a note, and the viewer renders
/// Markdown in a web view: a `<script>` would run there and an `<img>` would
/// be fetched, outside the daemon and with nobody asking. So every `<` in
/// text becomes `&lt;` (a Markdown renderer shows it as `<`), and every `<`
/// in math becomes `\lt ` (KaTeX typesets it as `<`; Marker shows math
/// verbatim, so `&lt;` would appear there as it is). Code spans, fenced
/// blocks and autolinks such as `<https://example.org>` are left alone: they
/// are shown literally anyway.
pub fn disarm_html(markdown: &str) -> String {
    let refs = collect_ref_defs(markdown);
    let mut out = String::with_capacity(markdown.len());
    let mut fence = FenceState::default();
    let mut in_math = false;
    let mut paragraph = String::new();
    let flush_paragraph = |paragraph: &mut String, out: &mut String, in_math: &mut bool| {
        if !paragraph.is_empty() {
            out.push_str(&disarm_paragraph(paragraph, in_math, &refs));
            paragraph.clear();
        }
    };
    for line in markdown.split_inclusive('\n') {
        if fence.step(line, in_math) || line.trim().is_empty() {
            flush_paragraph(&mut paragraph, &mut out, &mut in_math);
            out.push_str(line);
        } else {
            paragraph.push_str(line);
        }
    }
    flush_paragraph(&mut paragraph, &mut out, &mut in_math);
    out
}

/// One non-blank paragraph outside fenced blocks. Operating on the whole
/// paragraph lets inline code spans cross single newlines when closed while
/// still disarming `<` and `![…]` after unclosed backticks. `in_math` carries
/// a `$$` display block over across lines/paragraphs.
fn disarm_paragraph(paragraph: &str, in_math: &mut bool, refs: &HashMap<String, bool>) -> String {
    let mut out = String::with_capacity(paragraph.len());
    let mut rest = paragraph;
    while !rest.is_empty() {
        if *in_math {
            match rest.find("$$") {
                Some(end) => {
                    out.push_str(&math(&rest[..end + 2], refs));
                    rest = &rest[end + 2..];
                    *in_math = false;
                }
                None => {
                    out.push_str(&math(rest, refs));
                    return out;
                }
            }
            continue;
        }
        let Some(i) = rest.find(['`', '<', '$', '\\', '!']) else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let span = |open: &str, close: &str| {
            rest.strip_prefix(open)
                .and_then(|inner| inner.find(close))
                .map(|end| open.len() + end + close.len())
        };
        if rest.starts_with('`') {
            // CommonMark §6.1: a code span closes only with an exact run of
            // `run` backticks within the same paragraph.
            let run = rest.len() - rest.trim_start_matches('`').len();
            let end = find_closing_backticks(&rest.as_bytes()[run..], run)
                .map(|close| run + close)
                .unwrap_or(run);
            out.push_str(&rest[..end]);
            rest = &rest[end..];
        } else if rest.starts_with("$$") {
            match span("$$", "$$") {
                Some(end) => {
                    out.push_str(&math(&rest[..end], refs));
                    rest = &rest[end..];
                }
                None => {
                    *in_math = true;
                    out.push_str(&math(rest, refs));
                    return out;
                }
            }
        } else if let Some(end) = find_inline_dollar_math(rest)
            .or_else(|| span("\\(", "\\)"))
            .or_else(|| span("\\[", "\\]"))
        {
            out.push_str(&math(&rest[..end], refs));
            rest = &rest[end..];
        } else if rest.starts_with("![") && is_disarmed_image(rest, refs) {
            // Shown as a link: the web view would fetch or render it at once.
            out.push_str("[image: ");
            rest = &rest[2..];
        } else if rest.starts_with("\\<") {
            // Markdown's own escape for a `<` shown as it is.
            out.push_str("&lt;");
            rest = &rest[2..];
        } else if rest.starts_with("\\\\")
            || rest.starts_with("\\`")
            || rest.starts_with("\\$")
            || rest.starts_with("\\!")
        {
            out.push_str(&rest[..2]);
            rest = &rest[2..];
        } else if rest.starts_with('<') && is_autolink(rest) {
            let end = rest.find('>').map_or(rest.len(), |e| e + 1);
            out.push_str(&rest[..end]);
            rest = &rest[end..];
        } else if rest.starts_with('<') {
            out.push_str("&lt;");
            rest = &rest[1..];
        } else {
            // A lone `$`, `\` or `!`.
            let c = rest.chars().next().map_or(1, char::len_utf8);
            out.push_str(&rest[..c]);
            rest = &rest[c..];
        }
    }
    out
}

/// Byte length of a single-line `$…$` inline math span starting at `rest`, if
/// it has non-whitespace inner boundaries, contains no unescaped backticks, and
/// is not followed by an alphanumeric currency/identifier continuation.
fn find_inline_dollar_math(rest: &str) -> Option<usize> {
    if !rest.starts_with('$') || rest.starts_with("$$") {
        return None;
    }
    let line = rest.split('\n').next().unwrap_or(rest);
    let after_open = line.get(1..)?.chars().next()?;
    if after_open.is_whitespace() {
        return None;
    }
    let bytes = line.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += if i + 1 < bytes.len() { 2 } else { 1 };
            }
            b'`' => return None,
            b'$' => {
                let run = bytes[i..].iter().take_while(|&&b| b == b'$').count();
                if run > 1 {
                    return None;
                }
                let before = line[1..i].chars().next_back();
                let after = line[i + 1..].chars().next();
                let valid_close = before.is_some_and(|c| !c.is_whitespace())
                    && after.is_none_or(|c| !c.is_alphanumeric() && c != '{' && c != '\\');
                if valid_close {
                    return Some(i + 1);
                }
                return None;
            }
            _ => i += 1,
        }
    }
    None
}

/// Math with `<` spelt as KaTeX's `\lt` and any remote/scheme `![…]` images
/// disarmed in case the viewer rejects the math span and falls back to Markdown.
fn math(text: &str, refs: &HashMap<String, bool>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(['<', '!']) {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        if rest.starts_with('<') {
            out.push_str("\\lt ");
            rest = &rest[1..];
        } else if rest.starts_with("![") && is_disarmed_image(rest, refs) {
            out.push_str("[image: ");
            rest = &rest[2..];
        } else {
            out.push('!');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

/// Whether `![…]` (at the start of `text`) is an image whose target is remote
/// or carries a URI scheme such as `data:` (including CommonMark full,
/// collapsed, and shortcut reference images). Local file paths stay images.
fn is_disarmed_image(text: &str, refs: &HashMap<String, bool>) -> bool {
    let Some(alt_end) = find_closing_bracket(&text[2..]) else {
        return false;
    };
    let alt = &text[2..2 + alt_end];
    let after = &text[2 + alt_end + 1..];

    if let Some(after_paren) = after.strip_prefix('(') {
        let trimmed = after_paren.trim_start();
        if trimmed.is_empty() {
            // Multi-line destination (`![alt](\nhttps://…)`): fail closed.
            return true;
        }
        if trimmed.starts_with(')') {
            return false;
        }
        return is_disarmed_target(trimmed);
    }

    if let Some(after_bracket) = after.strip_prefix('[')
        && let Some(ref_end) = find_ref_label_end(after_bracket)
    {
        let ref_inner = &after_bracket[..ref_end];
        if ref_inner.is_empty() {
            // Collapsed reference image `![ref][]`.
            let key = normalize_ref_label(alt);
            return refs.get(&key).copied().unwrap_or(true);
        }
        let key = normalize_ref_label(ref_inner);
        if !key.is_empty() {
            // Full reference image `![alt][ref]`; fail closed when undefined.
            return refs.get(&key).copied().unwrap_or(true);
        }
    }

    // Shortcut reference image `![ref]`, disarmed only when a matching
    // definition exists in the document with a disarmed target.
    let key = normalize_ref_label(alt);
    !key.is_empty() && refs.get(&key).copied().unwrap_or(false)
}

/// Byte offset of the `]` matching an opening `[`, respecting backslash
/// escapes, nested brackets, and inline code spans.
fn find_closing_bracket(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut depth = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += 1;
                if i < bytes.len() {
                    i += s[i..].chars().next().map_or(1, char::len_utf8);
                }
            }
            b'`' => {
                let run = bytes[i..].iter().take_while(|&&b| b == b'`').count();
                if let Some(end) = find_closing_backticks(&bytes[i + run..], run) {
                    i += run + end;
                } else {
                    i += run;
                }
            }
            b'[' => {
                depth += 1;
                i += 1;
            }
            b']' => {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
                i += 1;
            }
            _ => i += s[i..].chars().next().map_or(1, char::len_utf8),
        }
    }
    None
}

/// Byte offset of the closing `]` of a CommonMark link label (which cannot
/// contain unescaped `[`).
fn find_ref_label_end(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += 1;
                if i < bytes.len() {
                    i += s[i..].chars().next().map_or(1, char::len_utf8);
                }
            }
            b'[' => return None,
            b']' => return Some(i),
            _ => i += s[i..].chars().next().map_or(1, char::len_utf8),
        }
    }
    None
}

fn normalize_ref_label(label: &str) -> String {
    label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Whether a Markdown image destination is protocol-relative (`//…`) or carries
/// an RFC 3986 URI scheme (`http:`, `https:`, `ftp:`, `data:`, `javascript:`,
/// `file:`, …), after CommonMark destination unescaping.
fn is_disarmed_target(target: &str) -> bool {
    let target = target.trim_start();
    let target = target.strip_prefix('<').unwrap_or(target);
    let prefix = unescape_destination_prefix(target);
    let clean: String = prefix.chars().filter(|c| !c.is_ascii_control()).collect();
    if clean.starts_with("//") {
        return true;
    }
    let mut chars = clean.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    for c in chars {
        if c == ':' {
            return true;
        }
        if !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
            return false;
        }
    }
    false
}

/// Decode CommonMark backslash-punctuation escapes and common HTML character
/// references in the leading prefix of a destination (capped so large `data:`
/// payloads do not allocate).
fn unescape_destination_prefix(s: &str) -> String {
    const MAX_PREFIX_CHARS: usize = 64;
    let bytes = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    let mut count = 0;
    while i < bytes.len() && count < MAX_PREFIX_CHARS {
        if bytes[i] == b'\\'
            && let Some(&next) = bytes.get(i + 1)
            && next.is_ascii_punctuation()
        {
            out.push(next as char);
            i += 2;
            count += 1;
            continue;
        }
        if bytes[i] == b'&'
            && let Some((ch, len)) = decode_scheme_entity(&s[i..])
        {
            out.push(ch);
            i += len;
            count += 1;
            continue;
        }
        let ch = s[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
        count += 1;
    }
    out
}

/// Decode a numeric HTML character reference (`&#58;`, `&#x3a;`) or `&colon;` /
/// `&sol;` at the start of `s`.
fn decode_scheme_entity(s: &str) -> Option<(char, usize)> {
    let rest = s.strip_prefix('&')?;
    let semi = rest.find(';')?;
    let body = &rest[..semi];
    let total = 1 + semi + 1;
    if let Some(hex) = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X")) {
        if (1..=6).contains(&hex.len())
            && hex.bytes().all(|b| b.is_ascii_hexdigit())
            && let Ok(code) = u32::from_str_radix(hex, 16)
            && code != 0
            && let Some(ch) = char::from_u32(code)
        {
            return Some((ch, total));
        }
        return None;
    }
    if let Some(dec) = body.strip_prefix('#') {
        if (1..=7).contains(&dec.len())
            && dec.bytes().all(|b| b.is_ascii_digit())
            && let Ok(code) = dec.parse::<u32>()
            && code != 0
            && let Some(ch) = char::from_u32(code)
        {
            return Some((ch, total));
        }
        return None;
    }
    if body.eq_ignore_ascii_case("colon") {
        return Some((':', total));
    }
    if body.eq_ignore_ascii_case("sol") {
        return Some(('/', total));
    }
    None
}

/// Collect CommonMark link reference definitions (`[label]: destination`)
/// outside fenced and indented code blocks, recording for each normalized label
/// whether any definition for it has a disarmed target.
fn collect_ref_defs(markdown: &str) -> HashMap<String, bool> {
    let mut refs: HashMap<String, bool> = HashMap::new();
    let mut fence = FenceState::default();
    let mut pending_label: Option<String> = None;

    for line in markdown.split_inclusive('\n') {
        if fence.step(line, false) {
            pending_label = None;
            continue;
        }
        let Some(rest) = strip_container_prefixes(line) else {
            pending_label = None;
            continue;
        };
        let trimmed = rest.trim();
        if let Some(label) = pending_label.take()
            && !trimmed.is_empty()
            && parse_ref_def_line(rest).is_none()
        {
            let disarmed = is_disarmed_target(trimmed);
            refs.entry(label)
                .and_modify(|d| *d |= disarmed)
                .or_insert(disarmed);
            continue;
        }
        if let Some((label, dest)) = parse_ref_def_line(rest) {
            if let Some(dest) = dest {
                let disarmed = is_disarmed_target(dest);
                refs.entry(label)
                    .and_modify(|d| *d |= disarmed)
                    .or_insert(disarmed);
            } else {
                pending_label = Some(label);
            }
        }
    }
    refs
}

/// Strip repeatable blockquote markers (`>`) and an optional list marker,
/// returning `None` for 4-space / tab indented code lines.
fn strip_container_prefixes(mut line: &str) -> Option<&str> {
    // Blockquote markers may nest (`> > [ref]: …`).
    loop {
        let spaces = line.bytes().take_while(|&b| b == b' ').count();
        if spaces <= 3
            && let Some(after) = line[spaces..].strip_prefix('>')
        {
            line = after.strip_prefix(' ').unwrap_or(after);
            continue;
        }
        break;
    }
    // Optional unordered (`-`, `*`, `+`) or ordered (`1.`, `1)`) list marker.
    let spaces = line.bytes().take_while(|&b| b == b' ').count();
    if spaces <= 3 {
        let after_spaces = &line[spaces..];
        let bytes = after_spaces.as_bytes();
        if matches!(bytes.first(), Some(b'-' | b'*' | b'+'))
            && matches!(bytes.get(1), Some(b' ' | b'\t'))
        {
            line = &after_spaces[2..];
        } else {
            let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
            if (1..=9).contains(&digits)
                && matches!(bytes.get(digits), Some(b'.' | b')'))
                && matches!(bytes.get(digits + 1), Some(b' ' | b'\t'))
            {
                line = &after_spaces[digits + 2..];
            }
        }
    }
    let indent = line.bytes().take_while(|&b| b == b' ').count();
    if indent > 3 || line[indent..].starts_with('\t') {
        return None;
    }
    Some(&line[indent..])
}

/// Parse `[label]: [destination]` at the start of `line`. Returns
/// `Some((normalized_label, Some(dest)))` when a destination is on the same
/// line, or `Some((normalized_label, None))` when the destination may follow on
/// the next line.
fn parse_ref_def_line(line: &str) -> Option<(String, Option<&str>)> {
    let after_open = line.strip_prefix('[')?;
    let end = find_ref_label_end(after_open)?;
    let after_colon = after_open[end + 1..].strip_prefix(':')?;
    let label = normalize_ref_label(&after_open[..end]);
    if label.is_empty() {
        return None;
    }
    let dest = after_colon.trim();
    if dest.is_empty() {
        Some((label, None))
    } else {
        Some((label, Some(dest)))
    }
}

/// `<https://…>`, `<http://…>` or `<mailto:…>`, closed before any space.
fn is_autolink(text: &str) -> bool {
    let inner = &text[1..];
    ["https://", "http://", "mailto:"].iter().any(|scheme| {
        inner
            .get(..scheme.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(scheme))
    }) && inner
        .find(|c: char| c == '>' || c.is_whitespace() || c == '<')
        .is_some_and(|end| inner[end..].starts_with('>'))
}

/// The first thing worth naming in a long answer: its first heading or line,
/// without Markdown markers.
pub fn headline(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("```") && !l.starts_with("$$"))
        .unwrap_or("");
    let line = line.trim_start_matches('#').trim();
    truncate(line, max)
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Records everything instead of showing it.
#[derive(Default)]
pub struct Recorder {
    items: Mutex<Vec<Presentation>>,
    changed: Notify,
}

impl Recorder {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn items(&self) -> Vec<Presentation> {
        self.items.lock().map(|i| i.clone()).unwrap_or_default()
    }

    /// Wait until `predicate` holds for the recorded items.
    pub async fn wait_for(
        &self,
        timeout: Duration,
        predicate: impl Fn(&[Presentation]) -> bool,
    ) -> Option<Vec<Presentation>> {
        let wait = async {
            loop {
                let notified = self.changed.notified();
                let items = self.items();
                if predicate(&items) {
                    return items;
                }
                notified.await;
            }
        };
        tokio::time::timeout(timeout, wait).await.ok()
    }
}

impl Presenter for Recorder {
    fn present(&self, item: Presentation) {
        if let Ok(mut items) = self.items.lock() {
            items.push(item);
        }
        self.changed.notify_waiters();
    }
}

/// Notifications through the configured notifier, long answers through the
/// configured viewer. A queue and one task keep popups in order, which
/// matters because each replaces the one before.
pub struct Desktop {
    queue: Mutex<Option<mpsc::UnboundedSender<Presentation>>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Desktop {
    pub fn spawn(config: &PresentConfig, answers_dir: PathBuf) -> Arc<Self> {
        Self::spawn_with_options(config, answers_dir, VIEWER_GRACE, PROGRESS_UPDATE_INTERVAL)
    }

    fn spawn_with_options(
        config: &PresentConfig,
        answers_dir: PathBuf,
        viewer_grace: Duration,
        progress_interval: Duration,
    ) -> Arc<Self> {
        Self::spawn_full(
            config,
            answers_dir,
            viewer_grace,
            progress_interval,
            ACTION_WAIT_TIMEOUT,
        )
    }

    fn spawn_full(
        config: &PresentConfig,
        answers_dir: PathBuf,
        viewer_grace: Duration,
        progress_interval: Duration,
        action_timeout: Duration,
    ) -> Arc<Self> {
        let (queue, mut items) = mpsc::unbounded_channel();
        let (action_cancel, _) = watch::channel(false);
        let mut worker = Worker {
            config: config.clone(),
            answers_dir,
            progress: None,
            pending: None,
            watchers: JoinSet::new(),
            action_waiters: JoinSet::new(),
            action_cancel,
            viewer_grace,
            progress_interval,
            action_timeout,
        };
        let task = tokio::spawn(async move {
            let mut deferred: Option<Presentation> = None;
            loop {
                let next = if let Some(item) = deferred.take() {
                    Some(item)
                } else {
                    tokio::select! {
                        biased;
                        item = items.recv() => item,
                        () = sleep_until_due(worker.pending_due()) => {
                            worker.flush_pending().await;
                            continue;
                        }
                    }
                };
                let Some(item) = next else {
                    break;
                };

                if let Presentation::ProgressUpdate { id, status } = item {
                    let mut latest = Some((id, status));
                    loop {
                        match items.try_recv() {
                            Ok(Presentation::ProgressUpdate { id, status }) => {
                                latest = Some((id, status));
                            }
                            Ok(other) => {
                                // A queued Answer, Failure, or newer Progress
                                // preempts any still-queued ProgressUpdate at once.
                                deferred = Some(other);
                                latest = None;
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    if let Some((id, status)) = latest {
                        worker.progress_update(id, status).await;
                    }
                    continue;
                }

                worker.show(item).await;
            }
            // Let any viewer started in the last VIEWER_GRACE finish its
            // early-exit check before exit; long-running viewers are already
            // handed off to detached reapers.
            while worker.watchers.join_next().await.is_some() {}
            // Never stall daemon shutdown on unclicked "Open" notifications.
            let _ = worker.action_cancel.send(true);
            worker.action_waiters.abort_all();
            while worker.action_waiters.join_next().await.is_some() {}
        });
        Arc::new(Self {
            queue: Mutex::new(Some(queue)),
            task: Mutex::new(Some(task)),
        })
    }

    /// Show what is queued, then stop taking more.
    pub async fn finish(&self) {
        drop(self.queue.lock().ok().and_then(|mut q| q.take()));
        let task = self.task.lock().ok().and_then(|mut t| t.take());
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

impl Presenter for Desktop {
    fn present(&self, item: Presentation) {
        if let Ok(queue) = self.queue.lock()
            && let Some(queue) = queue.as_ref()
        {
            let _ = queue.send(item);
        }
    }
}

enum ProgressSlot {
    /// Active "Thinking…" popup belonging to `request_id`.
    Active {
        request_id: u64,
        notif_id: u32,
        /// Notification summary (e.g. `"Thinking…"` or `"Thinking… (deep)"`).
        summary: String,
        /// Already truncated & Pango-escaped request preview.
        request_body: String,
        last_status: Option<String>,
        last_sent: Option<Instant>,
    },
    /// "Request replaced" popup id kept from a `Superseded` turn; only the
    /// next `Progress` may reuse it (never a `ProgressUpdate`).
    Reusable(u32),
}

fn with_profile(base: &str, profile: Option<&str>) -> String {
    match profile {
        Some(name) if !name.is_empty() => format!("{base} ({name})"),
        _ => base.to_string(),
    }
}

struct PendingUpdate {
    request_id: u64,
    status: String,
    due: Instant,
}

async fn sleep_until_due(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

fn format_progress_body(request_body: &str, status: &str) -> String {
    let status = escape_markup(status);
    if request_body.is_empty() {
        format!("<b>{status}</b>")
    } else {
        format!("{request_body}\n<b>{status}</b>")
    }
}

struct Worker {
    config: PresentConfig,
    answers_dir: PathBuf,
    /// The "Thinking…" popup on screen, which the next popup replaces.
    progress: Option<ProgressSlot>,
    /// Throttled trailing-edge progress update waiting for its interval.
    pending: Option<PendingUpdate>,
    /// Bounded early-exit watchers for recently launched viewers.
    watchers: JoinSet<()>,
    /// Detached `notify-send -A open=Open` waiters for completed background tasks.
    action_waiters: JoinSet<()>,
    action_cancel: watch::Sender<bool>,
    viewer_grace: Duration,
    progress_interval: Duration,
    action_timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Low,
    Normal,
}

impl Worker {
    fn pending_due(&self) -> Option<Instant> {
        self.pending.as_ref().map(|p| p.due)
    }

    fn take_progress_id(&mut self) -> Option<u32> {
        match self.progress.take() {
            Some(ProgressSlot::Active { notif_id, .. })
            | Some(ProgressSlot::Reusable(notif_id)) => Some(notif_id),
            None => None,
        }
    }

    async fn show(&mut self, item: Presentation) {
        while self.watchers.try_join_next().is_some() {}
        while self.action_waiters.try_join_next().is_some() {}
        if !matches!(
            item,
            Presentation::TaskComplete { .. }
                | Presentation::Notice { .. }
                | Presentation::Failure {
                    background: true,
                    ..
                }
        ) {
            self.pending = None;
        }
        match item {
            Presentation::Progress {
                id,
                request,
                profile,
                thinking_label,
            } => {
                if !self.config.progress {
                    return;
                }
                let summary = thinking_label
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| with_profile("Thinking…", profile.as_deref()));
                let request_body = escape_markup(&truncate(request.trim(), REQUEST_PREVIEW_CHARS));
                // A superseded request's popup is reused for the next one.
                let replaces = self.take_progress_id();
                if let Some(notif_id) = self
                    .notify(
                        &summary,
                        &request_body,
                        Urgency::Low,
                        PROGRESS_EXPIRE_MS,
                        replaces,
                    )
                    .await
                {
                    self.progress = Some(ProgressSlot::Active {
                        request_id: id,
                        notif_id,
                        summary,
                        request_body,
                        last_status: None,
                        last_sent: None,
                    });
                }
            }
            Presentation::ProgressUpdate { id, status } => {
                self.progress_update(id, status).await;
            }
            Presentation::DismissProgress { id } => {
                let matches = matches!(
                    &self.progress,
                    Some(ProgressSlot::Active { request_id, .. }) if *request_id == id
                );
                if matches && let Some(replaces) = self.take_progress_id() {
                    let _ = self
                        .notify("Done", "", Urgency::Low, 1_000, Some(replaces))
                        .await;
                }
            }
            Presentation::Answer {
                id,
                text,
                denied,
                profile,
            } => self.answer(id, &text, &denied, profile.as_deref()).await,
            Presentation::TaskComplete {
                id,
                text,
                denied,
                profile,
                ..
            } => {
                self.task_complete(id, &text, &denied, profile.as_deref())
                    .await;
            }
            Presentation::Failure {
                reason,
                message,
                profile,
                background: true,
                ..
            } => {
                if reason == FailReason::Cancelled {
                    return;
                }
                let summary =
                    with_profile(&format!("Task: {}", reason.headline()), profile.as_deref());
                self.notify(
                    &summary,
                    &escape_markup(&message),
                    Urgency::Normal,
                    FAILURE_EXPIRE_MS,
                    None,
                )
                .await;
            }
            Presentation::Failure {
                reason: reason @ (FailReason::Cancelled | FailReason::Superseded),
                background: false,
                ..
            } => {
                // Short-lived, so a popup does not linger when the newer
                // request shows none (one asked with `wait`). A superseded
                // request's popup id is kept for the next progress popup,
                // which then replaces this one in place.
                if let Some(replaces) = self.take_progress_id() {
                    let shown = self
                        .notify(
                            reason.headline(),
                            "",
                            Urgency::Low,
                            CANCELLED_EXPIRE_MS,
                            Some(replaces),
                        )
                        .await;
                    if reason == FailReason::Superseded {
                        self.progress = shown.map(ProgressSlot::Reusable);
                    }
                }
            }
            Presentation::Notice {
                summary,
                body,
                urgency,
                replaces_id,
            } => {
                let replaces = match (&self.progress, replaces_id) {
                    (
                        Some(ProgressSlot::Active {
                            request_id,
                            notif_id,
                            ..
                        }),
                        Some(target),
                    ) if *request_id == target => {
                        let id = *notif_id;
                        self.progress = None;
                        self.pending = None;
                        Some(id)
                    }
                    _ => None,
                };
                let expire = match urgency {
                    Urgency::Low => 4_000,
                    Urgency::Normal => FAILURE_EXPIRE_MS,
                };
                self.notify(&summary, &escape_markup(&body), urgency, expire, replaces)
                    .await;
            }
            Presentation::Failure {
                reason,
                message,
                profile,
                background: false,
                ..
            } => {
                let replaces = self.take_progress_id();
                let summary = with_profile(reason.headline(), profile.as_deref());
                self.notify(
                    &summary,
                    &escape_markup(&message),
                    Urgency::Normal,
                    FAILURE_EXPIRE_MS,
                    replaces,
                )
                .await;
            }
        }
    }

    async fn progress_update(&mut self, id: u64, status: String) {
        if !self.config.progress || status.is_empty() {
            return;
        }
        let Some(ProgressSlot::Active {
            request_id,
            last_status,
            last_sent,
            ..
        }) = &self.progress
        else {
            return;
        };
        if *request_id != id {
            return;
        }
        if last_status.as_deref() == Some(status.as_str()) {
            self.pending = None;
            return;
        }
        if let Some(last) = *last_sent {
            let due = last + self.progress_interval;
            if Instant::now() < due {
                self.pending = Some(PendingUpdate {
                    request_id: id,
                    status,
                    due,
                });
                return;
            }
        }
        self.pending = None;
        self.send_progress_update(id, status).await;
    }

    async fn flush_pending(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let matches_active = matches!(
            &self.progress,
            Some(ProgressSlot::Active {
                request_id,
                last_status,
                ..
            }) if *request_id == pending.request_id
                && last_status.as_deref() != Some(pending.status.as_str())
        );
        if matches_active {
            self.send_progress_update(pending.request_id, pending.status)
                .await;
        }
    }

    async fn send_progress_update(&mut self, id: u64, status: String) {
        let Some(ProgressSlot::Active {
            request_id,
            notif_id,
            summary,
            request_body,
            ..
        }) = &self.progress
        else {
            return;
        };
        if *request_id != id {
            return;
        }
        let replaces = *notif_id;
        let summary = summary.clone();
        let body = format_progress_body(request_body, &status);
        let shown = self
            .notify(
                &summary,
                &body,
                Urgency::Low,
                PROGRESS_EXPIRE_MS,
                Some(replaces),
            )
            .await;
        let now = Instant::now();
        if let Some(ProgressSlot::Active {
            request_id,
            notif_id,
            last_status,
            last_sent,
            ..
        }) = &mut self.progress
            && *request_id == id
        {
            *last_sent = Some(now);
            if let Some(new_id) = shown {
                *notif_id = new_id;
                *last_status = Some(status);
            }
        }
    }

    async fn answer(&mut self, id: u64, text: &str, denied: &[String], profile: Option<&str>) {
        let replaces = self.take_progress_id();
        let refused = if denied.is_empty() {
            String::new()
        } else {
            format!("Refused: {}", denied.join(", "))
        };

        if is_short(text, Limits::from(&self.config)) {
            let mut body = to_markup(text);
            if !refused.is_empty() {
                body.push_str("\n\n");
                body.push_str(&escape_markup(&refused));
            }
            let summary = with_profile("Answer", profile);
            self.notify(&summary, &body, Urgency::Normal, 0, replaces)
                .await;
            return;
        }

        let document = format_document(text, denied);
        let file = match write_answer(&self.answers_dir, id, &document, self.config.keep_answers) {
            Ok(file) => file,
            Err(e) => {
                tracing::warn!("cannot write the answer file: {e:#}");
                self.notify(
                    "Cannot show the answer",
                    &escape_markup(&format!("{e:#}")),
                    Urgency::Normal,
                    FAILURE_EXPIRE_MS,
                    replaces,
                )
                .await;
                return;
            }
        };
        tracing::info!(
            "answer {id}: {} bytes, opened from {}",
            document.len(),
            file.display()
        );

        if self.config.viewer.is_empty() {
            let body = format!("{}\n{}", headline(text, 120), file.display());
            let summary = with_profile("Answer saved", profile);
            self.notify(
                &summary,
                &escape_markup(&body),
                Urgency::Normal,
                0,
                replaces,
            )
            .await;
            return;
        }

        let viewer = match spawn_viewer(&self.config.viewer, &file) {
            Ok(viewer) => viewer,
            Err(e) => {
                report_viewer_failure(&self.config, &file, &format!("{e:#}"), replaces).await;
                return;
            }
        };
        let summary = with_profile("Answer opened", profile);
        let opened_id = self
            .notify(
                &summary,
                &escape_markup(&headline(text, 120)),
                Urgency::Low,
                0,
                replaces,
            )
            .await;
        // Watch for an early exit in the background so "Thinking…" is replaced
        // at once and the worker is not blocked on a viewer that stays running.
        let config = self.config.clone();
        let grace = self.viewer_grace;
        self.watchers
            .spawn(watch_viewer(config, viewer, file, opened_id, grace));
    }

    async fn task_complete(
        &mut self,
        id: u64,
        text: &str,
        denied: &[String],
        profile: Option<&str>,
    ) {
        if !self.config.notifications || self.config.notifier.is_empty() {
            return;
        }
        let summary = with_profile("Task complete", profile);
        let refused = if denied.is_empty() {
            String::new()
        } else {
            format!("Refused: {}", denied.join(", "))
        };
        let mut body = if is_short(text, Limits::from(&self.config)) {
            to_markup(text)
        } else {
            escape_markup(&format!("#{id} · {}", headline(text, 120)))
        };
        if !refused.is_empty() {
            body.push_str("\n\n");
            body.push_str(&escape_markup(&refused));
        }

        if self.config.viewer.is_empty() || self.action_waiters.len() >= MAX_ACTION_WAITERS {
            self.notify(&summary, &body, Urgency::Normal, 0, None).await;
            return;
        }

        let config = self.config.clone();
        let answers_dir = self.answers_dir.clone();
        let text = text.to_string();
        let denied = denied.to_vec();
        let viewer_grace = self.viewer_grace;
        let action_timeout = self.action_timeout;
        let cancel_rx = self.action_cancel.subscribe();
        self.action_waiters.spawn(wait_action_notification(
            config,
            answers_dir,
            id,
            summary,
            body,
            text,
            denied,
            viewer_grace,
            action_timeout,
            cancel_rx,
        ));
    }

    /// Show one popup; returns its id so the next one can replace it.
    /// `expire_ms` zero leaves the expiry to the notification server.
    async fn notify(
        &self,
        summary: &str,
        body: &str,
        urgency: Urgency,
        expire_ms: u32,
        replaces: Option<u32>,
    ) -> Option<u32> {
        notify(&self.config, summary, body, urgency, expire_ms, replaces).await
    }
}

/// Run the notifier once. `body` is Pango markup and must be escaped.
/// Returns the notification's id, so a later one can replace it.
pub async fn notify(
    config: &PresentConfig,
    summary: &str,
    body: &str,
    urgency: Urgency,
    expire_ms: u32,
    replaces: Option<u32>,
) -> Option<u32> {
    {
        if !config.notifications || config.notifier.is_empty() {
            return None;
        }
        let mut command = Command::new(&config.notifier[0]);
        command
            .args(&config.notifier[1..])
            .arg(format!("--app-name={APP_NAME}"))
            .arg(match urgency {
                Urgency::Low => "--urgency=low",
                Urgency::Normal => "--urgency=normal",
            })
            .arg("--print-id");
        if expire_ms > 0 {
            command.arg(format!("--expire-time={expire_ms}"));
        }
        if let Some(id) = replaces {
            command.arg(format!("--replace-id={id}"));
        }
        command
            .arg("--")
            .arg(summary)
            .arg(body)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = match tokio::time::timeout(NOTIFIER_TIMEOUT, command.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                tracing::warn!("cannot run the notifier {}: {e}", config.notifier[0]);
                return None;
            }
            Err(_) => {
                tracing::warn!("the notifier took longer than {NOTIFIER_TIMEOUT:?}");
                return None;
            }
        };
        if !output.status.success() {
            tracing::warn!(
                "the notifier failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            return None;
        }
        String::from_utf8_lossy(&output.stdout).trim().parse().ok()
    }
}

/// Show a background-task completion popup with an `"Open"` action without
/// blocking the sequential presenter worker. Clicking `"Open"` launches the
/// configured viewer on the HTML-disarmed Markdown document.
#[allow(clippy::too_many_arguments)]
async fn wait_action_notification(
    config: PresentConfig,
    answers_dir: PathBuf,
    id: u64,
    summary: String,
    body: String,
    text: String,
    denied: Vec<String>,
    viewer_grace: Duration,
    action_timeout: Duration,
    mut cancel_rx: watch::Receiver<bool>,
) {
    let mut command = Command::new(&config.notifier[0]);
    command
        .args(&config.notifier[1..])
        .arg(format!("--app-name={APP_NAME}"))
        .arg("--urgency=normal")
        .arg("--action=open=Open")
        .arg("--")
        .arg(&summary)
        .arg(&body)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            tracing::warn!("cannot run the notifier {}: {e}", config.notifier[0]);
            return;
        }
    };

    let output = tokio::select! {
        res = child.wait_with_output() => match res {
            Ok(out) => out,
            Err(e) => {
                tracing::debug!("action notifier wait failed: {e}");
                return;
            }
        },
        () = tokio::time::sleep(action_timeout) => return,
        _ = cancel_rx.changed() => return,
    };

    if !output.status.success() {
        tracing::debug!(
            "action notifier exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return;
    }

    let chosen = String::from_utf8_lossy(&output.stdout);
    if !chosen.lines().any(|l| l.trim() == "open") {
        return;
    }

    let document = format_document(&text, &denied);
    let file = match write_answer(&answers_dir, id, &document, config.keep_answers) {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!("cannot write task answer file: {e:#}");
            return;
        }
    };
    let viewer = match spawn_viewer(&config.viewer, &file) {
        Ok(viewer) => viewer,
        Err(e) => {
            report_viewer_failure(&config, &file, &format!("{e:#}"), None).await;
            return;
        }
    };
    watch_viewer(config, viewer, file, None, viewer_grace).await;
}

/// Format an answer as Markdown for the viewer, disarming raw HTML and remote
/// images so the web view never runs scripts or fetches network resources.
fn format_document(text: &str, denied: &[String]) -> String {
    let mut document = text.trim().to_string();
    if !denied.is_empty() {
        document.push_str("\n\n---\n\nRefused: ");
        document.push_str(&denied.join(", "));
    }
    document.push('\n');
    disarm_html(&document)
}

/// Write a long answer where the viewer can read it, and drop old ones.
fn write_answer(dir: &Path, id: u64, document: &str, keep: usize) -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    // An existing directory keeps its mode; make sure it is private.
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restricting {}", dir.display()))?;

    let base_stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let mut file = None;
    for offset in 0..64u128 {
        let candidate = dir.join(format!("answer-{}-{id}.md", base_stamp + offset));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(mut handle) => {
                handle
                    .write_all(document.as_bytes())
                    .with_context(|| format!("writing {}", candidate.display()))?;
                file = Some(candidate);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", candidate.display())),
        }
    }
    let file = file.ok_or_else(|| anyhow::anyhow!("cannot allocate a unique answer file"))?;
    prune(dir, keep.max(1));
    Ok(file)
}

/// Keep the newest `keep` answer files. Names sort by time.
fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut answers: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("answer-") && n.ends_with(".md"))
        })
        .collect();
    answers.sort_by_key(|p| answer_stamp(p));
    let excess = answers.len().saturating_sub(keep);
    for old in &answers[..excess] {
        if let Err(e) = std::fs::remove_file(old) {
            tracing::debug!("cannot remove {}: {e}", old.display());
        }
    }
}

fn answer_stamp(path: &Path) -> (u128, u64) {
    let name = path.file_stem().and_then(|n| n.to_str()).unwrap_or("");
    let mut parts = name.trim_start_matches("answer-").splitn(2, '-');
    let stamp = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let id = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (stamp, id)
}

struct SpawnedViewer {
    program: String,
    child: tokio::process::Child,
    log: std::fs::File,
    started: Instant,
}

/// Create an unlinked `0600` file for a detached child's stderr so writes after
/// this process exits never kill the child with SIGPIPE and no temp file is left
/// behind on disk.
fn unlinked_log_file(dir: &Path, prefix: &str) -> anyhow::Result<(std::fs::File, Stdio)> {
    use std::os::unix::fs::OpenOptionsExt;

    let n = LOG_COUNTER.fetch_add(1, Ordering::Relaxed);
    let log_path = dir.join(format!(".{prefix}-{}-{n}.log", std::process::id()));
    let log = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&log_path)
        .map_err(|e| anyhow::anyhow!("creating {}: {e}", log_path.display()))?;
    let _ = std::fs::remove_file(&log_path);
    let stderr = log
        .try_clone()
        .map_err(|e| anyhow::anyhow!("sharing {}: {e}", log_path.display()))?;
    Ok((log, Stdio::from(stderr)))
}

/// Start the viewer on `file` and return the running child with its unlinked
/// stderr file so "Answer opened" can be shown at once while [`watch_viewer`]
/// checks for an early exit.
fn spawn_viewer(argv: &[String], file: &Path) -> anyhow::Result<SpawnedViewer> {
    let dir = file.parent().unwrap_or_else(|| Path::new("."));
    let (log, stderr) = unlinked_log_file(dir, "viewer-stderr")?;

    let file = file.to_string_lossy().into_owned();
    let mut args: Vec<String> = argv[1..]
        .iter()
        .map(|a| a.replace("{file}", &file))
        .collect();
    if !argv.iter().any(|a| a.contains("{file}")) {
        args.push(file.clone());
    }
    let program = argv[0].replace("{file}", &file);
    let child = Command::new(&program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        // Its own process group: a viewer is a window of its own and must not
        // get the daemon's signals.
        .process_group(0)
        .spawn()
        .map_err(|e| anyhow::anyhow!("starting {program}: {e}"))?;

    Ok(SpawnedViewer {
        program,
        child,
        log,
        started: Instant::now(),
    })
}

/// Wait up to `grace` for a spawned viewer to exit; notify only if it fails.
/// A viewer still running past `grace` is handed off to a detached reaper.
async fn watch_viewer(
    config: PresentConfig,
    mut viewer: SpawnedViewer,
    file: PathBuf,
    replaces: Option<u32>,
    grace: Duration,
) {
    let waited = match viewer.child.try_wait() {
        Ok(Some(status)) => Ok(Ok(status)),
        Ok(None) => {
            let remaining = grace.saturating_sub(viewer.started.elapsed());
            if remaining.is_zero() {
                Err(())
            } else {
                tokio::time::timeout(remaining, viewer.child.wait())
                    .await
                    .map_err(|_| ())
            }
        }
        Err(e) => Ok(Err(e)),
    };

    match waited {
        Ok(Ok(status)) if !status.success() => {
            let detail = child_failure_detail(&viewer.program, status, &mut viewer.log);
            drop(viewer.log);
            report_viewer_failure(&config, &file, &detail, replaces).await;
        }
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            drop(viewer.log);
            let detail = format!("waiting for {}: {e}", viewer.program);
            report_viewer_failure(&config, &file, &detail, replaces).await;
        }
        Err(()) => {
            // Still running: a window is up. Drop our stderr handle so the
            // daemon holds no extra fd, and reap the child while the daemon
            // runs; after that it is the scope's or init's.
            drop(viewer.log);
            let mut child = viewer.child;
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
}

/// Always open an answer in the Markdown viewer (disarming raw HTML and remote
/// images), regardless of whether it would normally fit in a notification.
pub async fn open_answer(
    config: &PresentConfig,
    answers_dir: &Path,
    id: u64,
    text: &str,
    denied: &[String],
) -> anyhow::Result<()> {
    open_answer_with_grace(config, answers_dir, id, text, denied, VIEWER_GRACE).await
}

async fn open_answer_with_grace(
    config: &PresentConfig,
    answers_dir: &Path,
    id: u64,
    text: &str,
    denied: &[String],
    grace: Duration,
) -> anyhow::Result<()> {
    if config.viewer.is_empty() {
        notify(
            config,
            "Cannot open the viewer",
            &escape_markup("No viewer is configured (set present.viewer)."),
            Urgency::Normal,
            FAILURE_EXPIRE_MS,
            None,
        )
        .await;
        anyhow::bail!("no viewer is configured; set present.viewer in config.toml");
    }

    let document = format_document(text, denied);
    let file = match write_answer(answers_dir, id, &document, config.keep_answers) {
        Ok(file) => file,
        Err(e) => {
            notify(
                config,
                "Cannot show the answer",
                &escape_markup(&format!("{e:#}")),
                Urgency::Normal,
                FAILURE_EXPIRE_MS,
                None,
            )
            .await;
            return Err(e);
        }
    };

    let mut viewer = match spawn_viewer(&config.viewer, &file) {
        Ok(viewer) => viewer,
        Err(e) => {
            report_viewer_failure(config, &file, &format!("{e:#}"), None).await;
            return Err(e);
        }
    };
    let opened_id = notify(
        config,
        "Answer opened",
        &escape_markup(&headline(text, 120)),
        Urgency::Low,
        0,
        None,
    )
    .await;

    let remaining = grace.saturating_sub(viewer.started.elapsed());
    let waited = if remaining.is_zero() {
        Err(())
    } else {
        tokio::time::timeout(remaining, viewer.child.wait())
            .await
            .map_err(|_| ())
    };
    match waited {
        Ok(Ok(status)) if !status.success() => {
            let detail = child_failure_detail(&viewer.program, status, &mut viewer.log);
            drop(viewer.log);
            report_viewer_failure(config, &file, &detail, opened_id).await;
            anyhow::bail!("{detail}");
        }
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => {
            drop(viewer.log);
            let detail = format!("waiting for {}: {e}", viewer.program);
            report_viewer_failure(config, &file, &detail, opened_id).await;
            anyhow::bail!("{detail}");
        }
        Err(()) => {
            // Still running: leave the detached viewer alive after CLI exit.
            drop(viewer.log);
            Ok(())
        }
    }
}

/// Pipe `text` verbatim to `config.clipboard` without hanging on Wayland
/// clipboard owners (`wl-copy` forks a background child that inherits open FDs).
pub async fn copy_to_clipboard(config: &PresentConfig, text: &str) -> anyhow::Result<()> {
    copy_to_clipboard_with_timeout(config, text, CLIPBOARD_TIMEOUT).await
}

async fn copy_to_clipboard_with_timeout(
    config: &PresentConfig,
    text: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    if config.clipboard.is_empty() {
        notify(
            config,
            "Cannot copy the answer",
            &escape_markup("No clipboard program is configured (set present.clipboard)."),
            Urgency::Normal,
            FAILURE_EXPIRE_MS,
            None,
        )
        .await;
        anyhow::bail!("no clipboard program is configured; set present.clipboard in config.toml");
    }

    let log_dir = ipc::runtime_dir().unwrap_or_else(|_| std::env::temp_dir());
    let (mut log, stderr) = unlinked_log_file(&log_dir, "clipboard-stderr")?;
    let program = &config.clipboard[0];
    let mut child = match Command::new(program)
        .args(&config.clipboard[1..])
        .stdin(Stdio::piped())
        // Never use piped stdout/stderr: wl-copy forks a background owner that
        // keeps inherited FDs open until the clipboard is replaced, which would
        // deadlock wait_with_output() or kill it with SIGPIPE on exit.
        .stdout(Stdio::null())
        .stderr(stderr)
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            let detail = format!("starting {program}: {e}");
            report_clipboard_failure(config, &detail).await;
            anyhow::bail!("{detail}");
        }
    };

    let mut stdin = child.stdin.take();
    let run = async {
        if let Some(mut input) = stdin.take() {
            if let Err(e) = input.write_all(text.as_bytes()).await
                && e.kind() != std::io::ErrorKind::BrokenPipe
            {
                return Err(anyhow::anyhow!("writing to {program}: {e}"));
            }
            let _ = input.shutdown().await;
            drop(input);
        }
        child
            .wait()
            .await
            .map_err(|e| anyhow::anyhow!("waiting for {program}: {e}"))
    };

    match tokio::time::timeout(timeout, run).await {
        Ok(Ok(status)) if status.success() => {
            drop(log);
            notify(
                config,
                "Copied last answer",
                &escape_markup(&headline(text, 120)),
                Urgency::Low,
                CANCELLED_EXPIRE_MS,
                None,
            )
            .await;
            Ok(())
        }
        Ok(Ok(status)) => {
            let detail = child_failure_detail(program, status, &mut log);
            drop(log);
            report_clipboard_failure(config, &detail).await;
            anyhow::bail!("{detail}");
        }
        Ok(Err(e)) => {
            drop(log);
            let detail = format!("{e:#}");
            report_clipboard_failure(config, &detail).await;
            Err(e)
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            drop(log);
            let detail = format!("{program} took longer than {timeout:?}");
            report_clipboard_failure(config, &detail).await;
            anyhow::bail!("{detail}");
        }
    }
}

async fn report_clipboard_failure(config: &PresentConfig, detail: &str) {
    tracing::warn!("the clipboard program failed");
    tracing::debug!("clipboard: {detail}");
    notify(
        config,
        "Cannot copy the answer",
        &escape_markup(detail),
        Urgency::Normal,
        FAILURE_EXPIRE_MS,
        None,
    )
    .await;
}

fn child_failure_detail(
    program: &str,
    status: std::process::ExitStatus,
    log: &mut std::fs::File,
) -> String {
    use std::io::{Read, Seek};

    let mut said = Vec::new();
    let _ = log.rewind();
    let _ = (&mut *log)
        .take(VIEWER_STDERR_KEPT as u64 * 4)
        .read_to_end(&mut said);
    let said = String::from_utf8_lossy(&said);
    let said = truncate(said.trim(), VIEWER_STDERR_KEPT);
    if said.is_empty() {
        format!("{program} failed ({status})")
    } else {
        format!("{program} failed ({status}): {said}")
    }
}

async fn report_viewer_failure(
    config: &PresentConfig,
    file: &Path,
    detail: &str,
    replaces: Option<u32>,
) {
    // Its stderr could quote the answer, so it stays at debug.
    tracing::warn!("the viewer did not start");
    tracing::debug!("the viewer: {detail}");
    let body = format!("{detail}\n{}", file.display());
    notify(
        config,
        "Cannot open the viewer",
        &escape_markup(&body),
        Urgency::Normal,
        FAILURE_EXPIRE_MS,
        replaces,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: Limits = Limits {
        max_chars: 230,
        max_lines: 4,
    };

    #[test]
    fn plain_short_answers_go_to_a_notification() {
        for text in [
            "Es ist 14:32.",
            "Hyprland 0.55.4, built from the release tag.",
            "Line one.\nLine two.\nLine three.\nLine four.",
            "It is sunny.\n\nIt is 20 degrees.\n\nNo rain today.",
            "One.\n\n\nTwo.\n \nThree.\n\nFour.",
            "It costs $5 and the other one $10.",
            "Set $FOO_BAR and $BAZ_QUX.",
            "PATH=$HOME/$USER",
            "Use **hyprctl** to ask the compositor.",
            "- milk\n- eggs",
        ] {
            assert!(is_short(text, LIMITS), "{text}");
        }
    }

    #[test]
    fn long_or_rich_answers_go_to_the_viewer() {
        let long = "word ".repeat(60);
        let many_lines = "a\nb\nc\nd\ne";
        for text in [
            long.as_str(),
            many_lines,
            "# Heading\nbody",
            "Intro\n## Second level",
            "```sh\nls\n```",
            "~~~\ncode\n~~~",
            "| a | b |\n|---|---|",
            "The sum is $$\\sum_i x_i$$",
            "The root is $\\sqrt{2}$.",
            "Pythagoras: $x^2 + y^2 = z^2$.",
            "When $a_i < b_i$, swap them.",
            "Euler: \\(e^{i\\pi}\\)",
        ] {
            assert!(!is_short(text, LIMITS), "{text}");
        }
    }

    #[test]
    fn inline_math_detection_handles_scripts_currency_and_shell_vars() {
        for math in [
            "$\\sqrt{2}$",
            "$a \\le b$",
            "$e^{i\\pi}$",
            "$\\$5^2$",
            "$a\\\\$",
            "$\\`a$",
            "$x^2$",
            "$x^2 + y^2 = z^2$",
            "$a_i < b_i$",
            "$x_1 + x_2 = 1$",
            "$O(n^2)$",
            "$a_{n+1} = 2a_n$",
            "($a_i < b_i$)",
            "**$x^2$**",
            "_$x^2$_",
            "$x^2$-axis",
            "$a_1$/$b_2$",
            "Lösung: $x^2$—fertig.",
            "It costs $5 for $x^2$ units.",
            "It costs $5 ($x^2$)",
            "It costs $5 ($\\sqrt{2}$)",
            "Costs \\$5, so $x^2$ applies.",
            "$a$ + $x^2$",
            "$a$+$x^2$",
            "Use `$a_b$` and $x^2$.",
            "Unclosed ` backtick before $x^2$.",
            "\\\\$x^2$",
        ] {
            assert!(has_latex_inline_math(math), "{math}");
        }

        for plain in [
            "",
            "$",
            "$$",
            "$$$",
            "$$x^2$$",
            "$x^2",
            "x^2$",
            "\\$x^2$",
            "$x^2\\$",
            "\\\\\\$x^2$",
            "Costs \\$5 and \\$10.",
            "The cost is $5 and $10.",
            "price is $5_000 to $10_000",
            "price is $5_000-$10_000",
            "$5_000/$10_000",
            "It costs $5 (C:\\tmp) and $10.",
            "PATH=$HOME/$USER",
            "PATH=$HOME_DIR/$USER_DIR",
            "$FOO_BAR and $BAZ_QUX",
            "$FOO_BAR/$BAZ_QUX",
            "${FOO_BAR}/${BAZ_QUX}",
            "${FOO_BAR} and ${BAZ_QUX}",
            "$ x^2 $",
            "$x^2 $",
            "$ x^2$",
            "$\tx^2$",
            "$\u{a0}x^2$",
            "$ \\sqrt{2} $",
            "$x^2$s",
            "$x^2$ä",
            "$a_b$1",
            "$a$ and $b$",
            "$a$—foo_bar—$",
            "`$x^2$`",
            "``$x^2$``",
            "`` ` $x^2$ ``",
            "Check `$XDG_CONFIG_HOME` and `$XDG_STATE_HOME`.",
            "It costs $5 (`$a_b$`) today.",
            "$x^2 +\ny^2$",
            "Cost: $5\nnote: a_b$",
            "$x^2\n\n+ y^2$",
        ] {
            assert!(!has_latex_inline_math(plain), "{plain}");
        }
        assert!(is_rich("$$x^2$$"));
    }

    #[test]
    fn limits_are_inclusive_and_count_characters_not_bytes() {
        let exactly = "ä".repeat(230);
        assert!(is_short(&exactly, LIMITS));
        assert!(!is_short(&format!("{exactly}ö"), LIMITS));
        assert!(is_short("  padded  \n", LIMITS));
    }

    #[test]
    fn a_hash_without_a_space_is_not_a_heading() {
        assert!(is_short("Channel #general and issue #42.", LIMITS));
        assert!(!is_rich("#hashtag"));
    }

    #[test]
    fn markup_is_escaped() {
        assert_eq!(
            escape_markup("a < b && c > \"d\" 'e'"),
            "a &lt; b &amp;&amp; c &gt; &quot;d&quot; &apos;e&apos;"
        );
        assert_eq!(
            escape_markup("<b>not bold</b>"),
            "&lt;b&gt;not bold&lt;/b&gt;"
        );
        assert_eq!(escape_markup("bell\u{7} tab\tnl\n"), "bell tab\tnl\n");
    }

    #[test]
    fn short_answers_keep_bold_and_code() {
        assert_eq!(
            to_markup("Run **hyprctl** `version` <now>"),
            "Run <b>hyprctl</b> <tt>version</tt> &lt;now&gt;"
        );
        // An unpaired marker is left alone.
        assert_eq!(to_markup("2 ** 3 is 8"), "2 ** 3 is 8");
        assert_eq!(to_markup("a `b` c `d"), "a <tt>b</tt> c `d");
        // Pairs do not reach across lines.
        assert_eq!(to_markup("**a\nb**"), "**a\nb**");
    }

    /// The tags in `markup` open and close in order, as Pango demands.
    fn nests(markup: &str) -> bool {
        let mut open = Vec::new();
        let mut rest = markup;
        while let Some(start) = rest.find('<') {
            let Some(end) = rest[start..].find('>') else {
                return false;
            };
            let tag = &rest[start + 1..start + end];
            if let Some(name) = tag.strip_prefix('/') {
                if open.pop() != Some(name) {
                    return false;
                }
            } else {
                open.push(tag);
            }
            rest = &rest[start + end + 1..];
        }
        open.is_empty()
    }

    #[test]
    fn markup_in_code_spans_is_left_alone_and_tags_nest() {
        assert_eq!(
            to_markup("In Python, `2**8` is 256 and `2**10` is 1024."),
            "In Python, <tt>2**8</tt> is 256 and <tt>2**10</tt> is 1024."
        );
        assert_eq!(
            to_markup("**bold `code** tail`"),
            "**bold <tt>code** tail</tt>"
        );
        assert_eq!(
            to_markup("Use `**/*.rs` with **ripgrep**."),
            "Use <tt>**/*.rs</tt> with <b>ripgrep</b>."
        );
        for text in [
            "In Python, `2**8` is 256 and `2**10` is 1024.",
            "**bold `code** tail`",
            "`a**` b **c` d**",
            "***x*** and `y`` z",
            "**a** `b` **c `d` e** f`",
            "<b>`x`</b> **<i>**",
        ] {
            let markup = to_markup(text);
            assert!(nests(&markup), "{text:?} gave {markup:?}");
        }
    }

    #[test]
    fn headline_takes_the_first_heading_or_line() {
        assert_eq!(headline("\n# Title\ntext", 80), "Title");
        assert_eq!(
            headline("Plain first line\n# Later", 80),
            "Plain first line"
        );
        assert_eq!(headline(&"x".repeat(10), 5), "xxxx…");
    }

    #[test]
    fn answer_files_are_private_and_pruned() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("hc-answers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut files = Vec::new();
        for id in 0..5 {
            files.push(write_answer(&dir, id, "# x\n", 3).unwrap());
            std::thread::sleep(Duration::from_millis(2));
        }
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let mode = std::fs::metadata(&files[4]).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let left = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(left, 3);
        assert!(!files[0].exists() && !files[1].exists() && files[4].exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn recording_config(calls: &Path, viewer: &[&str]) -> PresentConfig {
        PresentConfig {
            notifier: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("echo \"$*\" >> '{}'; echo 7", calls.display()),
                "sh".to_string(),
            ],
            viewer: viewer.iter().map(|a| a.to_string()).collect(),
            ..PresentConfig::default()
        }
    }

    /// Show one long answer through `Desktop` with `viewer`, a notifier that
    /// records its calls, and return the recorded calls.
    async fn show_long_answer(name: &str, viewer: &[&str]) -> String {
        let dir = std::env::temp_dir().join(format!("hc-viewer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let calls = dir.join("notify.log");
        let config = recording_config(&calls, viewer);
        let desktop = Desktop::spawn(&config, dir.join("answers"));
        desktop.present(Presentation::Answer {
            id: 1,
            text: "# Heading\n\nbody".to_string(),
            denied: vec![],
            profile: None,
        });
        desktop.finish().await;
        let recorded = std::fs::read_to_string(&calls).unwrap_or_default();
        std::fs::remove_dir_all(&dir).unwrap();
        recorded
    }

    #[tokio::test]
    async fn a_viewer_that_fails_at_once_is_reported_with_what_it_said() {
        let calls = show_long_answer(
            "fails",
            &["/bin/sh", "-c", "echo 'no display here' >&2; exit 3", "sh"],
        )
        .await;
        let lines: Vec<&str> = calls.lines().collect();
        assert_eq!(lines.len(), 3, "{calls}");
        assert!(lines[0].contains("Answer opened"), "{calls}");
        assert!(lines[1].contains("Cannot open the viewer"), "{calls}");
        assert!(lines[1].contains("--replace-id=7"), "{calls}");
        assert!(lines[1].contains("no display here"), "{calls}");
        assert!(lines[2].ends_with(".md"), "{calls}");
    }

    #[tokio::test]
    async fn a_missing_viewer_reports_failure_without_answer_opened() {
        let dir = std::env::temp_dir().join(format!("hc-viewer-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let calls = dir.join("notify.log");
        let config = recording_config(&calls, &["/nonexistent/hey-computer-viewer"]);
        let desktop = Desktop::spawn(&config, dir.join("answers"));
        desktop.present(Presentation::Progress {
            id: 1,
            request: "explain".to_string(),
            profile: None,
            thinking_label: None,
        });
        desktop.present(Presentation::Answer {
            id: 1,
            text: "# Heading\n\nbody".to_string(),
            denied: vec![],
            profile: None,
        });
        desktop.finish().await;
        let recorded = std::fs::read_to_string(&calls).unwrap_or_default();
        std::fs::remove_dir_all(&dir).unwrap();

        let lines: Vec<&str> = recorded.lines().collect();
        assert_eq!(lines.len(), 3, "{recorded}");
        assert!(lines[0].contains("Thinking…"), "{recorded}");
        assert!(lines[1].contains("Cannot open the viewer"), "{recorded}");
        assert!(lines[1].contains("--replace-id=7"), "{recorded}");
        assert!(lines[2].ends_with(".md"), "{recorded}");
        assert!(!recorded.contains("Answer opened"), "{recorded}");
    }

    #[tokio::test]
    async fn a_viewer_that_keeps_running_is_an_opened_answer() {
        let dir = std::env::temp_dir().join(format!("hc-viewer-runs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let calls = dir.join("notify.log");
        let config = recording_config(&calls, &["/bin/sh", "-c", "sleep 3", "sh"]);
        let started = Instant::now();
        let desktop = Desktop::spawn(&config, dir.join("answers"));
        desktop.present(Presentation::Progress {
            id: 1,
            request: "first".to_string(),
            profile: None,
            thinking_label: None,
        });
        desktop.present(Presentation::Answer {
            id: 1,
            text: "# Heading\n\nbody".to_string(),
            denied: vec![],
            profile: None,
        });
        desktop.present(Presentation::Progress {
            id: 2,
            request: "second".to_string(),
            profile: None,
            thinking_label: None,
        });

        // "Answer opened" and the next popup must arrive without waiting for
        // the 1.5 s viewer grace period.
        let mut recorded = String::new();
        let deadline = Instant::now() + Duration::from_millis(800);
        while Instant::now() < deadline {
            recorded = std::fs::read_to_string(&calls).unwrap_or_default();
            if recorded.lines().count() >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            started.elapsed() < Duration::from_millis(800),
            "viewer grace blocked notifications: {recorded}"
        );
        let lines: Vec<&str> = recorded.lines().collect();
        assert_eq!(lines.len(), 3, "{recorded}");
        assert!(lines[0].contains("Thinking…"), "{recorded}");
        assert!(lines[1].contains("Answer opened"), "{recorded}");
        assert!(lines[1].contains("--replace-id=7"), "{recorded}");
        assert!(lines[2].contains("Thinking…"), "{recorded}");
        assert!(!lines[2].contains("--replace-id="), "{recorded}");

        desktop.finish().await;
        assert!(started.elapsed() < Duration::from_secs(3));
        let recorded = std::fs::read_to_string(&calls).unwrap_or_default();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!recorded.contains("Cannot open the viewer"), "{recorded}");

        let calls = show_long_answer("quick", &["/bin/sh", "-c", "exit 0", "sh"]).await;
        assert!(calls.contains("Answer opened"), "{calls}");
        assert!(!calls.contains("Cannot open the viewer"), "{calls}");
    }

    #[tokio::test]
    async fn a_viewer_that_fails_after_the_grace_window_is_not_reported() {
        let dir = std::env::temp_dir().join(format!("hc-viewer-late-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let calls = dir.join("notify.log");
        let config = recording_config(
            &calls,
            &["/bin/sh", "-c", "sleep 0.35; echo late >&2; exit 2", "sh"],
        );
        let desktop = Desktop::spawn_with_options(
            &config,
            dir.join("answers"),
            Duration::from_millis(150),
            PROGRESS_UPDATE_INTERVAL,
        );
        desktop.present(Presentation::Answer {
            id: 1,
            text: "# Heading\n\nbody".to_string(),
            denied: vec![],
            profile: None,
        });
        desktop.finish().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let recorded = std::fs::read_to_string(&calls).unwrap_or_default();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(recorded.contains("Answer opened"), "{recorded}");
        assert!(!recorded.contains("Cannot open the viewer"), "{recorded}");
    }

    #[test]
    fn raw_html_is_disarmed_outside_code() {
        assert_eq!(
            disarm_html("Hi <script>alert(1)</script> <img src=http://x/a.png>\n"),
            "Hi &lt;script>alert(1)&lt;/script> &lt;img src=http://x/a.png>\n"
        );
        // Math keeps its comparison as KaTeX's \lt, in every delimiter, and
        // across the lines of a display block.
        assert_eq!(
            disarm_html("$a<b$ and $$x < y$$, \\(p<q\\) \\[r<s\\]"),
            "$a\\lt b$ and $$x \\lt  y$$, \\(p\\lt q\\) \\[r\\lt s\\]"
        );
        assert_eq!(
            disarm_html("$$\n\\sum_i x_i < n\n$$ then <i>\n"),
            "$$\n\\sum_i x_i \\lt  n\n$$ then &lt;i>\n"
        );
        // Money is not math, and HTML between two prices is still disarmed
        // (as math, which is harmless).
        assert_eq!(disarm_html("$5 or $"), "$5 or $");
        assert!(!disarm_html("$5 <img src=x> $6").contains('<'));
        assert_eq!(disarm_html("\\<b>"), "&lt;b>");
        // A remote image would be fetched as soon as the answer is shown; it
        // becomes a link. A local one stays.
        assert_eq!(
            disarm_html("![a <b> plot](https://x.example/p.png) and ![ref][1]!"),
            "[image: a &lt;b> plot](https://x.example/p.png) and [image: ref][1]!"
        );
        assert_eq!(
            disarm_html("![plot](/tmp/plot.png) ![p]( <HTTP://x/y.png>) `![x](http://y)`"),
            "![plot](/tmp/plot.png) [image: p]( <HTTP://x/y.png>) `![x](http://y)`"
        );
        // Code spans, fenced blocks and autolinks are left alone.
        assert_eq!(
            disarm_html("Use `a<b` or ``x<`y``."),
            "Use `a<b` or ``x<`y``."
        );
        let fenced = "```html\n<div>\n```\nafter <b>\n~~~~\n<p>\n~~~\nstill <in>\n~~~~\n";
        assert_eq!(
            disarm_html(fenced),
            "```html\n<div>\n```\nafter &lt;b>\n~~~~\n<p>\n~~~\nstill <in>\n~~~~\n"
        );
        assert_eq!(
            disarm_html("See <https://example.org/a?b=1> or <mailto:a@example.org>."),
            "See <https://example.org/a?b=1> or <mailto:a@example.org>."
        );
        assert_eq!(disarm_html("<https://x y>"), "&lt;https://x y>");
        // An unclosed backtick does not hide what follows.
        assert_eq!(disarm_html("a ` <b>"), "a ` &lt;b>");
    }

    #[test]
    fn data_uri_and_scheme_images_are_disarmed_while_local_paths_stay() {
        assert_eq!(
            disarm_html("![svg](data:image/svg+xml,<svg onload=alert(1)>)"),
            "[image: svg](data:image/svg+xml,&lt;svg onload=alert(1)>)"
        );
        for md in [
            "![x](DATA:image/png;base64,AA==)",
            "![x]( <data:image/svg+xml;base64,PHN2Zz4=>)",
            "![x](javascript:alert(1))",
            "![x](blob:https://x/1)",
            "![x](file:///etc/passwd)",
            "![x](//cdn.example/a.png)",
            "![x](ftp://host/a.png)",
            "![x](data&#58;image/svg+xml,a)",
            "![x](data&#x3A;image/svg+xml,a)",
            "![x](data&colon;image/svg+xml,a)",
            "![x](da&#9;ta:image/svg+xml,a)",
            "![x](https\\://evil.example/a.png)",
            "![x](\nhttps://evil.example/a.png)",
        ] {
            let out = disarm_html(md);
            assert!(out.starts_with("[image: "), "{md} -> {out}");
        }

        for local in [
            "![plot](/tmp/plot.png)",
            "![plot](./plot.png)",
            "![plot](../img/a.png)",
            "![plot](dir/a:b.png)",
            "![plot](2026-01-01T12:00.png)",
            "![plot](plot.png)",
            "![empty]()",
        ] {
            assert_eq!(disarm_html(local), local, "{local}");
        }
    }

    #[test]
    fn reference_images_including_shortcuts_follow_their_definitions() {
        // Shortcut reference images (definition after and before, normalized).
        assert_eq!(
            disarm_html("![pic]\n\n[pic]: https://evil.example/p.png\n"),
            "[image: pic]\n\n[pic]: https://evil.example/p.png\n"
        );
        assert_eq!(
            disarm_html("[pic]: data:image/svg+xml,x\n\n![pic]\n"),
            "[pic]: data:image/svg+xml,x\n\n[image: pic]\n"
        );
        assert_eq!(
            disarm_html("![  My\tPic ]\n\n[my pic]: https://evil.example/p.png\n"),
            "[image:   My\tPic ]\n\n[my pic]: https://evil.example/p.png\n"
        );
        assert_eq!(
            disarm_html("![pic]\n\n[pic]: /tmp/local.png\n"),
            "![pic]\n\n[pic]: /tmp/local.png\n"
        );

        // Collapsed and full reference images.
        assert_eq!(
            disarm_html("![pic][]\n\n[pic]: https://evil.example/p.png\n"),
            "[image: pic][]\n\n[pic]: https://evil.example/p.png\n"
        );
        assert_eq!(
            disarm_html("![local][]\n\n[local]: /tmp/p.png\n"),
            "![local][]\n\n[local]: /tmp/p.png\n"
        );
        assert_eq!(
            disarm_html("![alt][local]\n\n[local]: /tmp/p.png\n"),
            "![alt][local]\n\n[local]: /tmp/p.png\n"
        );
        assert_eq!(
            disarm_html("![alt][remote]\n\n[remote]: data:image/png;base64,AA==\n"),
            "[image: alt][remote]\n\n[remote]: data:image/png;base64,AA==\n"
        );
        assert_eq!(disarm_html("![alt][missing]"), "[image: alt][missing]");
        assert_eq!(disarm_html("![missing][]"), "[image: missing][]");

        // Undefined shortcut syntax stays untouched (e.g. Rust attributes).
        assert_eq!(
            disarm_html("![cfg(test)] and ![not-a-ref]"),
            "![cfg(test)] and ![not-a-ref]"
        );

        // Definitions inside fenced or indented code blocks do not apply.
        let fenced = "![ref]\n\n```\n[ref]: https://evil.example/p.png\n```\n";
        assert_eq!(disarm_html(fenced), fenced);
        let indented = "![ref]\n\n    [ref]: https://evil.example/p.png\n";
        assert_eq!(disarm_html(indented), indented);

        // Blockquoted, list-nested, and multi-line definitions do apply.
        assert_eq!(
            disarm_html("![ref]\n\n> [ref]: https://evil.example/p.png\n"),
            "[image: ref]\n\n> [ref]: https://evil.example/p.png\n"
        );
        assert_eq!(
            disarm_html("![ref]\n\n- [ref]: data:image/svg+xml,x\n"),
            "[image: ref]\n\n- [ref]: data:image/svg+xml,x\n"
        );
        assert_eq!(
            disarm_html("![ref]\n\n[ref]:\n  https://evil.example/p.png\n"),
            "[image: ref]\n\n[ref]:\n  https://evil.example/p.png\n"
        );

        // Conflicting duplicate definitions fail closed.
        assert_eq!(
            disarm_html("![ref]\n\n[ref]: /tmp/a.png\n[ref]: https://evil.example/a.png\n"),
            "[image: ref]\n\n[ref]: /tmp/a.png\n[ref]: https://evil.example/a.png\n"
        );
    }

    #[test]
    fn brackets_and_protected_spans_in_image_disarming() {
        assert_eq!(
            disarm_html(r"![a \[b\]](https://evil.example/a.png)"),
            r"[image: a \[b\]](https://evil.example/a.png)"
        );
        assert_eq!(
            disarm_html(r"![a \\](https://evil.example/a.png)"),
            r"[image: a \\](https://evil.example/a.png)"
        );
        assert_eq!(
            disarm_html("![a [b]](https://evil.example/a.png)"),
            "[image: a [b]](https://evil.example/a.png)"
        );
        assert_eq!(
            disarm_html("![`]` ](https://evil.example/a.png)"),
            "[image: `]` ](https://evil.example/a.png)"
        );
        assert_eq!(
            disarm_html("![outer ![inner](https://x/in.png)](/tmp/a.png)"),
            "![outer [image: inner](https://x/in.png)](/tmp/a.png)"
        );
        assert_eq!(
            disarm_html("![outer ![inner](/tmp/a.png)](https://x/out.png)"),
            "[image: outer ![inner](/tmp/a.png)](https://x/out.png)"
        );
        assert_eq!(
            disarm_html("`![x](data:image/svg+xml,x)` and `![ref]`\n\n[ref]: https://x/a.png\n"),
            "`![x](data:image/svg+xml,x)` and `![ref]`\n\n[ref]: https://x/a.png\n"
        );
    }

    #[tokio::test]
    async fn progress_updates_replace_in_place_and_escape_markup() {
        let dir = std::env::temp_dir().join(format!("hc-prog-basic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let calls = dir.join("notify.log");
        let config = recording_config(&calls, &[]);
        let desktop = Desktop::spawn_with_options(
            &config,
            dir.join("answers"),
            VIEWER_GRACE,
            Duration::from_millis(120),
        );
        desktop.present(Presentation::Progress {
            id: 1,
            request: "check <lights>".to_string(),
            profile: Some("deep".to_string()),
            thinking_label: Some("Pondering Hard…".to_string()),
        });
        desktop.present(Presentation::ProgressUpdate {
            id: 1,
            status: "Check <a&b>".to_string(),
        });
        // Let the immediate update run before sending Answer.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if std::fs::read_to_string(&calls)
                .unwrap_or_default()
                .contains("Check &lt;a&amp;b&gt;")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        desktop.present(Presentation::Answer {
            id: 1,
            text: "Done.".to_string(),
            denied: vec![],
            profile: Some("deep".to_string()),
        });
        desktop.finish().await;
        let recorded = std::fs::read_to_string(&calls).unwrap_or_default();
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(recorded.contains("--urgency=low"), "{recorded}");
        assert!(recorded.contains("--replace-id=7"), "{recorded}");
        assert!(recorded.contains("Pondering Hard…"), "{recorded}");
        assert!(!recorded.contains("Thinking… (deep)"), "{recorded}");
        assert!(
            recorded.contains("check &lt;lights&gt;\n<b>Check &lt;a&amp;b&gt;</b>"),
            "{recorded}"
        );
        assert!(recorded.contains("Answer (deep) Done."), "{recorded}");
    }

    #[tokio::test]
    async fn progress_updates_are_throttled_and_coalesced() {
        let dir = std::env::temp_dir().join(format!("hc-prog-throttle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let calls = dir.join("notify.log");
        let config = recording_config(&calls, &[]);
        let desktop = Desktop::spawn_with_options(
            &config,
            dir.join("answers"),
            VIEWER_GRACE,
            Duration::from_millis(150),
        );
        desktop.present(Presentation::Progress {
            id: 1,
            request: "work".to_string(),
            profile: None,
            thinking_label: None,
        });
        desktop.present(Presentation::ProgressUpdate {
            id: 1,
            status: "step 1".to_string(),
        });
        // Wait for step 1 to be sent immediately.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if std::fs::read_to_string(&calls)
                .unwrap_or_default()
                .contains("step 1")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Burst within the throttle window: only the last status should flush.
        desktop.present(Presentation::ProgressUpdate {
            id: 1,
            status: "step 2".to_string(),
        });
        desktop.present(Presentation::ProgressUpdate {
            id: 1,
            status: "step 3".to_string(),
        });
        desktop.present(Presentation::ProgressUpdate {
            id: 1,
            status: "step 4".to_string(),
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if std::fs::read_to_string(&calls)
                .unwrap_or_default()
                .contains("step 4")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        desktop.finish().await;
        let recorded = std::fs::read_to_string(&calls).unwrap_or_default();
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(recorded.contains("<b>step 1</b>"), "{recorded}");
        assert!(!recorded.contains("step 2"), "{recorded}");
        assert!(!recorded.contains("step 3"), "{recorded}");
        assert!(recorded.contains("<b>step 4</b>"), "{recorded}");
    }

    #[tokio::test]
    async fn progress_updates_never_overwrite_terminal_or_resurrect() {
        let dir = std::env::temp_dir().join(format!("hc-prog-safe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let calls = dir.join("notify.log");
        let config = recording_config(&calls, &[]);
        let desktop = Desktop::spawn_with_options(
            &config,
            dir.join("answers"),
            VIEWER_GRACE,
            Duration::from_millis(200),
        );
        desktop.present(Presentation::Progress {
            id: 1,
            request: "first".to_string(),
            profile: None,
            thinking_label: None,
        });
        desktop.present(Presentation::ProgressUpdate {
            id: 1,
            status: "step 1".to_string(),
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if std::fs::read_to_string(&calls)
                .unwrap_or_default()
                .contains("step 1")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Schedule a throttled update, then preempt with Answer and a late update.
        desktop.present(Presentation::ProgressUpdate {
            id: 1,
            status: "should be dropped".to_string(),
        });
        desktop.present(Presentation::Answer {
            id: 1,
            text: "Finished.".to_string(),
            denied: vec![],
            profile: None,
        });
        desktop.present(Presentation::ProgressUpdate {
            id: 1,
            status: "late zombie".to_string(),
        });
        // Superseded keeps Reusable(id), which must NOT accept ProgressUpdate.
        desktop.present(Presentation::Progress {
            id: 2,
            request: "second".to_string(),
            profile: None,
            thinking_label: None,
        });
        desktop.present(Presentation::Failure {
            id: 2,
            reason: FailReason::Superseded,
            message: "replaced".to_string(),
            profile: None,
            background: false,
        });
        desktop.present(Presentation::ProgressUpdate {
            id: 2,
            status: "after superseded".to_string(),
        });
        tokio::time::sleep(Duration::from_millis(250)).await;
        desktop.finish().await;
        let recorded = std::fs::read_to_string(&calls).unwrap_or_default();
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(recorded.contains("Finished."), "{recorded}");
        assert!(!recorded.contains("should be dropped"), "{recorded}");
        assert!(!recorded.contains("late zombie"), "{recorded}");
        assert!(!recorded.contains("after superseded"), "{recorded}");
    }

    #[tokio::test]
    async fn progress_updates_respect_missing_print_id_and_transient_errors() {
        // Without --print-id output, progress updates must not spam new popups.
        let dir = std::env::temp_dir().join(format!("hc-prog-noid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let calls = dir.join("notify.log");
        let config = PresentConfig {
            notifier: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("echo \"$*\" >> '{}'", calls.display()),
                "sh".to_string(),
            ],
            ..PresentConfig::default()
        };
        let desktop = Desktop::spawn_with_options(
            &config,
            dir.join("answers"),
            VIEWER_GRACE,
            Duration::from_millis(50),
        );
        desktop.present(Presentation::Progress {
            id: 1,
            request: "hi".to_string(),
            profile: None,
            thinking_label: None,
        });
        desktop.present(Presentation::ProgressUpdate {
            id: 1,
            status: "Reading file…".to_string(),
        });
        desktop.present(Presentation::Answer {
            id: 1,
            text: "ok".to_string(),
            denied: vec![],
            profile: None,
        });
        desktop.finish().await;
        let recorded = std::fs::read_to_string(&calls).unwrap_or_default();
        assert!(!recorded.contains("Reading file…"), "{recorded}");
        assert!(recorded.contains("Thinking…"), "{recorded}");
        assert!(recorded.contains("Answer ok"), "{recorded}");

        // A transient notifier failure on ProgressUpdate must keep notif_id for Answer.
        let calls2 = dir.join("notify2.log");
        let count_file = dir.join("count");
        let config2 = PresentConfig {
            notifier: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!(
                    "n=$(cat '{}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{}'; echo \"$*\" >> '{}'; if [ \"$n\" -eq 2 ]; then exit 1; fi; echo 7",
                    count_file.display(),
                    count_file.display(),
                    calls2.display()
                ),
                "sh".to_string(),
            ],
            ..PresentConfig::default()
        };
        let desktop2 = Desktop::spawn_with_options(
            &config2,
            dir.join("answers2"),
            VIEWER_GRACE,
            Duration::from_millis(50),
        );
        desktop2.present(Presentation::Progress {
            id: 1,
            request: "hi".to_string(),
            profile: None,
            thinking_label: None,
        });
        desktop2.present(Presentation::ProgressUpdate {
            id: 1,
            status: "Running curl…".to_string(),
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if std::fs::read_to_string(&calls2)
                .unwrap_or_default()
                .contains("Running curl…")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        desktop2.present(Presentation::Answer {
            id: 1,
            text: "ok".to_string(),
            denied: vec![],
            profile: None,
        });
        desktop2.finish().await;
        let recorded2 = std::fs::read_to_string(&calls2).unwrap_or_default();
        std::fs::remove_dir_all(&dir).unwrap();
        let last = recorded2.lines().last().unwrap_or("");
        assert!(last.contains("Answer ok"), "{recorded2}");
        assert!(last.contains("--replace-id=7"), "{recorded2}");
    }

    #[tokio::test]
    async fn open_answer_disarms_html_and_opens_short_answers() {
        let dir = std::env::temp_dir().join(format!("hc-open-answer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let calls = dir.join("notify.log");
        let opened = dir.join("opened.md");
        let viewer_cmd = format!("cp \"$1\" '{}'", opened.display());
        let config = recording_config(&calls, &["/bin/sh", "-c", &viewer_cmd, "sh", "{file}"]);

        // Short answer with raw HTML must still open the viewer and be disarmed.
        open_answer_with_grace(
            &config,
            &dir.join("answers"),
            5,
            "hi <script>alert(1)</script>",
            &["sudo id".to_string()],
            Duration::from_millis(200),
        )
        .await
        .unwrap();

        let rendered = std::fs::read_to_string(&opened).unwrap();
        assert!(
            rendered.contains("&lt;script>alert(1)&lt;/script>"),
            "{rendered}"
        );
        assert!(rendered.contains("Refused: sudo id"), "{rendered}");
        let notified = std::fs::read_to_string(&calls).unwrap();
        assert!(notified.contains("Answer opened"), "{notified}");

        // Failing viewer reports failure and returns Err.
        let bad_config =
            recording_config(&calls, &["/bin/sh", "-c", "echo 'boom' >&2; exit 2", "sh"]);
        assert!(
            open_answer_with_grace(
                &bad_config,
                &dir.join("answers"),
                6,
                "hello",
                &[],
                Duration::from_millis(200),
            )
            .await
            .is_err()
        );
        let notified = std::fs::read_to_string(&calls).unwrap();
        assert!(notified.contains("Cannot open the viewer"), "{notified}");

        // Empty viewer fails cleanly.
        let empty_config = recording_config(&calls, &[]);
        assert!(
            open_answer_with_grace(
                &empty_config,
                &dir.join("answers"),
                7,
                "hello",
                &[],
                Duration::from_millis(200),
            )
            .await
            .is_err()
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn copy_to_clipboard_handles_forked_background_owner_and_errors() {
        let dir = std::env::temp_dir().join(format!("hc-clipboard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let calls = dir.join("notify.log");
        let captured = dir.join("clip.txt");

        // Simulates wl-copy: reads stdin, forks a background child holding
        // inherited stdout/stderr FDs, and exits 0 immediately.
        let mut config = recording_config(&calls, &[]);
        config.clipboard = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("cat > '{}'; (sleep 3) & exit 0", captured.display()),
            "sh".to_string(),
        ];

        let started = Instant::now();
        copy_to_clipboard_with_timeout(&config, "echo <raw>", Duration::from_secs(2))
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(800),
            "blocked on forked background clipboard owner"
        );
        assert_eq!(std::fs::read_to_string(&captured).unwrap(), "echo <raw>");
        let notified = std::fs::read_to_string(&calls).unwrap();
        assert!(notified.contains("Copied last answer"), "{notified}");

        // Non-zero exit captures stderr and notifies.
        config.clipboard = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo 'no wayland' >&2; exit 1".to_string(),
            "sh".to_string(),
        ];
        let err = copy_to_clipboard_with_timeout(&config, "hello", Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no wayland"), "{err}");

        // Hanging clipboard program times out cleanly.
        config.clipboard = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 5".to_string(),
            "sh".to_string(),
        ];
        assert!(
            copy_to_clipboard_with_timeout(&config, "hello", Duration::from_millis(120))
                .await
                .is_err()
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn task_complete_action_opens_viewer_without_blocking_interactive_popups() {
        let dir = std::env::temp_dir().join(format!("hc-task-action-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let calls = dir.join("notify.log");
        let opened = dir.join("opened.md");

        let notifier_script = format!(
            "echo \"$*\" >> '{}'; case \"$*\" in *--action=open=Open*) sleep 0.25; echo open ;; *) echo 7 ;; esac",
            calls.display()
        );
        let viewer_cmd = format!("cp \"$1\" '{}'", opened.display());
        let config = PresentConfig {
            notifier: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                notifier_script,
                "sh".to_string(),
            ],
            viewer: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                viewer_cmd,
                "sh".to_string(),
                "{file}".to_string(),
            ],
            ..PresentConfig::default()
        };

        let desktop = Desktop::spawn_full(
            &config,
            dir.join("answers"),
            Duration::from_millis(100),
            PROGRESS_UPDATE_INTERVAL,
            Duration::from_secs(5),
        );
        desktop.present(Presentation::Progress {
            id: 1,
            request: "foreground".to_string(),
            profile: None,
            thinking_label: None,
        });
        desktop.present(Presentation::TaskComplete {
            id: 2,
            request: "bg job".to_string(),
            text: "# Report\n\n<script>x</script>".to_string(),
            denied: vec![],
            profile: Some("deep".to_string()),
        });
        desktop.present(Presentation::Answer {
            id: 1,
            text: "Quick reply.".to_string(),
            denied: vec![],
            profile: None,
        });

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !opened.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(opened.exists(), "clicking Open must launch viewer");
        let content = std::fs::read_to_string(&opened).unwrap();
        assert!(content.contains("&lt;script>x&lt;/script>"), "{content}");

        // finish() must abort a hanging unclicked action waiter right away.
        let hanging_script = "case \"$*\" in *--action=open=Open*) sleep 30 ;; *) echo 7 ;; esac";
        let config2 = PresentConfig {
            notifier: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                hanging_script.to_string(),
                "sh".to_string(),
            ],
            viewer: vec!["/bin/true".to_string()],
            ..PresentConfig::default()
        };
        let desktop2 = Desktop::spawn_full(
            &config2,
            dir.join("answers2"),
            Duration::from_millis(50),
            PROGRESS_UPDATE_INTERVAL,
            Duration::from_secs(30),
        );
        desktop2.present(Presentation::TaskComplete {
            id: 3,
            request: "unclicked".to_string(),
            text: "Short answer".to_string(),
            denied: vec![],
            profile: None,
        });
        let t0 = Instant::now();
        desktop2.finish().await;
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "finish stalled on action waiter"
        );

        desktop.finish().await;
        let recorded = std::fs::read_to_string(&calls).unwrap_or_default();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(recorded.contains("--action=open=Open"), "{recorded}");
        assert!(recorded.contains("Task complete (deep)"), "{recorded}");
        assert!(
            recorded.contains("--replace-id=7 -- Answer Quick reply."),
            "{recorded}"
        );
    }

    #[test]
    fn disarm_html_closes_commonmark_backtick_fence_multiline_and_dollar_bypasses() {
        // 1. Exact backtick-run matching (` ``` ` does not close at byte 0 of ` ``` `).
        let a = disarm_html("Explain ` ``` ` and <script>alert(1)</script> `end`");
        assert!(!a.contains("<script>"), "{a}");
        assert!(a.contains("&lt;script>alert(1)&lt;/script>"), "{a}");

        // 2. Backticks inside backtick fence info strings are inline code, not fences.
        let b =
            disarm_html("```code``` <script>alert(1)</script>\n![x](https://evil.example/a.png)\n");
        assert!(!b.contains("<script>"), "{b}");
        assert!(b.contains("[image: x](https://evil.example/a.png)"), "{b}");

        // 3. Multi-line inline code spans do not invert parity on line 2, while
        //    unclosed backticks before a blank line still disarm line 2.
        let c = disarm_html("Here is `multi-line\ncode` <script>alert(1)</script> `tail`\n");
        assert!(!c.contains("<script>"), "{c}");
        let c_unclosed = disarm_html("Unclosed `tick\n<script>alert(1)</script>\n\nNext para\n");
        assert!(!c_unclosed.contains("<script>"), "{c_unclosed}");

        // 4. Currency `$` pairs do not swallow remote images or code-span backticks.
        let d = disarm_html("It costs $5 (see ![p](https://evil.example/p.png)) or $10.");
        assert!(d.contains("[image: p](https://evil.example/p.png)"), "{d}");
        let e = disarm_html("Price is $5 (`$10` in store) <script>alert(1)</script> `code`");
        assert!(!e.contains("<script>"), "{e}");
    }
}
