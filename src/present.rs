//! Showing answers: a notification when the answer is short and plain, a
//! Markdown viewer when it is long or rich.
//!
//! Everything goes through [`Presenter`], so the daemon can be tested with a
//! [`Recorder`] and never touch the desktop. The real one, [`Desktop`], runs
//! the notifier as a program (notify-send by default) rather than talking
//! D-Bus itself: an argument vector can be swapped for a script, which is how
//! the daemon is tested by hand without popping anything up.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinSet;

use crate::config::Present as PresentConfig;
use crate::protocol::FailReason;

const APP_NAME: &str = "hey-computer";

/// The notifier gets this long before it is given up on.
const NOTIFIER_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a failure stays up.
const FAILURE_EXPIRE_MS: u32 = 8_000;
/// "Thinking…" is replaced by the answer; this only bounds a popup whose
/// daemon died before it could replace it.
const PROGRESS_EXPIRE_MS: u32 = 120_000;
const CANCELLED_EXPIRE_MS: u32 = 2_000;
/// The request echoed in the progress popup is cut here.
const REQUEST_PREVIEW_CHARS: usize = 160;
/// A viewer that exits with an error within this long is reported as
/// failed; one that is still running then has its window up.
const VIEWER_GRACE: Duration = Duration::from_millis(1500);
/// How much of a failed viewer's stderr goes into the message.
const VIEWER_STDERR_KEPT: usize = 400;

#[derive(Debug, Clone, PartialEq)]
pub enum Presentation {
    /// A request has started.
    Progress { id: u64, request: String },
    Answer {
        id: u64,
        text: String,
        /// Commands the agent was refused on the way.
        denied: Vec<String>,
    },
    Failure {
        id: u64,
        reason: FailReason,
        message: String,
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
    let mut out = String::with_capacity(markdown.len());
    let mut fence: Option<String> = None;
    let mut in_math = false;
    for line in markdown.split_inclusive('\n') {
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
        match &fence {
            Some(open) => {
                if is_fence
                    && marker.starts_with(open.as_str())
                    && trimmed[marker.len()..].trim().is_empty()
                {
                    fence = None;
                }
                out.push_str(line);
            }
            None if is_fence => {
                fence = Some(marker);
                out.push_str(line);
            }
            None => out.push_str(&disarm_line(line, &mut in_math)),
        }
    }
    out
}

/// One line outside a fenced block. `in_math` carries a `$$` display block
/// over to the next line.
fn disarm_line(line: &str, in_math: &mut bool) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    if *in_math {
        match rest.find("$$") {
            Some(end) => {
                out.push_str(&math(&rest[..end + 2]));
                rest = &rest[end + 2..];
                *in_math = false;
            }
            None => return math(rest),
        }
    }
    while let Some(i) = rest.find(['`', '<', '$', '\\', '!']) {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        // The closing delimiter of a span that opens here, if any.
        let span = |open: &str, close: &str| {
            rest.strip_prefix(open)
                .and_then(|inner| inner.find(close))
                .map(|end| open.len() + end + close.len())
        };
        if rest.starts_with('`') {
            // A code span closes with a run of as many backticks.
            let run = rest.len() - rest.trim_start_matches('`').len();
            let end = span(&rest[..run], &rest[..run]).unwrap_or(run);
            out.push_str(&rest[..end]);
            rest = &rest[end..];
        } else if rest.starts_with("$$") {
            match span("$$", "$$") {
                Some(end) => {
                    out.push_str(&math(&rest[..end]));
                    rest = &rest[end..];
                }
                None => {
                    // A display block that goes on below.
                    *in_math = true;
                    out.push_str(&math(rest));
                    return out;
                }
            }
        } else if let Some(end) = span("$", "$")
            .or_else(|| span("\\(", "\\)"))
            .or_else(|| span("\\[", "\\]"))
        {
            out.push_str(&math(&rest[..end]));
            rest = &rest[end..];
        } else if rest.starts_with("![") && is_remote_image(rest) {
            // Shown as a link: the web view would fetch it at once.
            out.push_str("[image: ");
            rest = &rest[2..];
        } else if rest.starts_with("\\<") {
            // Markdown's own escape for a `<` shown as it is.
            out.push_str("&lt;");
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
    out.push_str(rest);
    out
}

/// Math with `<` spelt as KaTeX's `\lt`.
fn math(text: &str) -> String {
    text.replace('<', "\\lt ")
}

/// `![alt](http…)`, `![alt](//host/…)` or a reference image `![alt][ref]`
/// (whose target may well be remote). A local image stays an image.
fn is_remote_image(text: &str) -> bool {
    let Some(close) = text.find(']') else {
        return false;
    };
    let after = &text[close + 1..];
    if after.starts_with('[') {
        return true;
    }
    let Some(target) = after.strip_prefix('(') else {
        return false;
    };
    let target = target
        .trim_start()
        .trim_start_matches('<')
        .to_ascii_lowercase();
    ["http://", "https://", "//", "ftp://"]
        .iter()
        .any(|scheme| target.starts_with(scheme))
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
        Self::spawn_with_grace(config, answers_dir, VIEWER_GRACE)
    }

    fn spawn_with_grace(
        config: &PresentConfig,
        answers_dir: PathBuf,
        viewer_grace: Duration,
    ) -> Arc<Self> {
        let (queue, mut items) = mpsc::unbounded_channel();
        let mut worker = Worker {
            config: config.clone(),
            answers_dir,
            progress: None,
            watchers: JoinSet::new(),
            viewer_grace,
        };
        let task = tokio::spawn(async move {
            while let Some(item) = items.recv().await {
                worker.show(item).await;
            }
            // Let any viewer started in the last VIEWER_GRACE finish its
            // early-exit check before exit; long-running viewers are already
            // handed off to detached reapers.
            while worker.watchers.join_next().await.is_some() {}
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

struct Worker {
    config: PresentConfig,
    answers_dir: PathBuf,
    /// The "Thinking…" popup on screen, which the next popup replaces.
    progress: Option<u32>,
    /// Bounded early-exit watchers for recently launched viewers.
    watchers: JoinSet<()>,
    viewer_grace: Duration,
}

#[derive(Clone, Copy)]
pub enum Urgency {
    Low,
    Normal,
}

impl Worker {
    async fn show(&mut self, item: Presentation) {
        while self.watchers.try_join_next().is_some() {}
        match item {
            Presentation::Progress { request, .. } => {
                if !self.config.progress {
                    return;
                }
                let body = escape_markup(&truncate(request.trim(), REQUEST_PREVIEW_CHARS));
                // A superseded request's popup is reused for the next one.
                let replaces = self.progress.take();
                self.progress = self
                    .notify(
                        "Thinking…",
                        &body,
                        Urgency::Low,
                        PROGRESS_EXPIRE_MS,
                        replaces,
                    )
                    .await;
            }
            Presentation::Answer { id, text, denied } => self.answer(id, &text, &denied).await,
            Presentation::Failure {
                reason: reason @ (FailReason::Cancelled | FailReason::Superseded),
                ..
            } => {
                // Short-lived, so a popup does not linger when the newer
                // request shows none (one asked with `wait`). A superseded
                // request's popup id is kept for the next progress popup,
                // which then replaces this one in place.
                if let Some(replaces) = self.progress.take() {
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
                        self.progress = shown;
                    }
                }
            }
            Presentation::Failure {
                reason, message, ..
            } => {
                let replaces = self.progress.take();
                self.notify(
                    reason.headline(),
                    &escape_markup(&message),
                    Urgency::Normal,
                    FAILURE_EXPIRE_MS,
                    replaces,
                )
                .await;
            }
        }
    }

    async fn answer(&mut self, id: u64, text: &str, denied: &[String]) {
        let replaces = self.progress.take();
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
            self.notify("Answer", &body, Urgency::Normal, 0, replaces)
                .await;
            return;
        }

        let mut document = text.trim().to_string();
        if !refused.is_empty() {
            document.push_str("\n\n---\n\n");
            document.push_str(&refused);
        }
        document.push('\n');
        let document = disarm_html(&document);
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
            self.notify(
                "Answer saved",
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
        let opened_id = self
            .notify(
                "Answer opened",
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

/// Write a long answer where the viewer can read it, and drop old ones.
fn write_answer(dir: &Path, id: u64, document: &str, keep: usize) -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    // An existing directory keeps its mode; make sure it is private.
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restricting {}", dir.display()))?;

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let file = dir.join(format!("answer-{stamp}-{id}.md"));
    {
        use std::io::Write;
        let mut handle = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&file)
            .with_context(|| format!("creating {}", file.display()))?;
        handle
            .write_all(document.as_bytes())
            .with_context(|| format!("writing {}", file.display()))?;
    }
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

/// Start the viewer on `file` and return the running child with its unlinked
/// stderr file so "Answer opened" can be shown at once while [`watch_viewer`]
/// checks for an early exit.
fn spawn_viewer(argv: &[String], file: &Path) -> anyhow::Result<SpawnedViewer> {
    use std::os::unix::fs::OpenOptionsExt;

    // Its stderr goes to a file, not a pipe: with a pipe, the viewer's first
    // write after the daemon exits (a restart, a config change) would kill
    // it with SIGPIPE, and its window with it. The file is unlinked at once;
    // the viewer keeps writing to it until it exits.
    let log_path = file.with_extension("viewer-stderr");
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
        .stderr(Stdio::from(stderr))
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
            let detail = viewer_failure_detail(&viewer.program, status, &mut viewer.log);
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

fn viewer_failure_detail(
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
        });
        desktop.present(Presentation::Answer {
            id: 1,
            text: "# Heading\n\nbody".to_string(),
            denied: vec![],
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
        });
        desktop.present(Presentation::Answer {
            id: 1,
            text: "# Heading\n\nbody".to_string(),
            denied: vec![],
        });
        desktop.present(Presentation::Progress {
            id: 2,
            request: "second".to_string(),
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
        let desktop =
            Desktop::spawn_with_grace(&config, dir.join("answers"), Duration::from_millis(150));
        desktop.present(Presentation::Answer {
            id: 1,
            text: "# Heading\n\nbody".to_string(),
            denied: vec![],
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
}
