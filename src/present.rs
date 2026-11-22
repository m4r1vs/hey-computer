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
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::{Notify, mpsc};

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
pub fn is_short(text: &str, limits: Limits) -> bool {
    let text = text.trim();
    text.chars().count() <= limits.max_chars
        && text.lines().count() <= limits.max_lines
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

/// `$…$` with a LaTeX command inside. A plain `$` is money ("$5 and $10").
fn has_latex_inline_math(text: &str) -> bool {
    text.lines().any(|line| {
        let pieces: Vec<&str> = line.split('$').collect();
        // Pieces 1, 3, 5, … sit between an opening and a closing dollar.
        pieces
            .iter()
            .enumerate()
            .skip(1)
            .step_by(2)
            .any(|(i, piece)| i + 1 < pieces.len() && piece.contains('\\'))
    })
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
        let (queue, mut items) = mpsc::unbounded_channel();
        let mut worker = Worker {
            config: config.clone(),
            answers_dir,
            progress: None,
        };
        let task = tokio::spawn(async move {
            while let Some(item) = items.recv().await {
                worker.show(item).await;
            }
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
}

#[derive(Clone, Copy)]
pub enum Urgency {
    Low,
    Normal,
}

impl Worker {
    async fn show(&mut self, item: Presentation) {
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

        match open_viewer(&self.config.viewer, &file) {
            Ok(()) => {
                self.notify(
                    "Answer opened",
                    &escape_markup(&headline(text, 120)),
                    Urgency::Low,
                    0,
                    replaces,
                )
                .await;
            }
            Err(e) => {
                tracing::warn!("cannot start the viewer: {e:#}");
                let body = format!("{e:#}\n{}", file.display());
                self.notify(
                    "Cannot open the viewer",
                    &escape_markup(&body),
                    Urgency::Normal,
                    FAILURE_EXPIRE_MS,
                    replaces,
                )
                .await;
            }
        }
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

/// Start the viewer on `file` and let it run on its own.
fn open_viewer(argv: &[String], file: &Path) -> anyhow::Result<()> {
    let file = file.to_string_lossy().into_owned();
    let mut args: Vec<String> = argv[1..]
        .iter()
        .map(|a| a.replace("{file}", &file))
        .collect();
    if !argv.iter().any(|a| a.contains("{file}")) {
        args.push(file.clone());
    }
    let program = argv[0].replace("{file}", &file);
    let mut child = Command::new(&program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Its own process group: a viewer is a window of its own and must not
        // get the daemon's signals.
        .process_group(0)
        .spawn()
        .map_err(|e| anyhow::anyhow!("starting {program}: {e}"))?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
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
            "It costs $5 and the other one $10.",
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
            "Euler: \\(e^{i\\pi}\\)",
        ] {
            assert!(!is_short(text, LIMITS), "{text}");
        }
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
}
