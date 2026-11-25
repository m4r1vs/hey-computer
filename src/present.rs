//! Showing answers: a notification when the answer is short and plain, a
//! Markdown viewer when it is long or rich.
//!
//! Everything goes through [`Presenter`], so the daemon can be tested with a
//! [`Recorder`] and never touch the desktop. The real one, [`Desktop`], runs
//! the notifier as a program (notify-send by default) rather than talking
//! D-Bus itself: an argument vector can be swapped for a script, which is how
//! the daemon is tested by hand without popping anything up.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::{AbortHandle, JoinSet};

use crate::config::{MediaDuck, Present as PresentConfig, Speak as SpeakConfig};
use crate::ipc;
use crate::protocol::{FailReason, WidgetSpec};

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
/// Fast startup error check window for detached CLI/agent viewer launches.
const DETACHED_VIEWER_GRACE: Duration = Duration::from_millis(300);
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
    /// Mid-turn status and/or tool-icon update for an in-flight interactive request.
    ProgressUpdate {
        id: u64,
        status: String,
        icons: Vec<String>,
    },
    /// Quietly dismiss any active interactive progress popup (e.g. suppressed turn).
    DismissProgress { id: u64 },
    Answer {
        id: u64,
        text: String,
        /// Commands the agent was refused on the way.
        denied: Vec<String>,
        profile: Option<String>,
        widgets: Vec<WidgetSpec>,
        icons: Vec<String>,
    },
    /// A background task finished with a non-suppressed answer.
    TaskComplete {
        id: u64,
        request: String,
        text: String,
        denied: Vec<String>,
        profile: Option<String>,
        widgets: Vec<WidgetSpec>,
        icons: Vec<String>,
    },
    /// Start streaming speech/earcons for a voice-originated foreground turn.
    SpeechStart { id: u64 },
    /// Streamed text chunk for sentence-level TTS on a voice-originated turn.
    SpeechChunk { id: u64, delta: String },
    /// Finalize speech streaming for a completed voice-originated turn.
    SpeechFinish {
        id: u64,
        full_text: String,
        has_widgets: bool,
    },
    Failure {
        id: u64,
        reason: FailReason,
        message: String,
        profile: Option<String>,
        background: bool,
        icons: Vec<String>,
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
    /// Fallback desktop notification for a persisted notification/messenger draft
    /// when SwayNC Hold-and-Enrich is not holding the card (e.g. messenger bridge
    /// DM when desktop chat app is closed). Strictly read-only: offers Copy /
    /// Edit / Dismiss actions and never auto-sends.
    DraftReady {
        id: u64,
        turn_id: Option<u64>,
        token: String,
        channel: String,
        sender: String,
        context_note: String,
        draft_text: String,
        readonly: bool,
    },
}

pub trait Presenter: Send + Sync + 'static {
    /// Queue something to show. Must not block: the daemon calls it from the
    /// task that talks to the agent.
    fn present(&self, item: Presentation);

    /// Immediately stop active or queued TTS speech (optionally scoped to `id`),
    /// returning the request ID whose speech was stopped if any.
    fn stop_speech(&self, _id: Option<u64>) -> Option<u64> {
        None
    }

    /// Update runtime spoken-answer toggle state (`present.speak`).
    fn set_speak_enabled(&self, _enabled: bool) {}

    /// Current `(speak_enabled, currently_speaking)` state.
    fn speak_status(&self) -> (bool, bool) {
        (false, false)
    }

    /// Speak arbitrary Markdown or plain text on demand, bypassing
    /// `speak_enabled` and short-answer cutoffs. Returns `(speech_id, done_rx)`
    /// where `done_rx` resolves when playback finishes or is stopped.
    fn read_aloud(
        &self,
        _text: String,
    ) -> std::result::Result<(u64, oneshot::Receiver<()>), String> {
        Err("text-to-speech is not configured".to_string())
    }
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
    speak_enabled: AtomicBool,
    speaking: AtomicBool,
    active_speech_id: AtomicU64,
    stopped_speech: Mutex<Vec<Option<u64>>>,
    next_read_aloud_id: AtomicU64,
    hold_read_aloud: AtomicBool,
    read_alouds: Mutex<Vec<(u64, String)>>,
    read_aloud_done: Mutex<HashMap<u64, oneshot::Sender<()>>>,
}

impl Recorder {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            next_read_aloud_id: AtomicU64::new(1_000_000_000),
            ..Self::default()
        })
    }

    pub fn items(&self) -> Vec<Presentation> {
        self.items.lock().map(|i| i.clone()).unwrap_or_default()
    }

    /// Recorded `stop_speech(only)` calls for assertions in integration tests.
    pub fn stopped_speech(&self) -> Vec<Option<u64>> {
        self.stopped_speech
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    /// Recorded `read_aloud` calls `(id, text)` for integration tests.
    pub fn read_alouds(&self) -> Vec<(u64, String)> {
        self.read_alouds
            .lock()
            .map(|r| r.clone())
            .unwrap_or_default()
    }

    /// When `true`, `read_aloud` stays active until `stop_speech` or
    /// `finish_read_aloud` is called.
    pub fn set_hold_read_aloud(&self, hold: bool) {
        self.hold_read_aloud.store(hold, Ordering::SeqCst);
    }

    /// Manually finish a held `read_aloud` in tests.
    pub fn finish_read_aloud(&self, id: u64) {
        if let Ok(mut map) = self.read_aloud_done.lock()
            && let Some(tx) = map.remove(&id)
        {
            if self.active_speech_id.load(Ordering::SeqCst) == id {
                self.speaking.store(false, Ordering::SeqCst);
                self.active_speech_id.store(0, Ordering::SeqCst);
            }
            let _ = tx.send(());
        }
    }

    /// Simulate active TTS playback for `id` (`None` clears active playback).
    pub fn simulate_speaking(&self, id: Option<u64>) {
        match id {
            Some(id) => {
                self.active_speech_id.store(id, Ordering::SeqCst);
                self.speaking.store(true, Ordering::SeqCst);
            }
            None => {
                self.speaking.store(false, Ordering::SeqCst);
                self.active_speech_id.store(0, Ordering::SeqCst);
            }
        }
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
        match &item {
            Presentation::SpeechStart { id } if self.speak_enabled.load(Ordering::SeqCst) => {
                self.active_speech_id.store(*id, Ordering::SeqCst);
                self.speaking.store(true, Ordering::SeqCst);
            }
            Presentation::SpeechChunk { id, delta }
                if self.speak_enabled.load(Ordering::SeqCst)
                    && self.active_speech_id.load(Ordering::SeqCst) == *id =>
            {
                if is_rich(delta) {
                    self.speaking.store(false, Ordering::SeqCst);
                    self.active_speech_id.store(0, Ordering::SeqCst);
                }
            }
            Presentation::SpeechFinish { id, .. }
                if self.active_speech_id.load(Ordering::SeqCst) == *id =>
            {
                self.speaking.store(false, Ordering::SeqCst);
                self.active_speech_id.store(0, Ordering::SeqCst);
            }
            _ => {}
        }
        if let Ok(mut items) = self.items.lock() {
            items.push(item);
        }
        self.changed.notify_waiters();
    }

    fn stop_speech(&self, id: Option<u64>) -> Option<u64> {
        if let Ok(mut stopped) = self.stopped_speech.lock() {
            stopped.push(id);
        }
        let current = self.active_speech_id.load(Ordering::SeqCst);
        if current == 0 || !self.speaking.load(Ordering::SeqCst) {
            return None;
        }
        if let Some(only) = id
            && only != current
        {
            return None;
        }
        self.speaking.store(false, Ordering::SeqCst);
        self.active_speech_id.store(0, Ordering::SeqCst);
        if let Ok(mut map) = self.read_aloud_done.lock()
            && let Some(tx) = map.remove(&current)
        {
            let _ = tx.send(());
        }
        Some(current)
    }

    fn set_speak_enabled(&self, enabled: bool) {
        self.speak_enabled.store(enabled, Ordering::SeqCst);
        if !enabled {
            self.speaking.store(false, Ordering::SeqCst);
            self.active_speech_id.store(0, Ordering::SeqCst);
        }
    }

    fn speak_status(&self) -> (bool, bool) {
        (
            self.speak_enabled.load(Ordering::SeqCst),
            self.speaking.load(Ordering::SeqCst),
        )
    }

    fn read_aloud(
        &self,
        text: String,
    ) -> std::result::Result<(u64, oneshot::Receiver<()>), String> {
        let id = self.next_read_aloud_id.fetch_add(1, Ordering::SeqCst) + 1;
        if let Ok(mut calls) = self.read_alouds.lock() {
            calls.push((id, text));
        }
        let (tx, rx) = oneshot::channel();
        if self.hold_read_aloud.load(Ordering::SeqCst) {
            self.active_speech_id.store(id, Ordering::SeqCst);
            self.speaking.store(true, Ordering::SeqCst);
            if let Ok(mut map) = self.read_aloud_done.lock() {
                map.insert(id, tx);
            }
        } else {
            let _ = tx.send(());
        }
        self.changed.notify_waiters();
        Ok((id, rx))
    }
}

/// Notifications through the configured notifier, long answers through the
/// configured viewer, and optional streaming spoken answers through
/// [`SpeechActor`]. A queue and one task keep popups in order, which matters
/// because each replaces the one before, while speech runs independently so
/// notifications and TTS never block each other.
pub struct Desktop {
    queue: Mutex<Option<mpsc::UnboundedSender<Presentation>>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    speech: Arc<SpeechShared>,
    speech_queue: Mutex<Option<mpsc::UnboundedSender<SpeechCmd>>>,
    speech_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    next_read_aloud_id: AtomicU64,
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
        let speech = Arc::new(SpeechShared::new(&config.speak, Limits::from(config)));
        let (speech_tx, speech_rx) = mpsc::unbounded_channel();
        let speech_actor_shared = Arc::clone(&speech);
        let speech_task = tokio::spawn(async move {
            run_speech_actor(speech_actor_shared, speech_rx).await;
        });

        let mut worker = Worker {
            config: config.clone(),
            answers_dir,
            progress: None,
            pending: None,
            watchers: JoinSet::new(),
            next_waiter_seq: 0,
            action_lru: VecDeque::new(),
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

                if let Presentation::ProgressUpdate { id, status, icons } = item {
                    let mut latest_id = id;
                    let mut latest_status = (!status.is_empty()).then_some(status);
                    let mut latest_icons = icons;
                    let mut preempted = false;
                    loop {
                        match items.try_recv() {
                            Ok(Presentation::ProgressUpdate { id, status, icons }) => {
                                if id != latest_id {
                                    latest_id = id;
                                    latest_status = (!status.is_empty()).then_some(status);
                                    latest_icons = icons;
                                } else {
                                    // Preserve an un-flushed status string if an icon-only
                                    // update (`status == ""`) is coalesced behind it.
                                    if !status.is_empty() {
                                        latest_status = Some(status);
                                    }
                                    latest_icons = icons;
                                }
                            }
                            Ok(other) => {
                                // A queued Answer, Failure, or newer Progress
                                // preempts any still-queued ProgressUpdate at once.
                                deferred = Some(other);
                                preempted = true;
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    if !preempted {
                        worker
                            .progress_update(
                                latest_id,
                                latest_status.unwrap_or_default(),
                                latest_icons,
                            )
                            .await;
                    }
                    continue;
                }

                worker.show(item).await;
            }
            // Let any viewer started in the last VIEWER_GRACE finish its
            // early-exit check before exit; long-running viewers are already
            // handed off to detached reapers.
            while worker.watchers.join_next().await.is_some() {}
            // Never stall daemon shutdown on unclicked action notifications.
            let _ = worker.action_cancel.send(true);
            worker.action_lru.clear();
            worker.action_waiters.abort_all();
            while worker.action_waiters.join_next().await.is_some() {}
        });
        Arc::new(Self {
            queue: Mutex::new(Some(queue)),
            task: Mutex::new(Some(task)),
            speech,
            speech_queue: Mutex::new(Some(speech_tx)),
            speech_task: Mutex::new(Some(speech_task)),
            next_read_aloud_id: AtomicU64::new(1_000_000_000),
        })
    }

    /// Show what is queued, then stop taking more.
    pub async fn finish(&self) {
        drop(self.queue.lock().ok().and_then(|mut q| q.take()));
        drop(self.speech_queue.lock().ok().and_then(|mut q| q.take()));
        let task = self.task.lock().ok().and_then(|mut t| t.take());
        if let Some(task) = task {
            let _ = task.await;
        }
        let speech_task = self.speech_task.lock().ok().and_then(|mut t| t.take());
        if let Some(speech_task) = speech_task {
            let _ = speech_task.await;
        }
    }
}

impl Presenter for Desktop {
    fn present(&self, item: Presentation) {
        match item {
            Presentation::SpeechStart { id } => {
                if !self.speech.can_speak() {
                    return;
                }
                self.speech.kill_active_pgid();
                let epoch = self.speech.epoch.fetch_add(1, Ordering::SeqCst) + 1;
                self.speech.active_turn_id.store(id, Ordering::SeqCst);
                self.speech.speaking.store(false, Ordering::SeqCst);
                if let Ok(q) = self.speech_queue.lock()
                    && let Some(tx) = q.as_ref()
                {
                    let _ = tx.send(SpeechCmd::Start { id, epoch });
                }
                return;
            }
            Presentation::SpeechChunk { id, delta } => {
                if !self.speech.can_speak()
                    || self.speech.active_turn_id.load(Ordering::SeqCst) != id
                {
                    return;
                }
                let epoch = self.speech.epoch.load(Ordering::SeqCst);
                if let Ok(q) = self.speech_queue.lock()
                    && let Some(tx) = q.as_ref()
                {
                    let _ = tx.send(SpeechCmd::Chunk { id, delta, epoch });
                }
                return;
            }
            Presentation::SpeechFinish {
                id,
                full_text,
                has_widgets,
            } => {
                if self.speech.active_turn_id.load(Ordering::SeqCst) != id {
                    return;
                }
                let epoch = self.speech.epoch.load(Ordering::SeqCst);
                if let Ok(q) = self.speech_queue.lock()
                    && let Some(tx) = q.as_ref()
                {
                    let _ = tx.send(SpeechCmd::Finish {
                        id,
                        full_text,
                        has_widgets,
                        epoch,
                    });
                }
                return;
            }
            Presentation::Failure {
                id,
                background: false,
                ..
            } => {
                let _ = self.stop_speech(Some(id));
            }
            _ => {}
        }
        if let Ok(queue) = self.queue.lock()
            && let Some(queue) = queue.as_ref()
        {
            let _ = queue.send(item);
        }
    }

    fn stop_speech(&self, only: Option<u64>) -> Option<u64> {
        let current = self.speech.active_turn_id.load(Ordering::SeqCst);
        if current == 0 {
            return None;
        }
        if let Some(target) = only
            && target != current
        {
            return None;
        }
        if self
            .speech
            .active_turn_id
            .compare_exchange(current, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return None;
        }
        let epoch = self.speech.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.speech.speaking.store(false, Ordering::SeqCst);
        self.speech.kill_active_pgid();
        if let Ok(q) = self.speech_queue.lock()
            && let Some(tx) = q.as_ref()
        {
            let _ = tx.send(SpeechCmd::Stop { epoch });
        }
        Some(current)
    }

    fn set_speak_enabled(&self, enabled: bool) {
        self.speech.speak_enabled.store(enabled, Ordering::SeqCst);
        if !enabled {
            let _ = self.stop_speech(None);
        }
    }

    fn speak_status(&self) -> (bool, bool) {
        (
            self.speech.speak_enabled.load(Ordering::SeqCst),
            self.speech.speaking.load(Ordering::SeqCst),
        )
    }

    fn read_aloud(
        &self,
        text: String,
    ) -> std::result::Result<(u64, oneshot::Receiver<()>), String> {
        if !has_tts_command(&self.speech.config) {
            return Err(
                "no TTS command is configured (set present.speak.command, command_en, or command_de)"
                    .to_string(),
            );
        }
        let sentences = prepare_document_for_speech(&text);
        let id = self.next_read_aloud_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (done_tx, done_rx) = oneshot::channel();
        if sentences.is_empty() {
            let _ = done_tx.send(());
            return Ok((id, done_rx));
        }
        let lang = detect_document_lang(&sentences);
        self.speech.kill_active_pgid();
        let epoch = self.speech.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.speech.active_turn_id.store(id, Ordering::SeqCst);
        self.speech.speaking.store(true, Ordering::SeqCst);
        if let Ok(q) = self.speech_queue.lock()
            && let Some(tx) = q.as_ref()
            && tx
                .send(SpeechCmd::ReadAloud {
                    id,
                    epoch,
                    sentences,
                    lang,
                    done: done_tx,
                })
                .is_ok()
        {
            Ok((id, done_rx))
        } else {
            self.speech.speaking.store(false, Ordering::SeqCst);
            let _ = self.speech.active_turn_id.compare_exchange(
                id,
                0,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
            Err("the speech worker is stopped".to_string())
        }
    }
}

enum ProgressSlot {
    /// Active "Thinking…" popup belonging to `request_id`.
    Active {
        request_id: u64,
        notif_id: u32,
        /// Base notification summary without icons (e.g. `"Thinking…"` or `"Thinking… (deep)"`).
        base_summary: String,
        /// Tool icons currently shown in the active progress popup.
        icons: Vec<String>,
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

/// Append sequential Nerd Font tool icons to a notification summary (`"  "`
/// separator + space-separated icons, capped at `max_icons` with `" …"`).
pub fn format_summary_with_icons(base: &str, icons: &[String], max_icons: usize) -> String {
    if icons.is_empty() || max_icons == 0 {
        return base.to_string();
    }
    if icons.len() <= max_icons {
        format!("{base}  {}", icons.join(" "))
    } else {
        format!("{base}  {} …", icons[..max_icons].join(" "))
    }
}

struct PendingUpdate {
    request_id: u64,
    status: Option<String>,
    icons: Vec<String>,
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
    /// Monotonic sequence IDs for LRU eviction of `action_waiters`.
    next_waiter_seq: u64,
    /// Active action-notification waiters in oldest-to-newest order (`<= MAX_ACTION_WAITERS`).
    action_lru: VecDeque<(u64, AbortHandle)>,
    /// Detached `notify-send -A …` waiters for short answers and background tasks.
    action_waiters: JoinSet<u64>,
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
    fn with_icons(&self, base: &str, icons: &[String]) -> String {
        if !self.config.tool_icons.enabled || icons.is_empty() {
            base.to_string()
        } else {
            format_summary_with_icons(base, icons, self.config.tool_icons.max_icons)
        }
    }

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

    fn reap_action_waiters(&mut self) {
        while let Some(res) = self.action_waiters.try_join_next() {
            if let Ok(done_seq) = res {
                self.action_lru.retain(|(seq, _)| *seq != done_seq);
            }
        }
        self.action_lru.retain(|(_, handle)| !handle.is_finished());
    }

    fn spawn_action_waiter(&mut self, fut: impl std::future::Future<Output = ()> + Send + 'static) {
        self.reap_action_waiters();
        while self.action_lru.len() >= MAX_ACTION_WAITERS {
            if let Some((_, oldest_handle)) = self.action_lru.pop_front() {
                oldest_handle.abort();
            }
            while let Some(res) = self.action_waiters.try_join_next() {
                if let Ok(done_seq) = res {
                    self.action_lru.retain(|(seq, _)| *seq != done_seq);
                }
            }
        }
        self.next_waiter_seq += 1;
        let seq = self.next_waiter_seq;
        let handle = self.action_waiters.spawn(async move {
            fut.await;
            seq
        });
        self.action_lru.push_back((seq, handle));
    }

    async fn show(&mut self, item: Presentation) {
        while self.watchers.try_join_next().is_some() {}
        self.reap_action_waiters();
        if !matches!(
            item,
            Presentation::TaskComplete { .. }
                | Presentation::Notice { .. }
                | Presentation::DraftReady { .. }
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
                        base_summary: summary,
                        icons: Vec::new(),
                        request_body,
                        last_status: None,
                        last_sent: None,
                    });
                }
            }
            Presentation::ProgressUpdate { id, status, icons } => {
                self.progress_update(id, status, icons).await;
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
                widgets,
                icons,
            } => {
                self.answer(id, &text, &denied, profile.as_deref(), &widgets, &icons)
                    .await;
            }
            Presentation::TaskComplete {
                id,
                text,
                denied,
                profile,
                widgets,
                icons,
                ..
            } => {
                self.task_complete(id, &text, &denied, profile.as_deref(), &widgets, &icons)
                    .await;
            }
            Presentation::SpeechStart { .. }
            | Presentation::SpeechChunk { .. }
            | Presentation::SpeechFinish { .. } => {}
            Presentation::Failure {
                reason,
                message,
                profile,
                background: true,
                icons,
                ..
            } => {
                if reason == FailReason::Cancelled {
                    return;
                }
                let summary = self.with_icons(
                    &with_profile(&format!("Task: {}", reason.headline()), profile.as_deref()),
                    &icons,
                );
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
            Presentation::DraftReady {
                id,
                turn_id,
                token,
                channel,
                sender,
                context_note,
                draft_text,
                readonly: _,
            } => {
                self.draft_ready(
                    id,
                    turn_id,
                    &token,
                    &channel,
                    &sender,
                    &context_note,
                    &draft_text,
                )
                .await;
            }
            Presentation::Failure {
                reason,
                message,
                profile,
                background: false,
                icons,
                ..
            } => {
                let replaces = self.take_progress_id();
                let summary =
                    self.with_icons(&with_profile(reason.headline(), profile.as_deref()), &icons);
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

    async fn progress_update(&mut self, id: u64, status: String, icons: Vec<String>) {
        if !self.config.progress || (status.is_empty() && icons.is_empty()) {
            return;
        }
        let new_status = (!status.is_empty()).then_some(status);
        let merged_status = new_status.or_else(|| {
            self.pending
                .as_ref()
                .filter(|p| p.request_id == id)
                .and_then(|p| p.status.clone())
        });
        let effective_icons = if self.config.tool_icons.enabled {
            icons
        } else {
            Vec::new()
        };
        let Some(ProgressSlot::Active {
            request_id,
            icons: current_icons,
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
        let status_changed = merged_status
            .as_deref()
            .is_some_and(|s| last_status.as_deref() != Some(s));
        let icons_changed = self.config.tool_icons.enabled && effective_icons != *current_icons;
        if !status_changed && !icons_changed {
            self.pending = None;
            return;
        }
        if let Some(last) = *last_sent {
            let due = last + self.progress_interval;
            if Instant::now() < due {
                self.pending = Some(PendingUpdate {
                    request_id: id,
                    status: merged_status,
                    icons: effective_icons,
                    due,
                });
                return;
            }
        }
        self.pending = None;
        self.send_progress_update(id, merged_status, effective_icons)
            .await;
    }

    async fn flush_pending(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let matches_active = matches!(
            &self.progress,
            Some(ProgressSlot::Active {
                request_id,
                icons,
                last_status,
                ..
            }) if *request_id == pending.request_id
                && (pending
                    .status
                    .as_deref()
                    .is_some_and(|s| last_status.as_deref() != Some(s))
                    || (self.config.tool_icons.enabled && pending.icons != *icons))
        );
        if matches_active {
            self.send_progress_update(pending.request_id, pending.status, pending.icons)
                .await;
        }
    }

    async fn send_progress_update(&mut self, id: u64, status: Option<String>, icons: Vec<String>) {
        let Some(ProgressSlot::Active {
            request_id,
            notif_id,
            base_summary,
            request_body,
            last_status,
            ..
        }) = &self.progress
        else {
            return;
        };
        if *request_id != id {
            return;
        }
        let replaces = *notif_id;
        let summary = self.with_icons(base_summary, &icons);
        let effective_status = status.as_deref().or(last_status.as_deref());
        let body = match effective_status {
            Some(s) => format_progress_body(request_body, s),
            None => request_body.clone(),
        };
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
            icons: slot_icons,
            last_status,
            last_sent,
            ..
        }) = &mut self.progress
            && *request_id == id
        {
            *last_sent = Some(now);
            if let Some(new_id) = shown {
                *notif_id = new_id;
                if let Some(s) = status {
                    *last_status = Some(s);
                }
                *slot_icons = icons;
            }
        }
    }

    async fn answer(
        &mut self,
        id: u64,
        text: &str,
        denied: &[String],
        profile: Option<&str>,
        widgets: &[WidgetSpec],
        icons: &[String],
    ) {
        let replaces = self.take_progress_id();
        let refused = if denied.is_empty() {
            String::new()
        } else {
            format!("Refused: {}", denied.join(", "))
        };

        if widgets.is_empty() && is_short(text, Limits::from(&self.config)) {
            let mut body = to_markup(text);
            if !refused.is_empty() {
                body.push_str("\n\n");
                body.push_str(&escape_markup(&refused));
            }
            let summary = self.with_icons(&with_profile("Answer", profile), icons);
            let actions = short_answer_actions(&self.config);
            if actions.is_empty() {
                self.notify(&summary, &body, Urgency::Normal, 0, replaces)
                    .await;
                return;
            }
            if !self.config.notifications || self.config.notifier.is_empty() {
                return;
            }
            let cancel_rx = self.action_cancel.subscribe();
            if let Some(wait_fut) = start_foreground_action_notification(
                self.config.clone(),
                self.answers_dir.clone(),
                id,
                summary,
                body,
                replaces,
                actions,
                text.to_string(),
                denied.to_vec(),
                self.viewer_grace,
                self.action_timeout,
                cancel_rx,
            )
            .await
            {
                self.spawn_action_waiter(wait_fut);
            }
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

        let answer_head = answer_headline(text, widgets);
        if self.config.viewer.is_empty() {
            let body = format!("{answer_head}\n{}", file.display());
            let summary = self.with_icons(&with_profile("Answer saved", profile), icons);
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

        let viewer = match spawn_viewer(&self.config.viewer, &file, id, &[]) {
            Ok(viewer) => viewer,
            Err(e) => {
                report_viewer_failure(&self.config, &file, &format!("{e:#}"), replaces).await;
                return;
            }
        };
        let summary = self.with_icons(&with_profile("Answer opened", profile), icons);
        let opened_id = self
            .notify(
                &summary,
                &escape_markup(&answer_head),
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
        widgets: &[WidgetSpec],
        icons: &[String],
    ) {
        if !self.config.notifications || self.config.notifier.is_empty() {
            return;
        }
        let summary = self.with_icons(&with_profile("Task complete", profile), icons);
        let refused = if denied.is_empty() {
            String::new()
        } else {
            format!("Refused: {}", denied.join(", "))
        };
        let mut body = if widgets.is_empty() && is_short(text, Limits::from(&self.config)) {
            to_markup(text)
        } else {
            let head = answer_headline(text, widgets);
            escape_markup(&format!("#{id} · {head}"))
        };
        if !refused.is_empty() {
            body.push_str("\n\n");
            body.push_str(&escape_markup(&refused));
        }

        if self.config.viewer.is_empty() {
            self.notify(&summary, &body, Urgency::Normal, 0, None).await;
            return;
        }

        let cancel_rx = self.action_cancel.subscribe();
        if let Some(wait_fut) = start_background_action_notification(
            self.config.clone(),
            self.answers_dir.clone(),
            id,
            &summary,
            &body,
            None,
            &[("open", "Open")],
            text.to_string(),
            denied.to_vec(),
            self.viewer_grace,
            self.action_timeout,
            cancel_rx,
        ) {
            self.spawn_action_waiter(wait_fut);
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn draft_ready(
        &mut self,
        _id: u64,
        turn_id: Option<u64>,
        token: &str,
        channel: &str,
        sender: &str,
        context_note: &str,
        draft_text: &str,
    ) {
        if !self.config.notifications || self.config.notifier.is_empty() {
            return;
        }
        let summary = format!("Draft reply ({channel}) · {sender}");
        let mut body = String::new();
        let ctx = context_note.trim();
        let draft = draft_text.trim();
        if !ctx.is_empty() {
            body.push_str(&format!("✦ {}", escape_markup(ctx)));
            if !draft.is_empty() {
                body.push('\n');
            }
        }
        if !draft.is_empty() {
            body.push_str(&escape_markup(draft));
        }

        // Strictly read-only buttons: never expose `Send` on desktop fallback
        // draft cards (especially WhatsApp/Signal).
        let mut actions: Vec<(&'static str, &'static str)> = Vec::with_capacity(3);
        if !draft.is_empty() && !self.config.clipboard.is_empty() {
            actions.push(("copy", "Copy"));
        }
        if turn_id.is_some() {
            actions.push(("edit", "Edit"));
        }
        actions.push(("dismiss", "Dismiss"));

        let mut cancel_rx = self.action_cancel.subscribe();
        let config = self.config.clone();
        let action_timeout = self.action_timeout;
        let token_owned = token.to_string();
        let draft_owned = draft.to_string();
        let command = build_action_notifier_command(&config, &summary, &body, None, &actions);
        let Some((guard, stdout)) = ActionChildGuard::spawn(command, &config.notifier[0]) else {
            return;
        };
        let wait_fut = async move {
            let mut lines = BufReader::new(stdout).lines();
            let deadline = tokio::time::Instant::now() + action_timeout;
            let mut chosen: Option<String> = None;
            for _ in 0..2 {
                let next = tokio::select! {
                    res = lines.next_line() => match res {
                        Ok(Some(line)) => line,
                        Ok(None) | Err(_) => break,
                    },
                    () = tokio::time::sleep_until(deadline) => return,
                    _ = cancel_rx.changed() => return,
                };
                let trimmed = next.trim();
                if matches!(trimmed, "copy" | "edit" | "dismiss") {
                    chosen = Some(trimmed.to_string());
                    break;
                }
            }
            guard.finish_cleanly().await;
            let Some(action) = chosen else {
                return;
            };
            let draft_action = match action.as_str() {
                "copy" => crate::protocol::DraftAction::Copy,
                "edit" => crate::protocol::DraftAction::Edit,
                "dismiss" => crate::protocol::DraftAction::Dismiss,
                _ => return,
            };
            if let Ok(sock) = ipc::socket_path(None)
                && ipc::request(
                    &sock,
                    &crate::protocol::Request::DraftResume {
                        token: token_owned,
                        action: draft_action,
                    },
                )
                .await
                .is_ok()
            {
                return;
            }
            // Fallback when socket path isn't the default (e.g. custom --socket):
            match draft_action {
                crate::protocol::DraftAction::Copy | crate::protocol::DraftAction::Paste => {
                    let _ = copy_to_clipboard(&config, &draft_owned).await;
                }
                crate::protocol::DraftAction::Edit => {
                    if !draft_owned.is_empty() {
                        let _ = copy_to_clipboard(&config, &draft_owned).await;
                    }
                    let _ = open_draft_editor(&config, turn_id, &draft_owned).await;
                }
                crate::protocol::DraftAction::Dismiss => {}
            }
        };
        self.spawn_action_waiter(wait_fut);
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

fn is_notify_send(program: &str) -> bool {
    Path::new(program).file_name().and_then(|n| n.to_str()) == Some("notify-send")
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
        if is_notify_send(&config.notifier[0]) {
            command.arg("--hint=boolean:x-hey-computer:true");
        }
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

fn answer_headline(text: &str, widgets: &[WidgetSpec]) -> String {
    let head = headline(text, 120);
    if !head.is_empty() {
        head
    } else {
        widgets
            .first()
            .map(|w| truncate(w.title.trim(), 120))
            .unwrap_or_default()
    }
}

fn short_answer_actions(config: &PresentConfig) -> Vec<(&'static str, &'static str)> {
    let mut actions = Vec::with_capacity(3);
    if !config.clipboard.is_empty() {
        actions.push(("copy", "Copy"));
    }
    if !config.reply_command.is_empty() || !config.viewer.is_empty() {
        actions.push(("reply", "Reply"));
    }
    if !config.viewer.is_empty() {
        actions.push(("open", "Open"));
    }
    actions
}

struct ActionChildGuard {
    child: Option<tokio::process::Child>,
    pgid: Option<i32>,
}

impl ActionChildGuard {
    fn spawn(
        mut command: Command,
        program_name: &str,
    ) -> Option<(Self, tokio::process::ChildStdout)> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                tracing::warn!("cannot run the notifier {program_name}: {e}");
                return None;
            }
        };
        let pgid = child.id().and_then(|pid| i32::try_from(pid).ok());
        let stdout = child.stdout.take()?;
        Some((
            Self {
                child: Some(child),
                pgid,
            },
            stdout,
        ))
    }

    async fn finish_cleanly(mut self) {
        self.pgid = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.wait().await;
        }
    }
}

impl Drop for ActionChildGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let _ = child.wait().await;
                });
            }
        }
    }
}

fn build_action_notifier_command(
    config: &PresentConfig,
    summary: &str,
    body: &str,
    replaces: Option<u32>,
    actions: &[(&str, &str)],
) -> Command {
    let mut command = Command::new(&config.notifier[0]);
    command
        .args(&config.notifier[1..])
        .arg(format!("--app-name={APP_NAME}"))
        .arg("--urgency=normal")
        .arg("--print-id");
    if is_notify_send(&config.notifier[0]) {
        command.arg("--hint=boolean:x-hey-computer:true");
    }
    for (key, label) in actions {
        command.arg(format!("--action={key}={label}"));
    }
    if let Some(id) = replaces {
        command.arg(format!("--replace-id={id}"));
    }
    command.arg("--").arg(summary).arg(body);
    command
}

/// Show a short foreground answer notification with actions (`Copy` / `Reply` /
/// `Open`), reading line 1 (`--print-id`) within [`NOTIFIER_TIMEOUT`] so
/// `"Thinking…"` is replaced immediately in place, and returning a background
/// waiter future for line 2 (the clicked action, if any).
#[allow(clippy::too_many_arguments)]
async fn start_foreground_action_notification(
    config: PresentConfig,
    answers_dir: PathBuf,
    id: u64,
    summary: String,
    body: String,
    replaces: Option<u32>,
    actions: Vec<(&'static str, &'static str)>,
    text: String,
    denied: Vec<String>,
    viewer_grace: Duration,
    action_timeout: Duration,
    mut cancel_rx: watch::Receiver<bool>,
) -> Option<impl std::future::Future<Output = ()> + Send + 'static> {
    let command = build_action_notifier_command(&config, &summary, &body, replaces, &actions);
    let (guard, stdout) = ActionChildGuard::spawn(command, &config.notifier[0])?;
    let mut lines = BufReader::new(stdout).lines();

    let first_line = tokio::select! {
        res = tokio::time::timeout(NOTIFIER_TIMEOUT, lines.next_line()) => match res {
            Ok(Ok(line)) => line,
            Ok(Err(e)) => {
                tracing::warn!("reading notifier output failed: {e}");
                return None;
            }
            Err(_) => {
                tracing::warn!("the notifier took longer than {NOTIFIER_TIMEOUT:?}");
                return None;
            }
        },
        _ = cancel_rx.changed() => return None,
    };

    if first_line.is_none() {
        // Notifier exited immediately without printing an ID.
        guard.finish_cleanly().await;
        return None;
    }

    Some(async move {
        let second_line = tokio::select! {
            res = lines.next_line() => match res {
                Ok(Some(line)) => line,
                Ok(None) | Err(_) => {
                    guard.finish_cleanly().await;
                    return;
                }
            },
            () = tokio::time::sleep(action_timeout) => return,
            _ = cancel_rx.changed() => return,
        };
        guard.finish_cleanly().await;
        dispatch_notification_action(
            second_line.trim(),
            &config,
            &answers_dir,
            id,
            &text,
            &denied,
            viewer_grace,
        )
        .await;
    })
}

/// Spawn an action notification child synchronously (so queue order is preserved)
/// and return a background waiter future without blocking `Worker` on stdout.
#[allow(clippy::too_many_arguments)]
fn start_background_action_notification(
    config: PresentConfig,
    answers_dir: PathBuf,
    id: u64,
    summary: &str,
    body: &str,
    replaces: Option<u32>,
    actions: &[(&str, &str)],
    text: String,
    denied: Vec<String>,
    viewer_grace: Duration,
    action_timeout: Duration,
    mut cancel_rx: watch::Receiver<bool>,
) -> Option<impl std::future::Future<Output = ()> + Send + 'static> {
    let command = build_action_notifier_command(&config, summary, body, replaces, actions);
    let (guard, stdout) = ActionChildGuard::spawn(command, &config.notifier[0])?;
    Some(async move {
        let mut lines = BufReader::new(stdout).lines();
        let deadline = tokio::time::Instant::now() + action_timeout;

        // Real `notify-send --print-id -A …` prints the numeric notification ID on
        // line 1 and the selected action key on line 2; simple mock scripts may
        // print the action key directly on line 1.
        let mut chosen: Option<String> = None;
        for _ in 0..2 {
            let next = tokio::select! {
                res = lines.next_line() => match res {
                    Ok(Some(line)) => line,
                    Ok(None) | Err(_) => break,
                },
                () = tokio::time::sleep_until(deadline) => return,
                _ = cancel_rx.changed() => return,
            };
            let trimmed = next.trim();
            if matches!(trimmed, "copy" | "reply" | "open") {
                chosen = Some(trimmed.to_string());
                break;
            }
        }

        guard.finish_cleanly().await;
        if let Some(action) = chosen {
            dispatch_notification_action(
                &action,
                &config,
                &answers_dir,
                id,
                &text,
                &denied,
                viewer_grace,
            )
            .await;
        }
    })
}

async fn dispatch_notification_action(
    action: &str,
    config: &PresentConfig,
    answers_dir: &Path,
    id: u64,
    text: &str,
    denied: &[String],
    viewer_grace: Duration,
) {
    match action {
        "copy" => {
            let _ = copy_to_clipboard(config, text).await;
        }
        "reply" => {
            if !config.reply_command.is_empty() {
                let id_str = id.to_string();
                let program = config.reply_command[0].replace("{id}", &id_str);
                let args: Vec<String> = config.reply_command[1..]
                    .iter()
                    .map(|a| a.replace("{id}", &id_str))
                    .collect();
                let log_dir = ipc::runtime_dir().unwrap_or_else(|_| std::env::temp_dir());
                let stderr = unlinked_log_file(&log_dir, "reply-stderr")
                    .map(|(_, stdio)| stdio)
                    .unwrap_or_else(|_| Stdio::null());
                match Command::new(&program)
                    .args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(stderr)
                    .process_group(0)
                    .spawn()
                {
                    Ok(mut child) => {
                        tokio::spawn(async move {
                            let _ = child.wait().await;
                        });
                    }
                    Err(e) => {
                        tracing::warn!("cannot start reply_command {program}: {e}");
                    }
                }
            } else if !config.viewer.is_empty() {
                let is_hc_viewer = config
                    .viewer
                    .iter()
                    .any(|a| a.contains("hey-computer-viewer"))
                    || (config
                        .viewer
                        .first()
                        .is_some_and(|p| p.ends_with("hey-computer"))
                        && config.viewer.get(1).is_some_and(|s| s == "view"));
                let extra: &[&str] =
                    if is_hc_viewer && !config.viewer.iter().any(|a| a == "--focus-composer") {
                        &["--focus-composer"]
                    } else {
                        &[]
                    };
                open_disarmed_in_viewer(config, answers_dir, id, text, denied, extra, viewer_grace)
                    .await;
            }
        }
        "open" if !config.viewer.is_empty() => {
            open_disarmed_in_viewer(config, answers_dir, id, text, denied, &[], viewer_grace).await;
        }
        _ => {}
    }
}

async fn open_disarmed_in_viewer(
    config: &PresentConfig,
    answers_dir: &Path,
    id: u64,
    text: &str,
    denied: &[String],
    extra_args: &[&str],
    viewer_grace: Duration,
) {
    let document = format_document(text, denied);
    let file = match write_answer(answers_dir, id, &document, config.keep_answers) {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!("cannot write answer file: {e:#}");
            return;
        }
    };
    let viewer = match spawn_viewer(&config.viewer, &file, id, extra_args) {
        Ok(viewer) => viewer,
        Err(e) => {
            report_viewer_failure(config, &file, &format!("{e:#}"), None).await;
            return;
        }
    };
    watch_viewer(config.clone(), viewer, file, None, viewer_grace).await;
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

/// Start the viewer on `file` (expanding `{file}` and `{id}`) and return the
/// running child with its unlinked stderr file so "Answer opened" can be shown
/// at once while [`watch_viewer`] checks for an early exit.
fn spawn_viewer(
    argv: &[String],
    file: &Path,
    id: u64,
    extra_args: &[&str],
) -> anyhow::Result<SpawnedViewer> {
    let dir = file.parent().unwrap_or_else(|| Path::new("."));
    let (log, stderr) = unlinked_log_file(dir, "viewer-stderr")?;

    let file_str = file.to_string_lossy().into_owned();
    let id_str = id.to_string();
    let mut args: Vec<String> = argv[1..]
        .iter()
        .map(|a| a.replace("{file}", &file_str).replace("{id}", &id_str))
        .collect();
    for extra in extra_args {
        args.push((*extra).to_string());
    }
    if !argv.iter().any(|a| a.contains("{file}")) {
        args.push(file_str.clone());
    }
    let program = argv[0]
        .replace("{file}", &file_str)
        .replace("{id}", &id_str);
    let child = Command::new(&program)
        .args(&args)
        .env_remove("HEY_COMPUTER_AGENT_TURN")
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

/// Return a `PresentConfig` ready to open turn `turn_id` in a viewer: uses
/// `config.viewer` (with `{id}` expanded) when configured, falling back to the
/// sibling `hey-computer-viewer` binary when `WAYLAND_DISPLAY` or `DISPLAY` is set.
pub fn viewer_config_with_fallback(config: &PresentConfig, turn_id: u64) -> PresentConfig {
    let mut cfg = crate::distill::present_config_for_turn(config, turn_id);
    if cfg.viewer.is_empty() && has_graphical_display() {
        let bin = crate::distill::resolve_viewer_binary()
            .to_string_lossy()
            .into_owned();
        cfg.viewer = vec![
            bin,
            "--turn-id".to_string(),
            turn_id.to_string(),
            "{file}".to_string(),
        ];
    }
    cfg
}

fn has_graphical_display() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty())
}

/// Spawn the native viewer detached from the caller's process group (and via
/// `config.viewer`'s `systemd-run --user --scope` wrapper when configured),
/// waiting up to 300 ms to catch immediate startup errors and sending **no**
/// `"Answer opened"` notification.
///
/// Used by `hey-computer view --detach` (auto-enabled inside agent turns via
/// `HEY_COMPUTER_AGENT_TURN=1`) so the agent can open a past chat or schedule
/// without blocking its tool call or leaving the window inside the daemon cgroup.
pub async fn open_viewer_detached(
    config: &PresentConfig,
    viewer_args: &[std::ffi::OsString],
    fallback_turn: Option<(u64, &str, &[String])>,
) -> anyhow::Result<()> {
    let log_dir = ipc::runtime_dir().unwrap_or_else(|_| std::env::temp_dir());
    let (mut log, stderr) = unlinked_log_file(&log_dir, "viewer-detached-stderr")?;

    let (program, args): (std::ffi::OsString, Vec<std::ffi::OsString>) = if !config
        .viewer
        .is_empty()
    {
        if let Some(hc_idx) = config.viewer.iter().position(|arg| {
            Path::new(arg)
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == "hey-computer-viewer")
        }) {
            let program = std::ffi::OsString::from(&config.viewer[0]);
            let mut args: Vec<std::ffi::OsString> = config.viewer[1..=hc_idx]
                .iter()
                .map(std::ffi::OsString::from)
                .collect();
            args.extend(viewer_args.iter().cloned());
            (program, args)
        } else {
            // Custom external viewer or test recording script in `present.viewer`.
            let (file_str, id_str) = if let Some((id, text, denied)) = fallback_turn {
                let answers_dir = log_dir.join("hey-computer");
                let document = format_document(text, denied);
                let file = write_answer(&answers_dir, id, &document, config.keep_answers)?;
                (file.to_string_lossy().into_owned(), id.to_string())
            } else {
                (String::new(), String::new())
            };
            let program = std::ffi::OsString::from(
                config.viewer[0]
                    .replace("{file}", &file_str)
                    .replace("{id}", &id_str),
            );
            let mut args = Vec::new();
            let had_file_placeholder = config.viewer.iter().any(|a| a.contains("{file}"));
            let had_id_placeholder = config.viewer.iter().any(|a| a.contains("{id}"));
            let tail = &config.viewer[1..];
            let mut i = 0;
            while i < tail.len() {
                let raw = &tail[i];
                if raw == "--turn-id"
                    && tail.get(i + 1).is_some_and(|next| next.contains("{id}"))
                    && id_str.is_empty()
                {
                    i += 2;
                    continue;
                }
                if (raw.contains("{file}") && file_str.is_empty())
                    || (raw.contains("{id}") && id_str.is_empty())
                {
                    i += 1;
                    continue;
                }
                let expanded = raw.replace("{file}", &file_str).replace("{id}", &id_str);
                args.push(std::ffi::OsString::from(expanded));
                i += 1;
            }
            if fallback_turn.is_none() || (!had_file_placeholder && !had_id_placeholder) {
                args.extend(viewer_args.iter().cloned());
            } else if !had_file_placeholder && !file_str.is_empty() {
                args.push(std::ffi::OsString::from(file_str));
            }
            (program, args)
        }
    } else if has_graphical_display() {
        let bin = crate::distill::resolve_viewer_binary();
        (bin.into_os_string(), viewer_args.to_vec())
    } else {
        anyhow::bail!(
            "no viewer is configured (set present.viewer) and no graphical display (WAYLAND_DISPLAY/DISPLAY) is available"
        );
    };

    let program_display = program.to_string_lossy().into_owned();
    let mut child = Command::new(&program)
        .args(&args)
        .env_remove("HEY_COMPUTER_AGENT_TURN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .map_err(|e| anyhow::anyhow!("starting {program_display}: {e}"))?;

    match tokio::time::timeout(DETACHED_VIEWER_GRACE, child.wait()).await {
        Ok(Ok(status)) if !status.success() => {
            let detail = child_failure_detail(&program_display, status, &mut log);
            drop(log);
            anyhow::bail!("{detail}");
        }
        Ok(Ok(_)) => {
            drop(log);
            Ok(())
        }
        Ok(Err(e)) => {
            drop(log);
            anyhow::bail!("waiting for {program_display}: {e}");
        }
        Err(_) => {
            // Still running after 300 ms: leave the detached viewer alive after CLI exit.
            drop(log);
            Ok(())
        }
    }
}

/// Spawn `hey-computer-viewer` detached on `turn_id` with `--focus-composer`
/// so the user can refine a restricted notification draft interactively.
pub async fn open_draft_editor(
    config: &PresentConfig,
    turn_id: Option<u64>,
    draft_text: &str,
) -> anyhow::Result<()> {
    let mut viewer_args: Vec<std::ffi::OsString> = Vec::new();
    let fallback = if let Some(id) = turn_id {
        viewer_args.push(std::ffi::OsString::from("--turn-id"));
        viewer_args.push(std::ffi::OsString::from(id.to_string()));
        viewer_args.push(std::ffi::OsString::from("--focus-composer"));
        Some((id, draft_text, &[][..]))
    } else {
        viewer_args.push(std::ffi::OsString::from("--focus-composer"));
        None
    };
    open_viewer_detached(config, &viewer_args, fallback).await
}

/// Open turn `id`'s answer in the configured viewer using `$XDG_RUNTIME_DIR/hey-computer`
/// as the temporary Markdown directory and expanding `{id}` + `{file}`.
pub async fn open_answer_for_turn(
    config: &PresentConfig,
    id: u64,
    text: &str,
    denied: &[String],
) -> anyhow::Result<()> {
    let answers_dir = ipc::runtime_dir()?.join("hey-computer");
    open_answer(config, &answers_dir, id, text, denied).await
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

    let mut viewer = match spawn_viewer(&config.viewer, &file, id, &[]) {
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

/// Detect whether sentence 1 of a spoken answer is German (`"de"`) or English
/// (`"en"`). Latched on sentence 1 for the remainder of the turn.
pub fn detect_speech_lang(text: &str) -> &'static str {
    const DE_STOPWORDS: &[&str] = &[
        "der", "die", "das", "und", "ist", "nicht", "ich", "ein", "eine", "mit", "auf", "für",
        "sie", "es", "wir", "sind", "wird", "im", "am", "dem", "den", "von", "zu", "heute", "uhr",
        "grad",
    ];
    const EN_STOPWORDS: &[&str] = &[
        "the", "is", "are", "and", "with", "for", "you", "your", "this", "that", "have", "from",
    ];

    if text
        .chars()
        .any(|c| matches!(c, 'ä' | 'ö' | 'ü' | 'ß' | 'Ä' | 'Ö' | 'Ü'))
    {
        return "de";
    }

    let mut de_seen: HashSet<&str> = HashSet::new();
    let mut en_seen: HashSet<&str> = HashSet::new();
    for raw in text.split(|c: char| !c.is_alphabetic()) {
        if raw.is_empty() {
            continue;
        }
        let lower = raw.to_lowercase();
        if let Some(&w) = DE_STOPWORDS.iter().find(|&&w| w == lower) {
            de_seen.insert(w);
        }
        if let Some(&w) = EN_STOPWORDS.iter().find(|&&w| w == lower) {
            en_seen.insert(w);
        }
    }

    if !de_seen.is_empty() && de_seen.len() > en_seen.len() {
        "de"
    } else {
        "en"
    }
}

/// Strip inline Markdown formatting (`**`, `__`, `~~`, `*`, `` ` ``,
/// `$$...$$`, disarmed `[image: ...]` placeholders, and `[label](url)` ->
/// `label`) before piping a spoken sentence to TTS stdin.
pub fn strip_speech_markdown(text: &str) -> String {
    let mut unlinked = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let Some(pos) = rest.find(['!', '[', '$']) else {
            unlinked.push_str(rest);
            break;
        };
        unlinked.push_str(&rest[..pos]);
        rest = &rest[pos..];

        // Skip inline `$$...$$` display math spans.
        if let Some(after_math) = rest.strip_prefix("$$") {
            if let Some(end) = after_math.find("$$") {
                rest = &after_math[end + 2..];
                continue;
            }
            rest = after_math;
            continue;
        }

        let is_image = rest.starts_with("![");
        let bracket_rest = if let Some(after_bang) = rest.strip_prefix("![") {
            Some(after_bang)
        } else {
            rest.strip_prefix('[')
        };
        if let Some(after_open) = bracket_rest
            && let Some(label_end) = find_closing_bracket(after_open)
        {
            let label = &after_open[..label_end];
            let after_bracket = &after_open[label_end + 1..];
            let is_disarmed_img = label
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("image:");
            if let Some(after_paren_open) = after_bracket.strip_prefix('(')
                && let Some(paren_end) = find_closing_paren(after_paren_open)
            {
                if !is_image && !is_disarmed_img {
                    unlinked.push_str(label);
                }
                rest = &after_paren_open[paren_end + 1..];
                continue;
            }
            if is_disarmed_img {
                rest = after_bracket;
                continue;
            }
        }
        let ch_len = rest.chars().next().map_or(1, char::len_utf8);
        unlinked.push_str(&rest[..ch_len]);
        rest = &rest[ch_len..];
    }

    let cleaned = unlinked
        .replace("**", "")
        .replace("__", "")
        .replace("~~", "")
        .replace('`', "");
    let cleaned = strip_inline_emphasis(&cleaned);
    collapse_speech_whitespace(&cleaned)
}

/// Remove paired single `*…*` emphasis markers without harming standalone
/// arithmetic asterisks (`2 * 3`) or snake_case identifiers (`foo_bar`).
fn strip_inline_emphasis(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if (ch == '*' || ch == '_')
            && i + 1 < chars.len()
            && !chars[i + 1].is_whitespace()
            && (ch == '*' || i == 0 || !chars[i - 1].is_alphanumeric())
            && let Some(rel_close) = chars[i + 1..].iter().position(|&c| c == ch)
        {
            let close = i + 1 + rel_close;
            let valid_close = close > i + 1
                && !chars[close - 1].is_whitespace()
                && (ch == '*' || close + 1 >= chars.len() || !chars[close + 1].is_alphanumeric());
            if valid_close {
                for &inner in &chars[i + 1..close] {
                    out.push(inner);
                }
                i = close + 1;
                continue;
            }
        }
        out.push(ch);
        i += 1;
    }
    out
}

fn collapse_speech_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = true;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

/// Prepare a full Markdown document (or selection) for on-demand TTS playback.
///
/// Skips fenced code blocks (` ``` ` / `~~~`), `$$...$$` display math blocks,
/// disarmed `[image: ...]` placeholders, horizontal rules, and Markdown table
/// separator rows; turns GFM alerts (`[!NOTE]`, `[!WARNING]`, etc.) into spoken
/// prefixes; strips headings (`# `) and list/checkbox markers while treating
/// headings and list items as clean sentence boundaries; and splits prose with
/// the existing German/English-aware [`SentenceAccumulator`].
pub fn prepare_document_for_speech(markdown: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut prose = String::new();
    let mut fence: Option<(char, usize)> = None;
    let mut in_display_math = false;

    let flush_prose = |prose: &mut String, out: &mut Vec<String>| {
        if prose.trim().is_empty() {
            prose.clear();
            return;
        }
        let mut acc = SentenceAccumulator::default();
        let mut produced = acc.push_delta(prose);
        produced.extend(acc.flush_remaining());
        prose.clear();
        for raw in produced {
            let clean = strip_speech_markdown(&raw);
            if !clean.is_empty() {
                out.push(clean);
            }
        }
    };

    for raw_line in markdown.lines() {
        let trimmed = raw_line.trim();

        // Fenced code blocks (` ``` ` or `~~~`, up to 3 leading spaces).
        let indent = raw_line.len() - raw_line.trim_start_matches(' ').len();
        if let Some((fch, flen)) = fence {
            if indent <= 3 {
                let run = trimmed.chars().take_while(|&c| c == fch).count();
                if run >= flen && trimmed[run..].trim().is_empty() {
                    fence = None;
                }
            }
            continue;
        }
        if !in_display_math && indent <= 3 {
            if trimmed.starts_with("```") {
                let run = trimmed.chars().take_while(|&c| c == '`').count();
                if !trimmed[run..].contains('`') {
                    flush_prose(&mut prose, &mut sentences);
                    fence = Some(('`', run));
                    continue;
                }
            } else if trimmed.starts_with("~~~") {
                let run = trimmed.chars().take_while(|&c| c == '~').count();
                flush_prose(&mut prose, &mut sentences);
                fence = Some(('~', run));
                continue;
            }
        }

        // Display math blocks (`$$...$$`).
        if in_display_math {
            if trimmed.contains("$$") {
                in_display_math = false;
            }
            continue;
        }
        if let Some(after) = trimmed.strip_prefix("$$") {
            flush_prose(&mut prose, &mut sentences);
            if !after.contains("$$") {
                in_display_math = true;
            }
            continue;
        }

        // Blank lines flush the current prose paragraph.
        if trimmed.is_empty() {
            flush_prose(&mut prose, &mut sentences);
            continue;
        }

        // Strip leading blockquote markers (`>`).
        let mut line = trimmed;
        while let Some(after_gt) = line.strip_prefix('>') {
            line = after_gt.trim_start();
        }
        if line.is_empty() {
            flush_prose(&mut prose, &mut sentences);
            continue;
        }

        // Skip thematic breaks / horizontal rules (`---`, `***`, `___`).
        if is_horizontal_rule(line) {
            flush_prose(&mut prose, &mut sentences);
            continue;
        }

        // Skip Markdown table separator rows (`| --- | :---: |`).
        if is_table_separator_row(line) {
            continue;
        }

        // Convert GFM alert markers (`[!NOTE]`, `[!WARNING]`, etc.) into spoken prefixes.
        if let Some((label, rest)) = parse_gfm_alert(line) {
            flush_prose(&mut prose, &mut sentences);
            prose.push_str(label);
            if !rest.is_empty() {
                prose.push_str(rest);
                prose.push(' ');
            }
            continue;
        }

        // Headings (`# Heading`) are clean sentence boundaries.
        let hashes = line.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes)
            && (line.len() == hashes || line[hashes..].starts_with([' ', '\t']))
        {
            flush_prose(&mut prose, &mut sentences);
            let heading = line[hashes..].trim().trim_end_matches('#').trim();
            let clean = strip_speech_markdown(heading);
            if !clean.is_empty() {
                sentences.push(clean);
            }
            continue;
        }

        // Table rows: turn `| A | B |` into `A, B` as a clean boundary.
        if line.starts_with('|') && line.matches('|').count() >= 2 {
            flush_prose(&mut prose, &mut sentences);
            let cells: Vec<String> = line
                .trim_matches('|')
                .split('|')
                .map(|cell| strip_speech_markdown(cell.trim()))
                .filter(|c| !c.is_empty())
                .collect();
            if !cells.is_empty() {
                sentences.push(cells.join(", "));
            }
            continue;
        }

        // List items & task checkboxes (`- `, `* `, `+ `, `1. `, `1) `, `- [ ] `, `- [x] `).
        if let Some(item) = strip_list_marker(line) {
            flush_prose(&mut prose, &mut sentences);
            prose.push_str(item);
            flush_prose(&mut prose, &mut sentences);
            continue;
        }

        if !prose.is_empty() && !prose.ends_with(' ') {
            prose.push(' ');
        }
        prose.push_str(line);
    }

    flush_prose(&mut prose, &mut sentences);
    sentences
}

fn is_horizontal_rule(line: &str) -> bool {
    let mut marker: Option<char> = None;
    let mut count = 0usize;
    for ch in line.chars() {
        if ch.is_whitespace() {
            continue;
        }
        if !matches!(ch, '-' | '*' | '_') {
            return false;
        }
        if let Some(prev) = marker
            && prev != ch
        {
            return false;
        }
        marker = Some(ch);
        count += 1;
    }
    count >= 3
}

fn is_table_separator_row(line: &str) -> bool {
    line.contains('|')
        && line.contains('-')
        && line
            .chars()
            .all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
}

fn parse_gfm_alert(line: &str) -> Option<(&'static str, &str)> {
    const ALERTS: [(&str, &str); 5] = [
        ("[!NOTE]", "Note: "),
        ("[!TIP]", "Tip: "),
        ("[!IMPORTANT]", "Important: "),
        ("[!WARNING]", "Warning: "),
        ("[!CAUTION]", "Caution: "),
    ];
    let upper_prefix: String = line
        .chars()
        .take(16)
        .collect::<String>()
        .to_ascii_uppercase();
    for (tag, spoken) in ALERTS {
        if upper_prefix.starts_with(tag) {
            let rest = line[tag.len()..].trim();
            return Some((spoken, rest));
        }
    }
    None
}

fn strip_list_marker(line: &str) -> Option<&str> {
    let rest = if let Some(after) = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))
    {
        after
    } else {
        let digits = line.bytes().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0 || digits >= line.len() {
            return None;
        }
        let after_digits = &line[digits..];
        after_digits
            .strip_prefix(". ")
            .or_else(|| after_digits.strip_prefix(") "))?
    };
    let rest = rest.trim_start();
    let rest = rest
        .strip_prefix("[ ] ")
        .or_else(|| rest.strip_prefix("[x] "))
        .or_else(|| rest.strip_prefix("[X] "))
        .unwrap_or(rest);
    Some(rest.trim())
}

/// Latch DE vs EN language across the first ~160 chars of cleaned prose so a
/// single loanword heading does not mis-select the voice for the whole document.
fn detect_document_lang(sentences: &[String]) -> &'static str {
    let mut sample = String::new();
    for s in sentences {
        if !sample.is_empty() {
            sample.push(' ');
        }
        sample.push_str(s);
        if sample.chars().count() >= 160 {
            break;
        }
    }
    detect_speech_lang(&sample)
}

fn has_tts_command(speak: &SpeakConfig) -> bool {
    !speak.command.is_empty() || !speak.command_en.is_empty() || !speak.command_de.is_empty()
}

fn find_closing_paren(s: &str) -> Option<usize> {
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
            b'\n' => return None,
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
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

#[derive(Default)]
struct SentenceAccumulator {
    accumulated: String,
    pending: String,
}

impl SentenceAccumulator {
    fn should_cutoff(&self, limits: Limits) -> bool {
        let trimmed = self.accumulated.trim();
        if trimmed.chars().count() > limits.max_chars {
            return true;
        }
        if trimmed.lines().filter(|l| !l.trim().is_empty()).count() > limits.max_lines {
            return true;
        }
        if is_rich(&self.accumulated) {
            return true;
        }
        self.accumulated.lines().any(|line| {
            let indent = line.len() - line.trim_start_matches(' ').len();
            if indent > 3 {
                return false;
            }
            let t = line.trim_start();
            t.starts_with("```")
                || t.starts_with("~~~")
                || t.starts_with("$$")
                || t.starts_with('|')
        })
    }

    fn push_delta(&mut self, delta: &str) -> Vec<String> {
        self.accumulated.push_str(delta);
        self.pending.push_str(delta);
        self.drain_completed()
    }

    fn flush_remaining(&mut self) -> Vec<String> {
        let mut out = self.drain_completed();
        let tail = std::mem::take(&mut self.pending);
        let tail = tail.trim();
        if !tail.is_empty() {
            out.push(tail.to_string());
        }
        out
    }

    fn drain_completed(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Some((sent_end, next_start)) = next_sentence_split(&self.pending) {
            let candidate = self.pending[..sent_end].trim();
            if !candidate.is_empty() {
                out.push(candidate.to_string());
            }
            self.pending.drain(..next_start);
        }
        out
    }
}

fn next_sentence_split(s: &str) -> Option<(usize, usize)> {
    for (idx, ch) in s.char_indices() {
        if s[idx..].starts_with("\n\n") || s[idx..].starts_with("\r\n\r\n") {
            let ws_len: usize = s[idx..]
                .chars()
                .take_while(|c| c.is_whitespace())
                .map(char::len_utf8)
                .sum();
            return Some((idx, idx + ws_len));
        }
        if !matches!(ch, '.' | '!' | '?') {
            continue;
        }
        let mut close_end = idx + ch.len_utf8();
        while let Some(next_ch) = s[close_end..].chars().next() {
            if matches!(next_ch, '"' | '\'' | ')' | ']' | '»' | '”' | '’') {
                close_end += next_ch.len_utf8();
            } else {
                break;
            }
        }
        let Some(after_ch) = s[close_end..].chars().next() else {
            continue;
        };
        if !after_ch.is_whitespace() {
            continue;
        }
        if ch == '.' && is_non_terminal_period(&s[..idx]) {
            continue;
        }
        let ws_len: usize = s[close_end..]
            .chars()
            .take_while(|c| c.is_whitespace())
            .map(char::len_utf8)
            .sum();
        return Some((close_end, close_end + ws_len));
    }
    None
}

fn is_non_terminal_period(before_dot: &str) -> bool {
    let seg = before_dot.rsplit('\n').next().unwrap_or(before_dot);
    let raw_token = seg.rsplit(char::is_whitespace).next().unwrap_or(seg);
    let prefix_before_token = &seg[..seg.len() - raw_token.len()];
    let token = raw_token.trim_start_matches(['(', '[', '"', '\'', '*', '_']);
    if token.is_empty() {
        return false;
    }
    if token.bytes().all(|b| b.is_ascii_digit()) {
        // Leading list marker (`1. Milk`) or German date/ordinal (`am 25. Mai`).
        if !prefix_before_token.chars().any(char::is_alphabetic) {
            return true;
        }
        let prev_word = prefix_before_token
            .split_whitespace()
            .next_back()
            .unwrap_or("")
            .trim_matches(|c: char| !c.is_alphabetic())
            .to_lowercase();
        if matches!(
            prev_word.as_str(),
            "am" | "den" | "der" | "dem" | "vom" | "zum" | "im" | "nr" | "no"
        ) {
            return true;
        }
    }

    let lower = token.to_lowercase();
    if matches!(
        lower.as_str(),
        "e.g"
            | "i.e"
            | "z.b"
            | "u.a"
            | "d.h"
            | "o.ä"
            | "s.o"
            | "u.u"
            | "bzw"
            | "ca"
            | "dr"
            | "prof"
            | "nr"
            | "mr"
            | "mrs"
            | "ms"
            | "vs"
            | "etc"
            | "st"
            | "inkl"
            | "ggf"
            | "evtl"
            | "approx"
            | "fig"
    ) {
        return true;
    }
    lower.contains('.')
        && lower
            .split('.')
            .all(|p| !p.is_empty() && p.chars().count() <= 2 && p.chars().all(char::is_alphabetic))
}

struct SpeechShared {
    config: SpeakConfig,
    limits: Limits,
    speak_enabled: AtomicBool,
    speaking: AtomicBool,
    active_turn_id: AtomicU64,
    epoch: AtomicU64,
    active_pgid: Mutex<Option<i32>>,
}

impl SpeechShared {
    fn new(config: &SpeakConfig, limits: Limits) -> Self {
        Self {
            config: config.clone(),
            limits,
            speak_enabled: AtomicBool::new(config.enabled),
            speaking: AtomicBool::new(false),
            active_turn_id: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            active_pgid: Mutex::new(None),
        }
    }

    fn can_speak(&self) -> bool {
        self.speak_enabled.load(Ordering::SeqCst)
            && (has_tts_command(&self.config)
                || !self.config.earcon_start.is_empty()
                || !self.config.earcon_done.is_empty())
    }

    fn kill_active_pgid(&self) {
        if let Ok(mut guard) = self.active_pgid.lock()
            && let Some(pgid) = guard.take()
        {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
}

enum SpeechCmd {
    Start {
        id: u64,
        epoch: u64,
    },
    Chunk {
        id: u64,
        delta: String,
        epoch: u64,
    },
    Finish {
        id: u64,
        full_text: String,
        has_widgets: bool,
        epoch: u64,
    },
    ReadAloud {
        id: u64,
        epoch: u64,
        sentences: Vec<String>,
        lang: &'static str,
        done: oneshot::Sender<()>,
    },
    Stop {
        epoch: u64,
    },
}

struct TurnSpeech {
    id: u64,
    epoch: u64,
    accumulator: SentenceAccumulator,
    lang: Option<&'static str>,
    sentences: VecDeque<String>,
    finishing: bool,
    cut_off_to_viewer: bool,
    is_read_aloud: bool,
    done: Option<oneshot::Sender<()>>,
}

impl Drop for TurnSpeech {
    fn drop(&mut self) {
        if let Some(tx) = self.done.take() {
            let _ = tx.send(());
        }
    }
}

struct RunningTts {
    epoch: u64,
    child: tokio::process::Child,
    pgid: Option<i32>,
}

#[derive(Default)]
enum DuckState {
    #[default]
    Idle,
    PausedPlayers(Vec<String>),
    LoweredPamixer(u32),
    LoweredWpctl(String),
}

async fn run_speech_actor(shared: Arc<SpeechShared>, mut rx: mpsc::UnboundedReceiver<SpeechCmd>) {
    let mut turn: Option<TurnSpeech> = None;
    let mut running_tts: Option<RunningTts> = None;
    let mut duck_state = DuckState::Idle;
    let mut rx_closed = false;

    loop {
        let current_epoch = shared.epoch.load(Ordering::SeqCst);
        if let Some(t) = &turn
            && t.epoch != current_epoch
        {
            turn = None;
        }
        if let Some(tts) = &running_tts
            && tts.epoch != current_epoch
        {
            kill_running_tts(&shared, &mut running_tts).await;
        }

        if running_tts.is_none() {
            let mut completed_turn: Option<(u64, u64, bool)> = None;
            if let Some(t) = &mut turn {
                if t.cut_off_to_viewer {
                    completed_turn = Some((t.id, t.epoch, false));
                } else if let Some(raw_sentence) = t.sentences.pop_front() {
                    let lang = *t
                        .lang
                        .get_or_insert_with(|| detect_speech_lang(&raw_sentence));
                    let clean = if t.is_read_aloud {
                        raw_sentence
                    } else {
                        strip_speech_markdown(&raw_sentence)
                    };
                    let argv = select_tts_command(&shared.config, lang);
                    if !clean.trim().is_empty() && !argv.is_empty() {
                        duck_media(shared.config.media_duck, &mut duck_state).await;
                        shared.speaking.store(true, Ordering::SeqCst);
                        if let Some(tts) = spawn_tts_sentence(&shared, argv, lang, &clean, t.epoch)
                        {
                            running_tts = Some(tts);
                        } else {
                            shared.speaking.store(false, Ordering::SeqCst);
                        }
                    }
                    if running_tts.is_none() {
                        continue;
                    }
                } else if t.finishing {
                    completed_turn = Some((t.id, t.epoch, !t.is_read_aloud));
                } else if rx_closed {
                    completed_turn = Some((t.id, t.epoch, false));
                }
            }

            if let Some((done_id, done_epoch, play_done_earcon)) = completed_turn {
                turn = None;
                shared.speaking.store(false, Ordering::SeqCst);
                let _ = shared.active_turn_id.compare_exchange(
                    done_id,
                    0,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                if play_done_earcon
                    && !shared.config.earcon_done.is_empty()
                    && shared.epoch.load(Ordering::SeqCst) == done_epoch
                {
                    run_earcon(&shared.config.earcon_done).await;
                }
                restore_media(&mut duck_state).await;
                continue;
            }
        }

        if rx_closed && running_tts.is_none() && turn.is_none() {
            restore_media(&mut duck_state).await;
            break;
        }

        tokio::select! {
            biased;
            maybe_cmd = rx.recv(), if !rx_closed => {
                match maybe_cmd {
                    Some(cmd) => {
                        handle_speech_cmd(
                            &shared,
                            cmd,
                            &mut turn,
                            &mut running_tts,
                            &mut duck_state,
                        )
                        .await;
                    }
                    None => {
                        rx_closed = true;
                    }
                }
            }
            status = async {
                match running_tts.as_mut() {
                    Some(tts) => Some(tts.child.wait().await),
                    None => std::future::pending().await,
                }
            } => {
                if let Some(tts) = running_tts.take() {
                    if let Ok(mut guard) = shared.active_pgid.lock()
                        && *guard == tts.pgid
                    {
                        *guard = None;
                    }
                    if let Some(Ok(st)) = status
                        && !st.success()
                    {
                        tracing::debug!("tts command exited with {st}");
                    }
                }
                let more_queued = turn
                    .as_ref()
                    .is_some_and(|t| !t.cut_off_to_viewer && !t.sentences.is_empty());
                if !more_queued {
                    shared.speaking.store(false, Ordering::SeqCst);
                }
            }
        }
    }
}

async fn handle_speech_cmd(
    shared: &SpeechShared,
    cmd: SpeechCmd,
    turn: &mut Option<TurnSpeech>,
    running_tts: &mut Option<RunningTts>,
    duck_state: &mut DuckState,
) {
    let current_epoch = shared.epoch.load(Ordering::SeqCst);
    match cmd {
        SpeechCmd::Start { id, epoch } => {
            if epoch != current_epoch || !shared.can_speak() {
                return;
            }
            kill_running_tts(shared, running_tts).await;
            shared.speaking.store(false, Ordering::SeqCst);
            if !shared.config.earcon_start.is_empty() {
                spawn_earcon_detached(&shared.config.earcon_start);
            }
            *turn = Some(TurnSpeech {
                id,
                epoch,
                accumulator: SentenceAccumulator::default(),
                lang: None,
                sentences: VecDeque::new(),
                finishing: false,
                cut_off_to_viewer: false,
                is_read_aloud: false,
                done: None,
            });
        }
        SpeechCmd::ReadAloud {
            id,
            epoch,
            sentences,
            lang,
            done,
        } => {
            if epoch != current_epoch || !has_tts_command(&shared.config) {
                let _ = done.send(());
                return;
            }
            kill_running_tts(shared, running_tts).await;
            *turn = Some(TurnSpeech {
                id,
                epoch,
                accumulator: SentenceAccumulator::default(),
                lang: Some(lang),
                sentences: sentences.into(),
                finishing: true,
                cut_off_to_viewer: false,
                is_read_aloud: true,
                done: Some(done),
            });
        }
        SpeechCmd::Chunk { id, delta, epoch } => {
            if epoch != current_epoch || !shared.can_speak() {
                return;
            }
            let Some(t) = turn.as_mut() else {
                return;
            };
            if t.id != id || t.epoch != epoch || t.cut_off_to_viewer {
                return;
            }
            let newly_ready = t.accumulator.push_delta(&delta);
            if t.accumulator.should_cutoff(shared.limits) {
                t.cut_off_to_viewer = true;
                t.sentences.clear();
                kill_running_tts(shared, running_tts).await;
                shared.speaking.store(false, Ordering::SeqCst);
                let _ = shared.active_turn_id.compare_exchange(
                    id,
                    0,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                restore_media(duck_state).await;
                return;
            }
            t.sentences.extend(newly_ready);
        }
        SpeechCmd::Finish {
            id,
            full_text,
            has_widgets,
            epoch,
        } => {
            if epoch != current_epoch {
                return;
            }
            let Some(t) = turn.as_mut() else {
                let _ = shared.active_turn_id.compare_exchange(
                    id,
                    0,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                return;
            };
            if t.id != id || t.epoch != epoch {
                return;
            }
            if t.cut_off_to_viewer || has_widgets || !is_short(&full_text, shared.limits) {
                t.cut_off_to_viewer = true;
                t.sentences.clear();
                kill_running_tts(shared, running_tts).await;
                shared.speaking.store(false, Ordering::SeqCst);
                let _ = shared.active_turn_id.compare_exchange(
                    id,
                    0,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                *turn = None;
                restore_media(duck_state).await;
                return;
            }
            if t.accumulator.accumulated.is_empty() && !full_text.trim().is_empty() {
                let _ = t.accumulator.push_delta(&full_text);
            } else if full_text.starts_with(&t.accumulator.accumulated)
                && full_text.len() > t.accumulator.accumulated.len()
            {
                let suffix = &full_text[t.accumulator.accumulated.len()..];
                let extra = t.accumulator.push_delta(suffix);
                t.sentences.extend(extra);
            }
            t.sentences.extend(t.accumulator.flush_remaining());
            t.finishing = true;
        }
        SpeechCmd::Stop { epoch } => {
            let _ = epoch;
            kill_running_tts(shared, running_tts).await;
            *turn = None;
            shared.speaking.store(false, Ordering::SeqCst);
            if !shared.config.earcon_cancel.is_empty() {
                spawn_earcon_detached(&shared.config.earcon_cancel);
            }
            restore_media(duck_state).await;
        }
    }
}

fn select_tts_command<'a>(speak: &'a SpeakConfig, lang: &str) -> &'a [String] {
    match lang {
        "de" if !speak.command_de.is_empty() => &speak.command_de,
        "en" if !speak.command_en.is_empty() => &speak.command_en,
        _ => &speak.command,
    }
}

/// Offline fallback TTS playback used by `hey-computer-viewer` when
/// `hey-computer.sock` is unreachable (e.g. viewing a standalone Markdown file
/// while the daemon is stopped). Returns `Ok(true)` if spoken to completion,
/// `Ok(false)` if cancelled or empty, or `Err` if no TTS command is configured.
pub async fn speak_text_local(
    config: &SpeakConfig,
    text: String,
    mut cancel: watch::Receiver<bool>,
) -> anyhow::Result<bool> {
    if !has_tts_command(config) {
        anyhow::bail!(
            "no TTS command is configured (set present.speak.command, command_en, or command_de)"
        );
    }
    let sentences = prepare_document_for_speech(&text);
    if sentences.is_empty() || *cancel.borrow() {
        return Ok(false);
    }
    let lang = detect_document_lang(&sentences);
    let argv = select_tts_command(config, lang);
    if argv.is_empty() {
        anyhow::bail!("no TTS command available for language `{lang}`");
    }

    let mut duck_state = DuckState::Idle;
    duck_media(config.media_duck, &mut duck_state).await;

    let program = argv[0].replace("{lang}", lang);
    let args: Vec<String> = argv[1..]
        .iter()
        .map(|a| a.replace("{lang}", lang))
        .collect();

    let mut completed = true;
    for sentence in sentences {
        if *cancel.borrow() {
            completed = false;
            break;
        }
        let mut child = match Command::new(&program)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                restore_media(&mut duck_state).await;
                anyhow::bail!("cannot start TTS command {program}: {e}");
            }
        };
        let pgid = child.id().and_then(|pid| i32::try_from(pid).ok());
        if let Some(mut stdin) = child.stdin.take() {
            let mut payload = sentence.into_bytes();
            payload.push(b'\n');
            tokio::spawn(async move {
                let _ = stdin.write_all(&payload).await;
                let _ = stdin.shutdown().await;
            });
        }

        let cancelled = loop {
            tokio::select! {
                biased;
                res = cancel.changed() => {
                    if res.is_err() || *cancel.borrow() {
                        break true;
                    }
                }
                status = child.wait() => {
                    if let Ok(st) = status && !st.success() {
                        tracing::debug!("local tts command exited with {st}");
                    }
                    break false;
                }
            }
        };
        if cancelled {
            if let Some(p) = pgid {
                unsafe {
                    libc::kill(-p, libc::SIGKILL);
                }
            }
            let _ = child.kill().await;
            let _ = child.wait().await;
            completed = false;
            break;
        }
    }

    restore_media(&mut duck_state).await;
    Ok(completed)
}

fn spawn_tts_sentence(
    shared: &SpeechShared,
    argv: &[String],
    lang: &str,
    sentence: &str,
    turn_epoch: u64,
) -> Option<RunningTts> {
    if argv.is_empty() {
        return None;
    }
    let program = argv[0].replace("{lang}", lang);
    let args: Vec<String> = argv[1..]
        .iter()
        .map(|a| a.replace("{lang}", lang))
        .collect();
    let mut child = match Command::new(&program)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            tracing::warn!("cannot start TTS command {program}: {e}");
            return None;
        }
    };
    let pgid = child.id().and_then(|pid| i32::try_from(pid).ok());
    {
        let Ok(mut guard) = shared.active_pgid.lock() else {
            let _ = child.start_kill();
            return None;
        };
        if shared.epoch.load(Ordering::SeqCst) != turn_epoch {
            if let Some(p) = pgid {
                unsafe {
                    libc::kill(-p, libc::SIGKILL);
                }
            }
            let _ = child.start_kill();
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            return None;
        }
        *guard = pgid;
    }
    if let Some(mut stdin) = child.stdin.take() {
        let mut payload = sentence.as_bytes().to_vec();
        payload.push(b'\n');
        tokio::spawn(async move {
            let _ = stdin.write_all(&payload).await;
            let _ = stdin.shutdown().await;
        });
    }
    Some(RunningTts {
        epoch: turn_epoch,
        child,
        pgid,
    })
}

async fn kill_running_tts(shared: &SpeechShared, running: &mut Option<RunningTts>) {
    if let Some(mut tts) = running.take() {
        if let Ok(mut guard) = shared.active_pgid.lock()
            && *guard == tts.pgid
        {
            *guard = None;
        }
        if let Some(pgid) = tts.pgid {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
        let _ = tts.child.kill().await;
        let _ = tts.child.wait().await;
    }
}

fn spawn_earcon_detached(argv: &[String]) {
    if argv.is_empty() {
        return;
    }
    let program = argv[0].clone();
    let args = argv[1..].to_vec();
    tokio::spawn(async move {
        if let Ok(mut child) = Command::new(&program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
        {
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        }
    });
}

async fn run_earcon(argv: &[String]) {
    if argv.is_empty() {
        return;
    }
    if let Ok(mut child) = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
    {
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    }
}

async fn run_duck_cmd(program: &str, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_millis(800), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn duck_media(mode: MediaDuck, state: &mut DuckState) {
    duck_media_with(mode, state, |prog, args| {
        let prog = prog.to_string();
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        async move {
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            run_duck_cmd(&prog, &refs).await
        }
    })
    .await;
}

async fn duck_media_with<F, Fut>(mode: MediaDuck, state: &mut DuckState, mut run: F)
where
    F: FnMut(&str, &[&str]) -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    if !matches!(state, DuckState::Idle) || mode == MediaDuck::None {
        return;
    }
    match mode {
        MediaDuck::None => {}
        MediaDuck::Pause => {
            let Some(list) = run("playerctl", &["-l"]).await else {
                return;
            };
            let mut paused = Vec::new();
            for raw in list.lines() {
                let player = raw.trim();
                if player.is_empty() {
                    continue;
                }
                if let Some(status) = run("playerctl", &["-p", player, "status"]).await
                    && status.trim() == "Playing"
                    && run("playerctl", &["-p", player, "pause"]).await.is_some()
                {
                    paused.push(player.to_string());
                }
            }
            if !paused.is_empty() {
                *state = DuckState::PausedPlayers(paused);
            }
        }
        MediaDuck::Lower => {
            if let Some(vol_out) = run("pamixer", &["--get-volume"]).await
                && let Ok(vol) = vol_out.trim().parse::<u32>()
            {
                if vol > 0 {
                    let ducked = ((vol * 35) / 100).to_string();
                    if run("pamixer", &["--set-volume", &ducked]).await.is_some() {
                        *state = DuckState::LoweredPamixer(vol);
                    }
                }
                return;
            }
            if let Some(wp_out) = run("wpctl", &["get-volume", "@DEFAULT_AUDIO_SINK@"]).await
                && !wp_out.contains("[MUTED]")
                && let Some(num) = wp_out
                    .split_whitespace()
                    .nth(1)
                    .and_then(|t| t.parse::<f64>().ok())
                && num > 0.0
            {
                let orig = format!("{num:.2}");
                let ducked = format!("{:.2}", num * 0.35);
                if run("wpctl", &["set-volume", "@DEFAULT_AUDIO_SINK@", &ducked])
                    .await
                    .is_some()
                {
                    *state = DuckState::LoweredWpctl(orig);
                }
            }
        }
    }
}

async fn restore_media(state: &mut DuckState) {
    restore_media_with(state, |prog, args| {
        let prog = prog.to_string();
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        async move {
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            run_duck_cmd(&prog, &refs).await
        }
    })
    .await;
}

async fn restore_media_with<F, Fut>(state: &mut DuckState, mut run: F)
where
    F: FnMut(&str, &[&str]) -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    match std::mem::take(state) {
        DuckState::Idle => {}
        DuckState::PausedPlayers(players) => {
            for player in players {
                let _ = run("playerctl", &["-p", &player, "play"]).await;
            }
        }
        DuckState::LoweredPamixer(vol) => {
            let s = vol.to_string();
            let _ = run("pamixer", &["--set-volume", &s]).await;
        }
        DuckState::LoweredWpctl(orig) => {
            let _ = run("wpctl", &["set-volume", "@DEFAULT_AUDIO_SINK@", &orig]).await;
        }
    }
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
            widgets: vec![],
            icons: vec![],
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
            widgets: vec![],
            icons: vec![],
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
            widgets: vec![],
            icons: vec![],
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
            widgets: vec![],
            icons: vec![],
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
            icons: vec![],
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
            widgets: vec![],
            icons: vec![],
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
            icons: vec![],
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
            icons: vec![],
        });
        desktop.present(Presentation::ProgressUpdate {
            id: 1,
            status: "step 3".to_string(),
            icons: vec![],
        });
        desktop.present(Presentation::ProgressUpdate {
            id: 1,
            status: "step 4".to_string(),
            icons: vec![],
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
            icons: vec![],
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
            icons: vec![],
        });
        desktop.present(Presentation::Answer {
            id: 1,
            text: "Finished.".to_string(),
            denied: vec![],
            profile: None,
            widgets: vec![],
            icons: vec![],
        });
        desktop.present(Presentation::ProgressUpdate {
            id: 1,
            status: "late zombie".to_string(),
            icons: vec![],
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
            icons: vec![],
        });
        desktop.present(Presentation::ProgressUpdate {
            id: 2,
            status: "after superseded".to_string(),
            icons: vec![],
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
            icons: vec![],
        });
        desktop.present(Presentation::Answer {
            id: 1,
            text: "ok".to_string(),
            denied: vec![],
            profile: None,
            widgets: vec![],
            icons: vec![],
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
            icons: vec![],
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
            widgets: vec![],
            icons: vec![],
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
            widgets: vec![],
            icons: vec![],
        });
        desktop.present(Presentation::Answer {
            id: 1,
            text: "Quick reply.".to_string(),
            denied: vec![],
            profile: None,
            widgets: vec![],
            icons: vec![],
        });

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline
            && std::fs::read_to_string(&opened)
                .unwrap_or_default()
                .is_empty()
        {
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
            widgets: vec![],
            icons: vec![],
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

    #[tokio::test]
    async fn short_answer_replaces_thinking_and_handles_copy_reply_and_open_actions() {
        let dir = std::env::temp_dir().join(format!("hc-short-actions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 1. Copy action on short answer (also verifies --replace-id=7 and action flags).
        let calls = dir.join("notify-copy.log");
        let copied = dir.join("copied.txt");
        let action_file = dir.join("action.txt");
        std::fs::write(&action_file, "copy").unwrap();
        let notifier_script = format!(
            "echo \"$*\" >> '{}'; case \"$*\" in *--action=copy=Copy*) echo 8; sleep 0.05; cat '{}' ;; *) echo 7 ;; esac",
            calls.display(),
            action_file.display()
        );
        let viewer_out = dir.join("viewer.txt");
        let reply_out = dir.join("reply.txt");
        let config = PresentConfig {
            notifier: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                notifier_script,
                "sh".to_string(),
            ],
            clipboard: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("cat > '{}'", copied.display()),
                "sh".to_string(),
            ],
            reply_command: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("printf '%s' \"$1\" > '{}'", reply_out.display()),
                "sh".to_string(),
                "--reply-to={id}".to_string(),
            ],
            viewer: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!(
                    "{{ printf '%s|' \"$@\"; cat \"$2\"; }} > '{}.tmp' && mv '{}.tmp' '{}'",
                    viewer_out.display(),
                    viewer_out.display(),
                    viewer_out.display()
                ),
                "hey-computer-viewer".to_string(),
                "--turn-id={id}".to_string(),
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
            id: 42,
            request: "ask".to_string(),
            profile: None,
            thinking_label: None,
        });
        desktop.present(Presentation::Answer {
            id: 42,
            text: "Short <raw> reply.".to_string(),
            denied: vec![],
            profile: None,
            widgets: vec![],
            icons: vec![],
        });

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline
            && std::fs::read_to_string(&copied)
                .unwrap_or_default()
                .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert_eq!(
            std::fs::read_to_string(&copied).unwrap(),
            "Short <raw> reply."
        );
        let log = std::fs::read_to_string(&calls).unwrap();
        assert!(log.contains("--action=copy=Copy"), "{log}");
        assert!(log.contains("--action=reply=Reply"), "{log}");
        assert!(log.contains("--action=open=Open"), "{log}");
        assert!(
            log.contains("--replace-id=7 -- Answer Short &lt;raw&gt; reply."),
            "{log}"
        );

        // 2. Reply action with explicit `reply_command` expands `{id}`.
        std::fs::write(&action_file, "reply").unwrap();
        desktop.present(Presentation::Answer {
            id: 43,
            text: "Reply target.".to_string(),
            denied: vec![],
            profile: None,
            widgets: vec![],
            icons: vec![],
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline
            && std::fs::read_to_string(&reply_out)
                .unwrap_or_default()
                .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert_eq!(
            std::fs::read_to_string(&reply_out).unwrap(),
            "--reply-to=43"
        );
        desktop.finish().await;

        // 3. Reply fallback (empty `reply_command`) opens `hey-computer-viewer` with `--focus-composer` & `{id}`.
        let mut fallback_cfg = config.clone();
        fallback_cfg.reply_command.clear();
        let desktop_fb = Desktop::spawn_full(
            &fallback_cfg,
            dir.join("answers-fb"),
            Duration::from_millis(100),
            PROGRESS_UPDATE_INTERVAL,
            Duration::from_secs(5),
        );
        std::fs::write(&action_file, "reply").unwrap();
        desktop_fb.present(Presentation::Answer {
            id: 44,
            text: "Compose <here>.".to_string(),
            denied: vec![],
            profile: None,
            widgets: vec![],
            icons: vec![],
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline
            && std::fs::read_to_string(&viewer_out)
                .unwrap_or_default()
                .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        let fb_out = std::fs::read_to_string(&viewer_out).unwrap();
        assert!(fb_out.contains("--turn-id=44|"), "{fb_out}");
        assert!(fb_out.contains("|--focus-composer|"), "{fb_out}");
        assert!(fb_out.contains("Compose &lt;here>."), "{fb_out}");

        // 4. Open action writes disarmed Markdown and opens viewer without `--focus-composer`.
        let _ = std::fs::remove_file(&viewer_out);
        std::fs::write(&action_file, "open").unwrap();
        desktop_fb.present(Presentation::Answer {
            id: 45,
            text: "Open <safe>.".to_string(),
            denied: vec![],
            profile: None,
            widgets: vec![],
            icons: vec![],
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline
            && std::fs::read_to_string(&viewer_out)
                .unwrap_or_default()
                .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        let open_out = std::fs::read_to_string(&viewer_out).unwrap();
        assert!(open_out.contains("--turn-id=45|"), "{open_out}");
        assert!(!open_out.contains("--focus-composer"), "{open_out}");
        assert!(open_out.contains("Open &lt;safe>."), "{open_out}");

        desktop_fb.finish().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn action_waiters_evict_lru_oldest_keeping_newest_clickable() {
        let dir = std::env::temp_dir().join(format!("hc-lru-waiters-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let seq_file = dir.join("seq");
        let pids_dir = dir.join("pids");
        std::fs::create_dir_all(&pids_dir).unwrap();
        let opened_id = dir.join("opened-id.txt");

        // Notifications 1..=16 print their ID and sleep waiting for a click;
        // notification 17 prints its ID, sleeps briefly, and outputs `open`.
        let notifier_script = format!(
            "n=$(cat '{seq}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{seq}'; \
             echo $$ > '{pids}'/$n; echo $n; \
             if [ \"$n\" -eq 17 ]; then sleep 0.15; echo open; else exec sleep 30; fi",
            seq = seq_file.display(),
            pids = pids_dir.display(),
        );
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
                format!("printf '%s' \"$1\" > '{}'", opened_id.display()),
                "sh".to_string(),
                "{id}".to_string(),
                "{file}".to_string(),
            ],
            clipboard: vec![],
            reply_command: vec![],
            ..PresentConfig::default()
        };

        let desktop = Desktop::spawn_full(
            &config,
            dir.join("answers"),
            Duration::from_millis(100),
            PROGRESS_UPDATE_INTERVAL,
            Duration::from_secs(30),
        );

        for id in 1..=17u64 {
            desktop.present(Presentation::Answer {
                id,
                text: format!("Short #{id}"),
                denied: vec![],
                profile: None,
                widgets: vec![],
                icons: vec![],
            });
        }

        // Wait until #17's `open` action fires and launches the viewer with `{id} == 17`.
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline
            && std::fs::read_to_string(&opened_id)
                .unwrap_or_default()
                .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            std::fs::read_to_string(&opened_id).unwrap_or_default(),
            "17",
            "17th notification must stay active and open #17 (evicting #1 instead)"
        );

        let pid1: i32 = std::fs::read_to_string(pids_dir.join("1"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let pid2: i32 = std::fs::read_to_string(pids_dir.join("2"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        // Wait briefly for aborted #1 child reaping, then verify #1 is dead while #2 is still alive.
        let reap_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < reap_deadline && unsafe { libc::kill(pid1, 0) } == 0 {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert_ne!(
            unsafe { libc::kill(pid1, 0) },
            0,
            "oldest waiter #1 (pid {pid1}) must be killed on 17th arrival"
        );
        assert_eq!(
            unsafe { libc::kill(pid2, 0) },
            0,
            "waiter #2 (pid {pid2}) must remain alive"
        );

        desktop.finish().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn answers_and_tasks_with_widgets_always_route_to_viewer_and_expand_id() {
        let dir = std::env::temp_dir().join(format!("hc-widgets-route-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let calls = dir.join("notify.log");
        let viewer_args = dir.join("viewer-args.txt");

        let config = PresentConfig {
            notifier: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("echo \"$*\" >> '{}'; echo 7", calls.display()),
                "sh".to_string(),
            ],
            viewer: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("printf '%s|%s' \"$1\" \"$2\" > '{}'", viewer_args.display()),
                "sh".to_string(),
                "--turn-id={id}".to_string(),
                "{file}".to_string(),
            ],
            ..PresentConfig::default()
        };

        let widget = WidgetSpec {
            id: "w1".to_string(),
            kind: "calendar_agenda".to_string(),
            title: "Today's Agenda".to_string(),
            data: serde_json::json!({"events": []}),
        };

        let desktop = Desktop::spawn_full(
            &config,
            dir.join("answers"),
            Duration::from_millis(100),
            PROGRESS_UPDATE_INTERVAL,
            Duration::from_secs(5),
        );
        // Short text ("Sunny.") + non-empty widgets must bypass short notification and open viewer!
        desktop.present(Presentation::Answer {
            id: 91,
            text: "Sunny.".to_string(),
            denied: vec![],
            profile: None,
            widgets: vec![widget.clone()],
            icons: vec![],
        });
        // TaskComplete with short text + widget uses `#id · headline` and offers `Open`.
        desktop.present(Presentation::TaskComplete {
            id: 92,
            request: "agenda".to_string(),
            text: "Sunny.".to_string(),
            denied: vec![],
            profile: None,
            widgets: vec![widget],
            icons: vec![],
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if std::fs::read_to_string(&calls)
                .unwrap_or_default()
                .contains("Task complete #92")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        desktop.finish().await;

        let args_recorded = std::fs::read_to_string(&viewer_args).unwrap();
        assert!(
            args_recorded.starts_with("--turn-id=91|"),
            "{args_recorded}"
        );
        assert!(args_recorded.ends_with("-91.md"), "{args_recorded}");

        let notified = std::fs::read_to_string(&calls).unwrap();
        assert!(notified.contains("Answer opened Sunny."), "{notified}");
        assert!(
            notified.contains("--action=open=Open -- Task complete #92 · Sunny."),
            "{notified}"
        );

        // Also test `open_answer` with id=0 vs id=55 for `{id}` expansion.
        open_answer_with_grace(
            &config,
            &dir.join("answers"),
            0,
            "file mode",
            &[],
            Duration::from_millis(100),
        )
        .await
        .unwrap();
        let zero_args = std::fs::read_to_string(&viewer_args).unwrap();
        assert!(zero_args.starts_with("--turn-id=0|"), "{zero_args}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn detect_speech_lang_sentence_segmentation_and_markdown_sanitizer() {
        assert_eq!(detect_speech_lang("Draußen sind es 18°C."), "de");
        assert_eq!(detect_speech_lang("Schöne Grüße aus Hamburg!"), "de");
        assert_eq!(
            detect_speech_lang("Es ist 14:32 Uhr und heute scheint die Sonne."),
            "de"
        );
        assert_eq!(detect_speech_lang("Ich bin heute im Büro."), "de");
        assert_eq!(detect_speech_lang("It is 14:32 and sunny today."), "en");
        assert_eq!(
            detect_speech_lang("The process might die with your current config."),
            "en"
        );
        assert_eq!(detect_speech_lang("Done."), "en");

        assert_eq!(
            strip_speech_markdown(
                "Run **hyprctl** `version` — see [the docs](https://wiki.hypr.land) for __details__."
            ),
            "Run hyprctl version — see the docs for details."
        );

        let mut acc = SentenceAccumulator::default();
        let s1 = acc.push_delta("Pi is ca. 3.14, e.g. for Dr. Smith in Nr. 5 bzw. z.B. here! ");
        assert_eq!(
            s1,
            vec!["Pi is ca. 3.14, e.g. for Dr. Smith in Nr. 5 bzw. z.B. here!"]
        );
        assert!(acc.push_delta("1. First item").is_empty());
        let s2 = acc.push_delta(" works. Second fragment");
        assert_eq!(s2, vec!["1. First item works."]);
        assert_eq!(acc.flush_remaining(), vec!["Second fragment"]);
    }

    #[tokio::test]
    async fn speech_actor_streams_sentences_latches_language_and_cuts_off_on_rich_or_cancel() {
        let dir = std::env::temp_dir().join(format!("hc-speech-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tts_log = dir.join("tts.log");

        // 1. Streaming DE sentences with language latching on sentence 1 + earcons.
        let mut config = PresentConfig {
            notifications: false,
            ..PresentConfig::default()
        };
        config.speak = SpeakConfig {
            enabled: true,
            voice_only: true,
            command: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!(
                    "printf 'generic:%s:%s\\n' \"$1\" \"$(cat)\" >> '{}'",
                    tts_log.display()
                ),
                "sh".to_string(),
                "{lang}".to_string(),
            ],
            command_de: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("printf 'de:%s\\n' \"$(cat)\" >> '{}'", tts_log.display()),
                "sh".to_string(),
            ],
            command_en: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("printf 'en:%s\\n' \"$(cat)\" >> '{}'", tts_log.display()),
                "sh".to_string(),
            ],
            earcon_start: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("echo earcon:start >> '{}'", tts_log.display()),
                "sh".to_string(),
            ],
            earcon_done: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("echo earcon:done >> '{}'", tts_log.display()),
                "sh".to_string(),
            ],
            earcon_cancel: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("echo earcon:cancel >> '{}'", tts_log.display()),
                "sh".to_string(),
            ],
            media_duck: MediaDuck::None,
        };

        let desktop = Desktop::spawn(&config, dir.join("answers"));
        desktop.present(Presentation::SpeechStart { id: 10 });
        desktop.present(Presentation::SpeechChunk {
            id: 10,
            delta: "Es ist 14:32 Uhr. ".to_string(),
        });
        // Second sentence has no German stopwords/umlauts ("OK, **done** via [link](https://x.test).")
        // yet must remain latched to `"de"`!
        desktop.present(Presentation::SpeechChunk {
            id: 10,
            delta: "OK, **done** via [link](https://x.test).".to_string(),
        });
        desktop.present(Presentation::SpeechFinish {
            id: 10,
            full_text: "Es ist 14:32 Uhr. OK, **done** via [link](https://x.test).".to_string(),
            has_widgets: false,
        });
        desktop.finish().await;

        let log = std::fs::read_to_string(&tts_log).unwrap();
        assert!(log.contains("earcon:start"), "{log}");
        assert!(log.contains("de:Es ist 14:32 Uhr."), "{log}");
        assert!(log.contains("de:OK, done via link."), "{log}");
        assert!(log.ends_with("earcon:done\n"), "{log}");

        // 2. Mid-stream rich cutoff kills in-flight TTS immediately when code fence arrives.
        let cutoff_log = dir.join("cutoff.log");
        let cutoff_pid = dir.join("cutoff.pid");
        let mut cutoff_cfg = config.clone();
        cutoff_cfg.speak.command_en = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!(
                "echo $$ > '{}'; printf 'say:%s\\n' \"$(cat)\" >> '{}'; exec sleep 10",
                cutoff_pid.display(),
                cutoff_log.display()
            ),
            "sh".to_string(),
        ];
        cutoff_cfg.speak.earcon_done = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("echo earcon:done >> '{}'", cutoff_log.display()),
            "sh".to_string(),
        ];

        let desktop2 = Desktop::spawn(&cutoff_cfg, dir.join("answers2"));
        desktop2.present(Presentation::SpeechStart { id: 20 });
        desktop2.present(Presentation::SpeechChunk {
            id: 20,
            delta: "Here is the snippet you asked for. ".to_string(),
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !cutoff_pid.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(cutoff_pid.exists(), "sentence 1 TTS should have started");

        let t_cut = Instant::now();
        desktop2.present(Presentation::SpeechChunk {
            id: 20,
            delta: "\n```rust\nfn main() {}\n```\n".to_string(),
        });
        desktop2.present(Presentation::SpeechFinish {
            id: 20,
            full_text: "Here is the snippet you asked for. \n```rust\nfn main() {}\n```\n"
                .to_string(),
            has_widgets: false,
        });
        desktop2.finish().await;
        assert!(
            t_cut.elapsed() < Duration::from_secs(2),
            "rich cutoff must kill active TTS without waiting for sleep 10"
        );
        let c_log = std::fs::read_to_string(&cutoff_log).unwrap_or_default();
        assert!(
            c_log.contains("say:Here is the snippet you asked for."),
            "{c_log}"
        );
        assert!(!c_log.contains("earcon:done"), "{c_log}");

        // 3. Immediate post-turn `stop_speech(None)` kills active TTS in <50ms, skips queued
        //    sentence 2, and fires `earcon_cancel`.
        let cancel_log = dir.join("cancel.log");
        let cancel_pid = dir.join("cancel.pid");
        let mut cancel_cfg = config.clone();
        cancel_cfg.speak.command_en = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!(
                "printf 'tts:%s\\n' \"$(cat)\" >> '{}'; echo $$ > '{}'; exec sleep 10",
                cancel_log.display(),
                cancel_pid.display()
            ),
            "sh".to_string(),
        ];
        cancel_cfg.speak.earcon_done = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("echo earcon:done >> '{}'", cancel_log.display()),
            "sh".to_string(),
        ];
        cancel_cfg.speak.earcon_cancel = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("echo earcon:cancel >> '{}'", cancel_log.display()),
            "sh".to_string(),
        ];

        let desktop3 = Desktop::spawn(&cancel_cfg, dir.join("answers3"));
        desktop3.present(Presentation::SpeechStart { id: 77 });
        desktop3.present(Presentation::SpeechChunk {
            id: 77,
            delta: "First spoken sentence. Second spoken sentence.".to_string(),
        });
        desktop3.present(Presentation::SpeechFinish {
            id: 77,
            full_text: "First spoken sentence. Second spoken sentence.".to_string(),
            has_widgets: false,
        });

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !cancel_pid.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(desktop3.speak_status(), (true, true));

        let pid77: i32 = std::fs::read_to_string(&cancel_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let t_stop = Instant::now();
        assert_eq!(desktop3.stop_speech(Some(999)), None);
        assert_eq!(desktop3.stop_speech(None), Some(77));
        assert!(
            t_stop.elapsed() < Duration::from_millis(50),
            "stop_speech took {:?}",
            t_stop.elapsed()
        );
        assert_eq!(desktop3.speak_status(), (true, false));
        assert_eq!(desktop3.stop_speech(None), None);

        desktop3.finish().await;
        assert!(
            t_stop.elapsed() < Duration::from_secs(2),
            "post-turn stop_speech must terminate sleep 10 immediately"
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if std::fs::read_to_string(&cancel_log)
                .unwrap_or_default()
                .contains("earcon:cancel")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_ne!(unsafe { libc::kill(pid77, 0) }, 0);
        let k_log = std::fs::read_to_string(&cancel_log).unwrap_or_default();
        assert!(k_log.contains("tts:First spoken sentence."), "{k_log}");
        assert!(!k_log.contains("Second spoken sentence"), "{k_log}");
        assert!(k_log.contains("earcon:cancel"), "{k_log}");
        assert!(!k_log.contains("earcon:done"), "{k_log}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn media_ducking_resumes_only_playing_players_and_restores_volume() {
        // 1. MediaDuck::Pause pauses only `Playing` players and resumes only those.
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut state = DuckState::Idle;
        {
            let calls = Arc::clone(&calls);
            duck_media_with(MediaDuck::Pause, &mut state, move |prog, args| {
                let call = format!("{prog} {}", args.join(" "));
                calls.lock().unwrap().push(call.clone());
                async move {
                    match call.as_str() {
                        "playerctl -l" => Some("spotify\nfirefox\n".to_string()),
                        "playerctl -p spotify status" => Some("Playing\n".to_string()),
                        "playerctl -p firefox status" => Some("Paused\n".to_string()),
                        "playerctl -p spotify pause" => Some(String::new()),
                        _ => None,
                    }
                }
            })
            .await;
        }
        {
            let calls = Arc::clone(&calls);
            restore_media_with(&mut state, move |prog, args| {
                let call = format!("{prog} {}", args.join(" "));
                calls.lock().unwrap().push(call);
                async { Some(String::new()) }
            })
            .await;
        }
        let log = calls.lock().unwrap().clone();
        assert_eq!(
            log,
            vec![
                "playerctl -l",
                "playerctl -p spotify status",
                "playerctl -p spotify pause",
                "playerctl -p firefox status",
                "playerctl -p spotify play",
            ]
        );

        // 2. MediaDuck::Lower falls back to `wpctl` when `pamixer` is absent and restores exact volume.
        let wp_calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut wp_state = DuckState::Idle;
        {
            let wp_calls = Arc::clone(&wp_calls);
            duck_media_with(MediaDuck::Lower, &mut wp_state, move |prog, args| {
                let call = format!("{prog} {}", args.join(" "));
                wp_calls.lock().unwrap().push(call.clone());
                async move {
                    match call.as_str() {
                        "pamixer --get-volume" => None,
                        "wpctl get-volume @DEFAULT_AUDIO_SINK@" => {
                            Some("Volume: 0.80\n".to_string())
                        }
                        "wpctl set-volume @DEFAULT_AUDIO_SINK@ 0.28" => Some(String::new()),
                        _ => None,
                    }
                }
            })
            .await;
        }
        {
            let wp_calls = Arc::clone(&wp_calls);
            restore_media_with(&mut wp_state, move |prog, args| {
                let call = format!("{prog} {}", args.join(" "));
                wp_calls.lock().unwrap().push(call);
                async { Some(String::new()) }
            })
            .await;
        }
        assert_eq!(
            *wp_calls.lock().unwrap(),
            vec![
                "pamixer --get-volume",
                "wpctl get-volume @DEFAULT_AUDIO_SINK@",
                "wpctl set-volume @DEFAULT_AUDIO_SINK@ 0.28",
                "wpctl set-volume @DEFAULT_AUDIO_SINK@ 0.80",
            ]
        );
    }

    #[test]
    fn prepare_document_for_speech_skips_code_math_images_tables_and_handles_alerts() {
        let doc = "\
# Morning Brief

> [!NOTE]
> Rain arrives at **15:00** in [Hamburg](https://example.test).

```rust
fn secret_code() { println!(\"skip me\"); }
```

$$
E = mc^2
$$

Here is the first sentence. Second sentence with *emphasis* and [image: chart](https://x.test/a.png).

| Service | State |
| --- | :---: |
| Daemon | Ready |

- [x] Buy coffee
- Review PR #88
";
        let sentences = prepare_document_for_speech(doc);
        assert_eq!(
            sentences,
            vec![
                "Morning Brief",
                "Note: Rain arrives at 15:00 in Hamburg.",
                "Here is the first sentence.",
                "Second sentence with emphasis and .",
                "Service, State",
                "Daemon, Ready",
                "Buy coffee",
                "Review PR #88",
            ]
        );
        // Single English heading + German body latches German over first ~160 chars.
        let de_doc =
            "# Dashboard\n\nHeute ist es im Büro ruhig und wir haben am 25. Mai einen Termin.";
        let de_sents = prepare_document_for_speech(de_doc);
        assert_eq!(detect_document_lang(&de_sents), "de");
    }

    #[tokio::test]
    async fn desktop_read_aloud_bypasses_speak_enabled_and_rich_cutoff_and_local_cancels() {
        let dir = std::env::temp_dir().join(format!("hc-read-aloud-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("spoken.log");

        let mut cfg = PresentConfig {
            notifications: false,
            ..PresentConfig::default()
        };
        // Notice: speak.enabled is FALSE, yet on-demand ReadAloud must still speak!
        cfg.speak = SpeakConfig {
            enabled: false,
            voice_only: true,
            command_en: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("printf 'en:%s\\n' \"$(cat)\" >> '{}'", log_path.display()),
                "sh".to_string(),
            ],
            command_de: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("printf 'de:%s\\n' \"$(cat)\" >> '{}'", log_path.display()),
                "sh".to_string(),
            ],
            media_duck: MediaDuck::None,
            ..SpeakConfig::default()
        };

        let desktop = Desktop::spawn(&cfg, dir.join("answers"));
        let (_id, done_rx) = desktop
            .read_aloud("# Summary\n\n```sh\necho skip\n```\n\nThe server is healthy.".to_string())
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), done_rx)
            .await
            .expect("read_aloud should finish")
            .unwrap();
        desktop.finish().await;

        let spoken = std::fs::read_to_string(&log_path).unwrap();
        assert!(spoken.contains("en:Summary\n"), "{spoken}");
        assert!(spoken.contains("en:The server is healthy.\n"), "{spoken}");
        assert!(!spoken.contains("echo skip"), "{spoken}");

        // Offline `speak_text_local` cancels promptly when watch receiver flips true.
        let local_pid = dir.join("local.pid");
        let mut slow_speak = cfg.speak.clone();
        slow_speak.command_en = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("echo $$ > '{}'; exec sleep 10", local_pid.display()),
            "sh".to_string(),
        ];
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            speak_text_local(
                &slow_speak,
                "First sentence. Second sentence.".to_string(),
                cancel_rx,
            )
            .await
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !local_pid.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(local_pid.exists());
        let t0 = Instant::now();
        let _ = cancel_tx.send(true);
        let completed = task.await.unwrap().unwrap();
        assert!(!completed);
        assert!(t0.elapsed() < Duration::from_secs(2));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn format_summary_with_icons_uses_two_space_separator_and_caps_overflow() {
        assert_eq!(format_summary_with_icons("Thinking…", &[], 8), "Thinking…");
        assert_eq!(
            format_summary_with_icons("Thinking…", &["\u{e8a4}".to_string()], 8),
            "Thinking…  \u{e8a4}"
        );
        assert_eq!(
            format_summary_with_icons(
                "Answer (deep)",
                &["a".to_string(), "b".to_string(), "c".to_string()],
                3
            ),
            "Answer (deep)  a b c"
        );
        assert_eq!(
            format_summary_with_icons(
                "Answer (deep)",
                &["a".to_string(), "b".to_string(), "c".to_string()],
                2
            ),
            "Answer (deep)  a b …"
        );
    }

    #[tokio::test]
    async fn tool_icons_render_on_progress_answer_task_failure_and_preserve_coalesced_status() {
        let dir = std::env::temp_dir().join(format!("hc-tool-icons-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let calls = dir.join("notify.log");
        let mut config = recording_config(&calls, &[]);
        config.clipboard.clear();
        config.tool_icons.enabled = true;
        config.tool_icons.max_icons = 2;

        let desktop = Desktop::spawn_with_options(
            &config,
            dir.join("answers"),
            VIEWER_GRACE,
            Duration::from_millis(120),
        );
        desktop.present(Presentation::Progress {
            id: 1,
            request: "check sprint".to_string(),
            profile: Some("deep".to_string()),
            thinking_label: Some("Pondering Hard…".to_string()),
        });
        // First status + icon flushes immediately; second status + icon-only update
        // coalesce inside the throttle window without dropping the status text.
        desktop.present(Presentation::ProgressUpdate {
            id: 1,
            status: "slack: list".to_string(),
            icons: vec!["\u{e8a4}".to_string()],
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if std::fs::read_to_string(&calls)
                .unwrap_or_default()
                .contains("slack: list")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        desktop.present(Presentation::ProgressUpdate {
            id: 1,
            status: "linear: issues".to_string(),
            icons: vec!["\u{e8a4}".to_string()],
        });
        desktop.present(Presentation::ProgressUpdate {
            id: 1,
            status: String::new(),
            icons: vec!["\u{e8a4}".to_string(), "\u{f145}".to_string()],
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if std::fs::read_to_string(&calls)
                .unwrap_or_default()
                .contains("\u{e8a4} \u{f145}")
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
            widgets: vec![],
            icons: vec![
                "\u{e8a4}".to_string(),
                "\u{f145}".to_string(),
                "\u{e7eb}".to_string(),
            ],
        });
        desktop.present(Presentation::TaskComplete {
            id: 2,
            request: "brief".to_string(),
            text: "Ready.".to_string(),
            denied: vec![],
            profile: Some("deep".to_string()),
            widgets: vec![],
            icons: vec!["\u{f455}".to_string(), "\u{f42f}".to_string()],
        });
        desktop.present(Presentation::Failure {
            id: 3,
            reason: FailReason::Timeout,
            message: "timed out".to_string(),
            profile: Some("deep".to_string()),
            background: false,
            icons: vec!["\u{e8a4}".to_string()],
        });
        desktop.finish().await;

        let recorded = std::fs::read_to_string(&calls).unwrap_or_default();
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(
            recorded
                .contains("Pondering Hard…  \u{e8a4} \u{f145} check sprint\n<b>linear: issues</b>"),
            "{recorded}"
        );
        assert!(
            recorded.contains("Answer (deep)  \u{e8a4} \u{f145} … Done."),
            "{recorded}"
        );
        assert!(
            recorded.contains("Task complete (deep)  \u{f455} \u{f42f} Ready."),
            "{recorded}"
        );
        assert!(
            recorded.contains("Request timed out (deep)  \u{e8a4} timed out"),
            "{recorded}"
        );
    }

    #[tokio::test]
    async fn notify_send_basename_receives_hey_computer_boolean_hint() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("hc-notify-hint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let calls = dir.join("notify.log");
        let fake_notify_send = dir.join("notify-send");
        std::fs::write(
            &fake_notify_send,
            format!("#!/bin/sh\necho \"$*\" >> '{}'\necho 11\n", calls.display()),
        )
        .unwrap();
        std::fs::set_permissions(&fake_notify_send, std::fs::Permissions::from_mode(0o755))
            .unwrap();

        let mut config = PresentConfig {
            notifier: vec![fake_notify_send.to_string_lossy().into_owned()],
            clipboard: vec!["/bin/true".to_string()],
            ..PresentConfig::default()
        };
        let id = notify(&config, "Title", "Body", Urgency::Normal, 0, None).await;
        assert_eq!(id, Some(11));

        // Action notifier path also passes the hint when basename is `notify-send`.
        let desktop = Desktop::spawn(&config, dir.join("answers"));
        desktop.present(Presentation::Answer {
            id: 1,
            text: "Short.".to_string(),
            denied: vec![],
            profile: None,
            widgets: vec![],
            icons: vec![],
        });
        desktop.finish().await;

        let recorded = std::fs::read_to_string(&calls).unwrap();
        let lines: Vec<&str> = recorded.lines().collect();
        assert_eq!(lines.len(), 2, "{recorded}");
        for line in &lines {
            assert!(
                line.contains("--hint=boolean:x-hey-computer:true"),
                "missing hint: {line}"
            );
        }

        // Non-notify-send recording script does NOT get the hint.
        let calls2 = dir.join("notify2.log");
        config = recording_config(&calls2, &[]);
        let _ = notify(&config, "Title", "Body", Urgency::Normal, 0, None).await;
        let recorded2 = std::fs::read_to_string(&calls2).unwrap();
        assert!(
            !recorded2.contains("--hint=boolean:x-hey-computer:true"),
            "{recorded2}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
