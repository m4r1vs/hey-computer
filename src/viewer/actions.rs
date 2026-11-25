//! Real host actions for the production viewer: bounded code execution, local
//! Vault Journal appends/task-toggles, `0700`/`0600` skill template writing,
//! human-gated `distill_diff` application, and `openhue` room/scene control.

use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hey_computer::config::Config;
use hey_computer::store;
use serde::{Deserialize, Serialize};
use serde_json::Value;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct SnippetOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub timed_out: bool,
}

/// Execute a runnable snippet (`bash`, `zsh`, `python`/`py`, or `sh`/`shell`)
/// in its own process group with a wall-clock timeout and 64 KiB output caps.
pub fn execute_snippet_with_timeout(code: &str, lang: &str, timeout: Duration) -> SnippetOutcome {
    let started = Instant::now();
    let norm_lang = lang.trim().to_ascii_lowercase();
    let mut cmd = match norm_lang.as_str() {
        "bash" => {
            let mut c = Command::new("bash");
            c.arg("-c").arg(code);
            c
        }
        "zsh" => {
            let mut c = Command::new("zsh");
            c.arg("-c").arg(code);
            c
        }
        "python" | "py" => {
            let mut c = Command::new("python3");
            c.arg("-c").arg(code);
            c
        }
        _ => {
            let mut c = Command::new("sh");
            c.arg("-c").arg(code);
            c
        }
    };

    cmd.process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => {
            return SnippetOutcome {
                stdout: String::new(),
                stderr: format!("Failed to spawn `{norm_lang}` runner: {err}"),
                exit_code: 127,
                duration_ms: started.elapsed().as_millis() as u64,
                timed_out: false,
            };
        }
    };

    let pid = child.id() as i32;
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let out_handle = std::thread::spawn(move || read_bounded_pipe(stdout_pipe, 64 * 1024));
    let err_handle = std::thread::spawn(move || read_bounded_pipe(stderr_pipe, 64 * 1024));

    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if started.elapsed() >= timeout {
                    timed_out = true;
                    if pid > 0 {
                        // SAFETY: child was spawned in its own process group (`process_group(0)`),
                        // so `-pid` targets only the snippet and its descendants.
                        unsafe {
                            libc::kill(-pid, libc::SIGKILL);
                        }
                    }
                    let _ = child.kill();
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break None,
        }
    };

    let stdout = out_handle.join().unwrap_or_default();
    let mut stderr = err_handle.join().unwrap_or_default();
    if timed_out {
        if !stderr.is_empty() && !stderr.ends_with('\n') {
            stderr.push('\n');
        }
        stderr.push_str(&format!(
            "[hey-computer-viewer] Timed out after {:.1}s.",
            timeout.as_secs_f64()
        ));
    }
    let exit_code = if timed_out {
        124
    } else {
        status.and_then(|s| s.code()).unwrap_or(-1)
    };

    SnippetOutcome {
        stdout,
        stderr,
        exit_code,
        duration_ms: started.elapsed().as_millis() as u64,
        timed_out,
    }
}

fn read_bounded_pipe<R: Read>(pipe: Option<R>, cap: usize) -> String {
    let Some(pipe) = pipe else {
        return String::new();
    };
    let mut buf = Vec::new();
    let _ = pipe.take((cap + 1) as u64).read_to_end(&mut buf);
    let truncated = buf.len() > cap;
    if truncated {
        buf.truncate(cap);
    }
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        text.push_str("\n… [output truncated at 64 KiB]");
    }
    text
}

/* --------------------------------------------------------------------------
Vault Journal Appends, Task Prepending & Checkbox Toggling (Local Date)
-------------------------------------------------------------------------- */

#[derive(Debug, Clone)]
pub struct VaultSaveOutcome {
    pub saved: bool,
    pub preview_only: bool,
    pub quiet: bool,
    pub path: String,
    pub preview: String,
    pub message: String,
}

fn normalize_task_line(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with("- [ ] ")
        || trimmed.starts_with("- [x] ")
        || trimmed.starts_with("- [X] ")
    {
        trimmed.to_string()
    } else if let Some(rest) = trimmed.strip_prefix("- ") {
        format!("- [ ] {}", rest.trim())
    } else {
        format!("- [ ] {trimmed}")
    }
}

fn is_markdown_task_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(after_dash) = trimmed.strip_prefix("- [") else {
        return false;
    };
    let mut chars = after_dash.chars();
    let Some(mark) = chars.next() else {
        return false;
    };
    matches!(mark, ' ' | 'x' | 'X') && chars.next() == Some(']')
}

/// Insert a `- [ ] ...` task line at the top of the first task list (`- [` line)
/// in `existing`, or immediately after the top `# ...` heading if no tasks exist
/// yet, or at the top of the file when neither is present.
pub fn insert_journal_task_prepend(existing: &str, task_line: &str) -> String {
    let clean = normalize_task_line(task_line);
    let normalized = existing.replace("\r\n", "\n");
    if normalized.trim().is_empty() {
        return format!("{clean}\n");
    }

    // 1. Before the first `- [` task line.
    let mut byte_offset = 0usize;
    for line in normalized.split_inclusive('\n') {
        if is_markdown_task_line(line) {
            let indent_len = line.len() - line.trim_start().len();
            let indent = &line[..indent_len];
            let mut out = String::with_capacity(normalized.len() + clean.len() + indent_len + 2);
            out.push_str(&normalized[..byte_offset]);
            out.push_str(indent);
            out.push_str(&clean);
            out.push('\n');
            out.push_str(&normalized[byte_offset..]);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            return out;
        }
        byte_offset += line.len();
    }

    // 2. Immediately after the top `# ...` heading when no tasks exist yet.
    byte_offset = 0;
    for line in normalized.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("# ") || trimmed == "#" {
            let end_of_heading = byte_offset + line.len();
            let head = normalized[..end_of_heading].trim_end_matches('\n');
            let rest = normalized[end_of_heading..].trim_start_matches('\n');
            return if rest.is_empty() {
                format!("{head}\n\n{clean}\n")
            } else {
                format!("{head}\n\n{clean}\n\n{}", ensure_trailing_newline(rest))
            };
        }
        byte_offset += line.len();
    }

    // 3. Top of file fallback.
    format!(
        "{clean}\n\n{}",
        ensure_trailing_newline(normalized.trim_start_matches('\n'))
    )
}

/// Toggle `- [ ]` <-> `- [x]` on the matching task line inside a Journal note.
pub fn toggle_journal_task_line(
    existing: &str,
    task_text: &str,
    desired_done: Option<bool>,
) -> Option<(String, bool)> {
    let target = task_text.trim();
    if target.is_empty() {
        return None;
    }

    let normalized = existing.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.lines().collect();

    let find_match = |exact: bool| -> Option<(usize, bool, String)> {
        for (idx, &line) in lines.iter().enumerate() {
            let indent_len = line.len() - line.trim_start().len();
            let indent = &line[..indent_len];
            let trimmed = &line[indent_len..];
            let (was_done, body) = if let Some(rest) = trimmed.strip_prefix("- [ ]") {
                (false, rest.trim())
            } else if let Some(rest) = trimmed
                .strip_prefix("- [x]")
                .or_else(|| trimmed.strip_prefix("- [X]"))
            {
                (true, rest.trim())
            } else {
                continue;
            };

            let body_no_tag = strip_trailing_hashtag(body);
            let matched = if exact {
                body == target || body_no_tag == target
            } else {
                body.eq_ignore_ascii_case(target)
                    || body_no_tag.eq_ignore_ascii_case(target)
                    || body.contains(target)
            };
            if matched {
                let next_done = desired_done.unwrap_or(!was_done);
                let mark = if next_done { "x" } else { " " };
                let replaced = format!("{indent}- [{mark}] {body}");
                return Some((idx, next_done, replaced));
            }
        }
        None
    };

    let (line_idx, next_done, replaced_line) = find_match(true).or_else(|| find_match(false))?;

    let mut out_lines: Vec<String> = lines.into_iter().map(String::from).collect();
    out_lines[line_idx] = replaced_line;
    let mut joined = out_lines.join("\n");
    joined.push('\n');
    Some((joined, next_done))
}

fn strip_trailing_hashtag(s: &str) -> &str {
    let trimmed = s.trim_end();
    if let Some((before, last_word)) = trimmed.rsplit_once(char::is_whitespace)
        && last_word.starts_with('#')
        && last_word.len() > 1
    {
        return before.trim_end();
    }
    trimmed
}

pub fn save_to_vault(
    vault_dir: Option<&Path>,
    mode: &str,
    title: &str,
    markdown: &str,
) -> Result<VaultSaveOutcome> {
    let date = store::local_today_ymd();
    let rel_note = format!("Journal/{date}.md");
    let is_prepend = mode == "prepend_task";
    let preview_entry = if is_prepend {
        format!("{}\n", normalize_task_line(markdown))
    } else {
        format!("\n## Ask Computer · {title}\n\n{}\n", markdown.trim())
    };

    let Some(vault_root) = vault_dir.filter(|p| !p.as_os_str().is_empty()) else {
        let preview_path = format!("<vault>/{rel_note}");
        let msg = format!("Safe preview (configure instructions.vault to write): {rel_note}");
        return Ok(VaultSaveOutcome {
            saved: false,
            preview_only: true,
            quiet: false,
            path: preview_path,
            preview: preview_entry.trim_start().to_string(),
            message: msg,
        });
    };

    let journal_dir = vault_root.join("Journal");
    std::fs::create_dir_all(&journal_dir)
        .with_context(|| format!("creating {}", journal_dir.display()))?;
    let target = journal_dir.join(format!("{date}.md"));

    if is_prepend {
        let existing = match std::fs::read_to_string(&target) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", target.display())),
        };
        let updated = insert_journal_task_prepend(&existing, markdown);
        atomic_write_preserving_mode(&target, updated.as_bytes(), None)?;
        let msg = format!("Added task to {}", target.display());
        Ok(VaultSaveOutcome {
            saved: true,
            preview_only: false,
            quiet: true,
            path: target.display().to_string(),
            preview: preview_entry.trim_start().to_string(),
            message: msg,
        })
    } else {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&target)
            .with_context(|| format!("opening {}", target.display()))?;
        f.write_all(preview_entry.as_bytes())
            .with_context(|| format!("appending to {}", target.display()))?;
        f.flush()?;
        let msg = format!("Appended answer to {}", target.display());
        Ok(VaultSaveOutcome {
            saved: true,
            preview_only: false,
            quiet: false,
            path: target.display().to_string(),
            preview: preview_entry.trim_start().to_string(),
            message: msg,
        })
    }
}

pub fn toggle_vault_journal_task(
    vault_dir: Option<&Path>,
    task_text: &str,
    desired_done: Option<bool>,
) -> Result<(PathBuf, bool)> {
    let Some(vault_root) = vault_dir.filter(|p| !p.as_os_str().is_empty()) else {
        bail!("instructions.vault is not configured");
    };
    let date = store::local_today_ymd();
    let target = vault_root.join("Journal").join(format!("{date}.md"));
    let existing = std::fs::read_to_string(&target)
        .with_context(|| format!("reading {}", target.display()))?;
    let Some((updated, new_done)) = toggle_journal_task_line(&existing, task_text, desired_done)
    else {
        bail!(
            "task `{}` not found in {}",
            task_text.trim(),
            target.display()
        );
    };
    atomic_write_preserving_mode(&target, updated.as_bytes(), None)?;
    Ok((target, new_done))
}

/* --------------------------------------------------------------------------
Skill Template & Distilled Skill Writing (`0700` dir, `0600` files)
-------------------------------------------------------------------------- */

#[derive(Debug, Clone)]
pub struct SkillSaveOutcome {
    pub saved: bool,
    pub preview_only: bool,
    pub skill_name: String,
    pub path: String,
    pub message: String,
}

/// Sanitize a skill directory slug to lowercase ASCII letters, digits, and `-`.
/// When `ensure_widget_prefix` is true (widget template saves), prefixes
/// `widget-` if not already `widget-*`.
pub fn sanitize_skill_slug(raw: &str, ensure_widget_prefix: bool) -> String {
    let base = raw
        .trim()
        .trim_end_matches("/SKILL.md")
        .rsplit('/')
        .next()
        .unwrap_or(raw.trim());
    let mut slug = String::with_capacity(base.len());
    for ch in base.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if matches!(ch, '-' | '_' | ' ') && !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let cleaned = slug.trim_matches('-');
    if cleaned.is_empty() {
        return if ensure_widget_prefix {
            "widget-custom".to_string()
        } else {
            "distilled-skill".to_string()
        };
    }
    if ensure_widget_prefix && !cleaned.starts_with("widget-") {
        if cleaned == "widget" {
            "widget-custom".to_string()
        } else {
            format!("widget-{cleaned}")
        }
    } else {
        cleaned.to_string()
    }
}

pub fn save_skill_files(
    skills_dir: Option<&Path>,
    raw_name: &str,
    skill_markdown: &str,
    template_html: &str,
    ensure_widget_prefix: bool,
) -> Result<SkillSaveOutcome> {
    let skill_name = sanitize_skill_slug(raw_name, ensure_widget_prefix);
    let Some(root) = skills_dir.filter(|p| !p.as_os_str().is_empty()) else {
        let preview_path = format!("$XDG_CONFIG_HOME/hey-computer/skills/{skill_name}/SKILL.md");
        let msg = format!("Safe preview (no skills directory configured): {preview_path}");
        return Ok(SkillSaveOutcome {
            saved: false,
            preview_only: true,
            skill_name,
            path: preview_path,
            message: msg,
        });
    };

    ensure_private_dir_0700(root)?;
    let dir = root.join(&skill_name);
    ensure_private_dir_0700(&dir)?;

    let skill_md_path = dir.join("SKILL.md");
    let clean_md = ensure_trailing_newline(skill_markdown.trim_end());
    atomic_write_private_0600(&skill_md_path, clean_md.as_bytes())?;

    if !template_html.trim().is_empty() {
        let clean_html = ensure_trailing_newline(template_html.trim_end());
        atomic_write_private_0600(&dir.join("template.html"), clean_html.as_bytes())?;
    }

    let path_str = skill_md_path.display().to_string();
    let msg = format!("Saved skill to {path_str}");
    Ok(SkillSaveOutcome {
        saved: true,
        preview_only: false,
        skill_name,
        path: path_str,
        message: msg,
    })
}

/* --------------------------------------------------------------------------
Distillation Review (`distill_diff` -> `approve_changes`)
-------------------------------------------------------------------------- */

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DistillHunk {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub section: String,
    #[serde(default)]
    pub before: Option<String>,
    #[serde(default)]
    pub after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillDraftInput {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub path: String,
    #[serde(default, alias = "preview_markdown")]
    pub preview_markdown: String,
}

#[derive(Debug, Clone)]
pub struct DistillApproveOutcome {
    pub target_file: PathBuf,
    pub applied_hunks: usize,
    pub combined_chars: usize,
    pub max_chars: usize,
    pub skill_saved_path: Option<PathBuf>,
    pub message: String,
}

/// Apply accepted distillation hunks (`add`, `update`, `remove`, `prune`) to an
/// existing `memory.md` body.
pub fn apply_distill_hunks(existing: &str, hunks: &[DistillHunk]) -> Result<String> {
    let mut content = existing.replace("\r\n", "\n");

    for hunk in hunks {
        let kind = hunk.kind.trim().to_ascii_lowercase();
        let before = hunk.before.as_deref().unwrap_or("").trim();
        let after = hunk.after.as_deref().unwrap_or("").trim();

        match kind.as_str() {
            "add" => {
                if after.is_empty() || content.contains(after) {
                    continue;
                }
                content = append_under_section(&content, &hunk.section, after);
            }
            "update" => {
                if before.is_empty() {
                    if !after.is_empty() && !content.contains(after) {
                        content = append_under_section(&content, &hunk.section, after);
                    }
                    continue;
                }
                if let Some(updated) = replace_block(&content, before, Some(after)) {
                    content = updated;
                } else if content.contains(after) && !after.is_empty() {
                    // Already applied idempotently.
                } else {
                    bail!(
                        "cannot apply update hunk `{}`: expected original text was not found",
                        if hunk.id.is_empty() {
                            "update"
                        } else {
                            &hunk.id
                        }
                    );
                }
            }
            "remove" | "prune" => {
                if before.is_empty() {
                    continue;
                }
                let replacement = (!after.is_empty()).then_some(after);
                if let Some(updated) = replace_block(&content, before, replacement) {
                    content = updated;
                }
            }
            other => bail!("unsupported distill hunk kind `{other}`"),
        }
    }

    Ok(collapse_excess_blank_lines(&content))
}

fn normalize_section_heading(section: &str) -> String {
    let trimmed = section.trim();
    if trimmed.is_empty() {
        "## Memory".to_string()
    } else if trimmed.starts_with('#') {
        trimmed.to_string()
    } else {
        format!("## {trimmed}")
    }
}

fn append_under_section(content: &str, section: &str, entry: &str) -> String {
    let heading = normalize_section_heading(section);
    let heading_title = heading.trim_start_matches('#').trim();
    let heading_level = heading.chars().take_while(|&c| c == '#').count().max(2);

    if content.trim().is_empty() {
        return format!("{heading}\n\n{entry}\n");
    }

    let lines: Vec<&str> = content.lines().collect();
    let mut found_section_idx: Option<usize> = None;
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            let line_title = trimmed.trim_start_matches('#').trim();
            if trimmed.eq_ignore_ascii_case(&heading)
                || (!heading_title.is_empty() && line_title.eq_ignore_ascii_case(heading_title))
            {
                found_section_idx = Some(idx);
                break;
            }
        }
    }

    if let Some(sec_idx) = found_section_idx {
        let mut insert_at = lines.len();
        for (idx, line) in lines.iter().enumerate().skip(sec_idx + 1) {
            let trimmed = line.trim_start();
            let hashes = trimmed.chars().take_while(|&c| c == '#').count();
            if hashes > 0 && hashes <= heading_level && trimmed[hashes..].starts_with([' ', '\t']) {
                insert_at = idx;
                break;
            }
        }
        // Trim trailing blank lines inside the section so the new bullet sits cleanly with its peers.
        let mut before_part: Vec<&str> = lines[..insert_at].to_vec();
        while before_part.last().is_some_and(|l| l.trim().is_empty()) {
            before_part.pop();
        }
        let after_part = &lines[insert_at..];
        let mut out = before_part.join("\n");
        if before_part.len() == sec_idx + 1 {
            out.push_str("\n\n");
        } else {
            out.push('\n');
        }
        out.push_str(entry);
        out.push('\n');
        if !after_part.is_empty() {
            out.push('\n');
            out.push_str(&after_part.join("\n"));
            out.push('\n');
        }
        return out;
    }

    format!("{}\n\n{heading}\n\n{entry}\n", content.trim_end())
}

fn replace_block(content: &str, before: &str, replacement: Option<&str>) -> Option<String> {
    if let Some(pos) = content.find(before) {
        let end = pos + before.len();
        let mut out = String::with_capacity(content.len() + replacement.map_or(0, str::len));
        out.push_str(&content[..pos]);
        match replacement.filter(|s| !s.is_empty()) {
            Some(rep) => {
                out.push_str(rep);
                out.push_str(&content[end..]);
            }
            None => {
                let rest = content[end..].strip_prefix('\n').unwrap_or(&content[end..]);
                out.push_str(rest);
            }
        }
        return Some(out);
    }

    // Fallback: line-trimmed match for single-line entries with minor whitespace differences.
    if !before.contains('\n') {
        let needle = before.trim();
        let lines: Vec<&str> = content.lines().collect();
        if let Some(idx) = lines.iter().position(|l| l.trim() == needle) {
            let mut updated: Vec<String> = Vec::with_capacity(lines.len());
            for (i, line) in lines.into_iter().enumerate() {
                if i == idx {
                    if let Some(rep) = replacement.filter(|s| !s.is_empty()) {
                        updated.push(rep.to_string());
                    }
                } else {
                    updated.push(line.to_string());
                }
            }
            let mut joined = updated.join("\n");
            if !joined.is_empty() {
                joined.push('\n');
            }
            return Some(joined);
        }
    }

    None
}

fn collapse_excess_blank_lines(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(trimmed.len() + 1);
    let mut blank_run = 0usize;
    for line in trimmed.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }
    out
}

pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn expand_target_memory_path(raw: &str, state_dir: Option<&Path>) -> PathBuf {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("$HEY_COMPUTER_STATE_DIR/")
        && let Some(sd) = state_dir
    {
        return sd.join(rest);
    }
    if let Some(rest) = trimmed.strip_prefix("$XDG_STATE_HOME/hey-computer/")
        && let Some(sd) = state_dir
    {
        return sd.join(rest);
    }
    if let Some(rest) = trimmed.strip_prefix("$XDG_STATE_HOME/")
        && let Some(xdg) = dirs::state_dir()
    {
        return xdg.join(rest);
    }
    PathBuf::from(trimmed)
}

/// Verify `target_path` is an absolute path outside `vault` (checking both
/// lexical normalization and symlink canonicalization) and not a read-only
/// secret/store mount.
pub fn validate_memory_target_path(target_path: &Path, vault: &Path) -> Result<PathBuf> {
    if target_path.as_os_str().is_empty() || !target_path.is_absolute() {
        bail!(
            "target memory file `{}` must be an absolute path",
            target_path.display()
        );
    }
    let lexical = normalize_lexical(target_path);
    for ro_prefix in ["/nix/store", "/run/agenix", "/run/secrets"] {
        if lexical.starts_with(ro_prefix) {
            bail!(
                "target memory file `{}` is inside read-only path `{ro_prefix}`",
                target_path.display()
            );
        }
    }

    // Refuse writing through a symlink at `target_path` itself so atomic rename
    // never clobbers a symlink or escapes via symlink target.
    if let Ok(meta) = std::fs::symlink_metadata(target_path)
        && meta.file_type().is_symlink()
    {
        bail!(
            "refusing to overwrite symlinked memory file `{}`",
            target_path.display()
        );
    }

    if !vault.as_os_str().is_empty() {
        let norm_vault = normalize_lexical(vault);
        if lexical.starts_with(&norm_vault) {
            bail!(
                "target memory file `{}` must not be inside instructions.vault (`{}`)",
                target_path.display(),
                vault.display()
            );
        }

        let canon_vault = std::fs::canonicalize(vault).unwrap_or_else(|_| norm_vault.clone());
        let resolved_target = canonicalize_with_missing_tail(target_path);
        if resolved_target.starts_with(&canon_vault) {
            bail!(
                "target memory file `{}` resolves inside instructions.vault (`{}`)",
                target_path.display(),
                vault.display()
            );
        }
    }

    Ok(lexical)
}

fn canonicalize_with_missing_tail(path: &Path) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(path) {
        return canon;
    }
    let mut cur = path;
    let mut tail = Vec::new();
    while let Some(parent) = cur.parent() {
        if let Some(name) = cur.file_name() {
            tail.push(name.to_os_string());
        }
        if let Ok(canon_parent) = std::fs::canonicalize(parent) {
            let mut out = canon_parent;
            for part in tail.into_iter().rev() {
                out.push(part);
            }
            return out;
        }
        cur = parent;
    }
    normalize_lexical(path)
}

fn compute_combined_memory_chars(
    config: &Config,
    target_path: &Path,
    new_target_content: &str,
) -> usize {
    let norm_target = canonicalize_with_missing_tail(target_path);
    let mut paths: Vec<PathBuf> = Vec::new();
    if !config.instructions.memory_file.as_os_str().is_empty() {
        paths.push(config.instructions.memory_file.clone());
    }
    for extra in &config.instructions.memory_files {
        if !extra.as_os_str().is_empty() && !paths.contains(extra) {
            paths.push(extra.clone());
        }
    }
    let target_listed = paths
        .iter()
        .any(|p| canonicalize_with_missing_tail(p) == norm_target);
    if !target_listed {
        paths.push(target_path.to_path_buf());
    }

    let mut chunks: Vec<String> = Vec::new();
    for p in &paths {
        if canonicalize_with_missing_tail(p) == norm_target {
            let trimmed = new_target_content.trim();
            if !trimmed.is_empty() {
                chunks.push(trimmed.to_string());
            }
        } else if let Ok(raw) = std::fs::read_to_string(p) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                chunks.push(trimmed.to_string());
            }
        }
    }
    chunks.join("\n\n").chars().count()
}

pub fn approve_distill_changes(
    config: &Config,
    state_dir: Option<&Path>,
    skills_dir: Option<&Path>,
    payload: &Value,
    fallback_widget_data: Option<&Value>,
) -> Result<DistillApproveOutcome> {
    let raw_target = payload
        .get("targetFile")
        .or_else(|| payload.get("target_file"))
        .and_then(Value::as_str)
        .or_else(|| {
            fallback_widget_data
                .and_then(|d| d.get("targetFile").or_else(|| d.get("target_file")))
                .and_then(Value::as_str)
        })
        .unwrap_or("");
    if raw_target.trim().is_empty() {
        bail!("distill_diff is missing targetFile");
    }

    let expanded_target = expand_target_memory_path(raw_target, state_dir);
    let target_file = validate_memory_target_path(&expanded_target, &config.instructions.vault)?;

    let accepted_hunks = resolve_accepted_hunks(payload, fallback_widget_data)?;
    let include_skill = payload
        .get("includeSkill")
        .or_else(|| payload.get("include_skill"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let existing_memory = match std::fs::read_to_string(&target_file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(e).with_context(|| format!("reading {}", target_file.display()));
        }
    };

    let updated_memory = if accepted_hunks.is_empty() {
        existing_memory.clone()
    } else {
        apply_distill_hunks(&existing_memory, &accepted_hunks)?
    };

    let max_chars = config.instructions.memory_max_chars;
    let combined_chars = compute_combined_memory_chars(config, &target_file, &updated_memory);
    if combined_chars > max_chars {
        bail!(
            "approved memory changes ({combined_chars} chars) exceed instructions.memory_max_chars ({max_chars}); skip or prune hunks first"
        );
    }

    if !accepted_hunks.is_empty() {
        if let Some(parent) = target_file.parent()
            && !parent.as_os_str().is_empty()
        {
            if state_dir.is_some_and(|sd| parent.starts_with(sd)) {
                ensure_private_dir_0700(parent)?;
            } else {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        atomic_write_private_0600(&target_file, updated_memory.as_bytes())?;
    }

    let mut skill_saved_path = None;
    if include_skill
        && let Some(draft) = resolve_skill_draft(payload, fallback_widget_data)
        && !draft.preview_markdown.trim().is_empty()
    {
        let raw_name = if !draft.name.trim().is_empty() {
            draft.name.as_str()
        } else {
            draft.path.as_str()
        };
        let Some(root) = skills_dir.filter(|p| !p.as_os_str().is_empty()) else {
            bail!("no user skills directory configured for saving distilled SKILL.md");
        };
        let saved = save_skill_files(Some(root), raw_name, &draft.preview_markdown, "", false)?;
        skill_saved_path = Some(PathBuf::from(saved.path));
    }

    let mut summary_parts = Vec::new();
    if !accepted_hunks.is_empty() {
        summary_parts.push(format!(
            "Applied {} hunk(s) to {} ({combined_chars}/{max_chars} chars)",
            accepted_hunks.len(),
            target_file.display()
        ));
    }
    if let Some(ref sp) = skill_saved_path {
        summary_parts.push(format!("saved {}", sp.display()));
    }
    if summary_parts.is_empty() {
        summary_parts.push("No distillation changes selected".to_string());
    }

    Ok(DistillApproveOutcome {
        target_file,
        applied_hunks: accepted_hunks.len(),
        combined_chars,
        max_chars,
        skill_saved_path,
        message: summary_parts.join("; "),
    })
}

fn resolve_accepted_hunks(
    payload: &Value,
    fallback_widget_data: Option<&Value>,
) -> Result<Vec<DistillHunk>> {
    // 1. Direct hunk objects in `payload.hunks`.
    if let Some(arr) = payload.get("hunks").and_then(Value::as_array) {
        let mut out = Vec::with_capacity(arr.len());
        for item in arr {
            if item.is_object() {
                out.push(serde_json::from_value::<DistillHunk>(item.clone())?);
            }
        }
        if !out.is_empty() || payload.get("acceptedHunks").is_none() {
            return Ok(out);
        }
    }

    let Some(accepted_val) = payload
        .get("acceptedHunks")
        .or_else(|| payload.get("accepted_hunks"))
        .and_then(Value::as_array)
    else {
        return Ok(Vec::new());
    };

    if accepted_val.is_empty() {
        return Ok(Vec::new());
    }

    // 2. `acceptedHunks` passed directly as objects.
    if accepted_val.first().is_some_and(Value::is_object) {
        let mut out = Vec::with_capacity(accepted_val.len());
        for item in accepted_val {
            out.push(serde_json::from_value::<DistillHunk>(item.clone())?);
        }
        return Ok(out);
    }

    // 3. `acceptedHunks` passed as string IDs, resolved against `memoryHunks`.
    let accepted_ids: Vec<&str> = accepted_val.iter().filter_map(Value::as_str).collect();
    let pool = payload
        .get("memoryHunks")
        .or_else(|| payload.get("memory_hunks"))
        .and_then(Value::as_array)
        .or_else(|| {
            fallback_widget_data.and_then(|d| {
                d.get("memoryHunks")
                    .or_else(|| d.get("memory_hunks"))
                    .and_then(Value::as_array)
            })
        });

    let Some(pool) = pool else {
        bail!("distill_diff payload is missing memoryHunks definitions");
    };

    let mut out = Vec::new();
    for (idx, item) in pool.iter().enumerate() {
        let mut hunk: DistillHunk = serde_json::from_value(item.clone())?;
        if hunk.id.is_empty() {
            hunk.id = format!("hunk-{idx}");
        }
        if accepted_ids.iter().any(|&id| id == hunk.id) {
            out.push(hunk);
        }
    }
    Ok(out)
}

fn resolve_skill_draft(
    payload: &Value,
    fallback_widget_data: Option<&Value>,
) -> Option<SkillDraftInput> {
    let raw = payload
        .get("skillDraft")
        .or_else(|| payload.get("skill_draft"))
        .filter(|v| !v.is_null())
        .or_else(|| {
            fallback_widget_data.and_then(|d| {
                d.get("skillDraft")
                    .or_else(|| d.get("skill_draft"))
                    .filter(|v| !v.is_null())
            })
        })?;
    serde_json::from_value::<SkillDraftInput>(raw.clone()).ok()
}

/* --------------------------------------------------------------------------
Philips Hue (`openhue` CLI) Execution + Safe Fallback Prompt
-------------------------------------------------------------------------- */

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HueCommandPlan {
    pub commands: Vec<Vec<String>>,
    pub summary: String,
    pub fallback_prompt: String,
}

fn clean_cli_arg<'a>(val: Option<&'a str>, label: &str) -> Result<&'a str> {
    let s = val.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("");
    if s.is_empty() {
        bail!("missing {label}");
    }
    if s.starts_with('-') {
        bail!("invalid {label} `{s}`");
    }
    Ok(s)
}

pub fn plan_openhue_commands(action: &str, payload: Option<&Value>) -> Result<HueCommandPlan> {
    let empty = Value::Null;
    let p = payload.unwrap_or(&empty);
    let room_opt = p
        .get("room")
        .or_else(|| p.get("roomName"))
        .or_else(|| p.get("roomId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    match action {
        "toggle_room" => {
            let room = clean_cli_arg(room_opt, "room")?;
            let on = p.get("on").and_then(Value::as_bool).unwrap_or(true);
            let brightness = p.get("brightness").and_then(Value::as_u64);
            let cmd = if on {
                if let Some(b) = brightness.filter(|&b| b > 0) {
                    let clamped = b.clamp(1, 100);
                    vec![
                        "openhue".to_string(),
                        "set".to_string(),
                        "room".to_string(),
                        room.to_string(),
                        "--on".to_string(),
                        "--brightness".to_string(),
                        clamped.to_string(),
                    ]
                } else {
                    vec![
                        "openhue".to_string(),
                        "set".to_string(),
                        "room".to_string(),
                        room.to_string(),
                        "--on".to_string(),
                    ]
                }
            } else {
                vec![
                    "openhue".to_string(),
                    "set".to_string(),
                    "room".to_string(),
                    room.to_string(),
                    "--off".to_string(),
                ]
            };
            let state_word = if on { "on" } else { "off" };
            Ok(HueCommandPlan {
                commands: vec![cmd],
                summary: format!("Hue · {room} {state_word}"),
                fallback_prompt: format!("Turn the \"{room}\" Hue lights {state_word}."),
            })
        }
        "set_brightness" => {
            let room = clean_cli_arg(room_opt, "room")?;
            let brightness = p
                .get("brightness")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(100);
            let on = p
                .get("on")
                .and_then(Value::as_bool)
                .unwrap_or(brightness > 0);
            let cmd = if !on || brightness == 0 {
                vec![
                    "openhue".to_string(),
                    "set".to_string(),
                    "room".to_string(),
                    room.to_string(),
                    "--off".to_string(),
                ]
            } else {
                vec![
                    "openhue".to_string(),
                    "set".to_string(),
                    "room".to_string(),
                    room.to_string(),
                    "--on".to_string(),
                    "--brightness".to_string(),
                    brightness.clamp(1, 100).to_string(),
                ]
            };
            let summary = if !on || brightness == 0 {
                format!("Hue · {room} off")
            } else {
                format!("Hue · {room} {brightness}%")
            };
            let fallback_prompt = if !on || brightness == 0 {
                format!("Turn off the \"{room}\" Hue room lights.")
            } else {
                format!("Set \"{room}\" Hue room brightness to {brightness}%.")
            };
            Ok(HueCommandPlan {
                commands: vec![cmd],
                summary,
                fallback_prompt,
            })
        }
        "set_temp" => {
            let room = clean_cli_arg(room_opt, "room")?;
            let mirek = p
                .get("tempMirek")
                .or_else(|| p.get("temp_mirek"))
                .and_then(Value::as_u64)
                .unwrap_or(320)
                .clamp(153, 500);
            let cmd = vec![
                "openhue".to_string(),
                "set".to_string(),
                "room".to_string(),
                room.to_string(),
                "--on".to_string(),
                "-t".to_string(),
                mirek.to_string(),
            ];
            Ok(HueCommandPlan {
                commands: vec![cmd],
                summary: format!("Hue · {room} {mirek} mirek"),
                fallback_prompt: format!(
                    "Set \"{room}\" Hue room color temperature to {mirek} mirek."
                ),
            })
        }
        "activate_scene" => {
            let scene = clean_cli_arg(p.get("scene").and_then(Value::as_str), "scene")?;
            if scene.eq_ignore_ascii_case("all off") {
                let rooms: Vec<&str> = p
                    .get("rooms")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(str::trim)
                            .filter(|r| !r.is_empty() && !r.starts_with('-'))
                            .collect()
                    })
                    .unwrap_or_default();
                if !rooms.is_empty() {
                    let cmds = rooms
                        .iter()
                        .map(|r| {
                            vec![
                                "openhue".to_string(),
                                "set".to_string(),
                                "room".to_string(),
                                (*r).to_string(),
                                "--off".to_string(),
                            ]
                        })
                        .collect();
                    return Ok(HueCommandPlan {
                        commands: cmds,
                        summary: "Hue · All rooms off".to_string(),
                        fallback_prompt: "Turn off all Hue room lights.".to_string(),
                    });
                }
            }

            let cmd = if let Some(room) = room_opt.filter(|r| !r.starts_with('-')) {
                vec![
                    "openhue".to_string(),
                    "set".to_string(),
                    "scene".to_string(),
                    "--room".to_string(),
                    room.to_string(),
                    scene.to_string(),
                ]
            } else {
                vec![
                    "openhue".to_string(),
                    "set".to_string(),
                    "scene".to_string(),
                    scene.to_string(),
                ]
            };
            Ok(HueCommandPlan {
                commands: vec![cmd],
                summary: format!("Hue scene · {scene}"),
                fallback_prompt: format!("Activate the \"{scene}\" Philips Hue lighting scene."),
            })
        }
        other => bail!("unsupported hue_rooms action `{other}`"),
    }
}

pub fn run_openhue_plan(plan: &HueCommandPlan, timeout: Duration) -> Result<()> {
    for argv in &plan.commands {
        let Some((prog, args)) = argv.split_first() else {
            continue;
        };
        let mut child = Command::new(prog)
            .args(args)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning `{prog}`"))?;

        let pid = child.id() as i32;
        let stderr_pipe = child.stderr.take();
        let err_handle = std::thread::spawn(move || read_bounded_pipe(stderr_pipe, 4096));
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break st,
                Ok(None) => {
                    if started.elapsed() >= timeout {
                        if pid > 0 {
                            unsafe {
                                libc::kill(-pid, libc::SIGKILL);
                            }
                        }
                        let _ = child.kill();
                        let _ = child.wait();
                        bail!("`{prog}` timed out");
                    }
                    std::thread::sleep(Duration::from_millis(15));
                }
                Err(e) => return Err(e).with_context(|| format!("waiting for `{prog}`")),
            }
        };
        let stderr = err_handle.join().unwrap_or_default();
        if !status.success() {
            let detail = stderr.trim();
            if detail.is_empty() {
                bail!("`{prog}` exited with {status}");
            }
            bail!("`{prog}` failed: {detail}");
        }
    }
    Ok(())
}

/* --------------------------------------------------------------------------
Atomic Filesystem Helpers
-------------------------------------------------------------------------- */

fn ensure_trailing_newline(s: &str) -> String {
    if s.is_empty() {
        String::new()
    } else if s.ends_with('\n') {
        s.to_string()
    } else {
        format!("{s}\n")
    }
}

fn ensure_private_dir_0700(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restricting {}", dir.display()))?;
    Ok(())
}

fn atomic_write_private_0600(target: &Path, bytes: &[u8]) -> Result<()> {
    atomic_write_preserving_mode(target, bytes, Some(0o600))
}

fn atomic_write_preserving_mode(
    target: &Path,
    bytes: &[u8],
    forced_mode: Option<u32>,
) -> Result<()> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{file_name}.hc-{}-{seq}.tmp", std::process::id()));

    let existing_mode = forced_mode.or_else(|| {
        std::fs::metadata(target)
            .ok()
            .map(|m| m.permissions().mode() & 0o777)
    });
    let create_mode = existing_mode.unwrap_or(0o600);

    let write_res = (|| -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(create_mode)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        if let Some(m) = forced_mode.or(existing_mode) {
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(m))
                .with_context(|| format!("setting permissions on {}", tmp.display()))?;
        }
        f.write_all(bytes)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.flush()
            .with_context(|| format!("flushing {}", tmp.display()))?;
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }

    if let Err(err) = std::fs::rename(&tmp, target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("replacing {}", target.display()));
    }
    Ok(())
}
