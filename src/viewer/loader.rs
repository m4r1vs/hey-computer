//! Startup conversation resolution, `answer-<ms>-<id>.md` filename parsing,
//! Morning Brief drop-cap detection, FTS5 recall history mapping, and
//! Scheduled Actions overview/inspector loading.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use hey_computer::protocol::WidgetSpec;
use hey_computer::render::render_markdown;
use hey_computer::scheduler::{humanize_schedule_name, humanize_schedule_spec};
use hey_computer::store::{
    self, ListFilter, RecallHit, RecallOptions, RecallSort, ScheduleRecord, TurnOrigin, TurnRecord,
    TurnStatus,
};
use serde::{Deserialize, Serialize};

const MAX_THREAD_TURNS: usize = 50;
const MAX_SCHEDULE_RECENT_RUNS: usize = 15;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewerTurn {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<u64>,
    pub prompt: String,
    #[serde(default)]
    pub hide_prompt: bool,
    pub badge: String,
    pub profile: String,
    pub lane: String,
    pub origin: String,
    #[serde(default = "default_turn_status")]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppressed_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_name: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub reading_time: String,
    pub word_count: usize,
    #[serde(default)]
    pub denied: Vec<String>,
    #[serde(default)]
    pub widgets: Vec<WidgetSpec>,
    pub markdown: String,
    pub html: String,
    #[serde(default)]
    pub is_morning_brief: bool,
    #[serde(default)]
    pub drop_cap_hint: bool,
}

fn default_turn_status() -> String {
    "completed".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewerThread {
    pub id: String,
    pub title: String,
    pub badge: String,
    pub created_at: String,
    #[serde(default)]
    pub drop_cap_hint: bool,
    #[serde(default)]
    pub turns: Vec<ViewerTurn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: String,
    pub turn_id: u64,
    pub title: String,
    pub request: String,
    pub prompt: String,
    pub snippet: String,
    pub answer_snippet: String,
    pub badge: String,
    pub profile: String,
    pub lane: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewerScheduleSummary {
    pub id: u64,
    pub name: String,
    pub title: String,
    pub enabled: bool,
    pub managed: bool,
    pub running: bool,
    pub status: String,
    pub mode: String,
    pub schedule_spec: String,
    pub human_schedule: String,
    pub profile: String,
    pub run_count: u64,
    pub consecutive_failures: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_run_local: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_local: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_turn_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewerScheduleRun {
    pub turn_id: u64,
    pub status: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub headline: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suppressed_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fail_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub widget_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewerScheduleDetail {
    pub summary: ViewerScheduleSummary,
    pub prompt: String,
    pub script: String,
    pub no_agent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suppress_token: Option<String>,
    pub failure_deliver: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_runs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at_local: Option<String>,
    pub recent_runs: Vec<ViewerScheduleRun>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_turn: Option<ViewerTurn>,
}

/// Extract `<id>` from answer filenames generated by `present.rs`
/// (`answer-<timestamp_ms>-<id>.md`).
pub fn parse_answer_filename_turn_id(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let inner = name.strip_prefix("answer-")?.strip_suffix(".md")?;
    let (ms_part, id_part) = inner.split_once('-')?;
    if ms_part.is_empty()
        || id_part.is_empty()
        || !ms_part.bytes().all(|b| b.is_ascii_digit())
        || !id_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let _ms: u64 = ms_part.parse().ok()?;
    let id: u64 = id_part.parse().ok()?;
    (id > 0).then_some(id)
}

/// Parse a numeric turn target (`"42"` or `"#42"`) when the positional `file`
/// argument does not exist on disk.
pub fn parse_numeric_turn_arg(path: &Path) -> Option<u64> {
    if path.exists() {
        return None;
    }
    let raw = path.to_str()?.trim();
    let digits = raw.strip_prefix('#').unwrap_or(raw);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let id: u64 = digits.parse().ok()?;
    (id > 0).then_some(id)
}

/// Detect whether an opening turn is a Morning Brief (scheduled job turn or a
/// request/title naming `"morning brief"` / `"morning-digest"`), which receives
/// an automatic editorial drop cap regardless of the global `D` toggle.
pub fn is_morning_brief_turn(
    origin: TurnOrigin,
    schedule_id: Option<u64>,
    request: &str,
    title: &str,
) -> bool {
    if matches!(origin, TurnOrigin::Schedule | TurnOrigin::ScheduleScript) || schedule_id.is_some()
    {
        return true;
    }
    for text in [request, title] {
        let lower = text.to_ascii_lowercase();
        if lower.contains("morning brief")
            || lower.contains("morning-brief")
            || lower.contains("morning digest")
            || lower.contains("morning-digest")
        {
            return true;
        }
    }
    false
}

pub fn extract_markdown_title(md: &str) -> Option<String> {
    md.lines()
        .map(str::trim)
        .find(|l| l.starts_with('#'))
        .map(|l| l.trim_start_matches('#').trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn headline(text: &str, max_chars: usize) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("Ask Computer");
    let clean = first.trim_start_matches('#').trim();
    if clean.is_empty() {
        return "Ask Computer".to_string();
    }
    match clean.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}…", clean[..idx].trim_end()),
        None => clean.to_string(),
    }
}

fn schedule_name_map(state_dir: Option<&Path>) -> HashMap<u64, String> {
    let Some(dir) = state_dir else {
        return HashMap::new();
    };
    match store::list_schedules(dir, true) {
        Ok(jobs) => jobs.into_iter().map(|j| (j.id, j.name)).collect(),
        Err(_) => HashMap::new(),
    }
}

/// Whether the viewer should hide `.turn-prompt-box` for `turn` (scheduled/repeating
/// runs and system-generated notification root prompts, while keeping interactive
/// user follow-up turns visible).
pub fn should_hide_turn_prompt(turn: &TurnRecord, is_opening: bool) -> bool {
    if turn.schedule_id.is_some()
        || matches!(
            turn.origin,
            TurnOrigin::Schedule | TurnOrigin::ScheduleScript
        )
    {
        return true;
    }
    (is_opening || turn.lane == store::Lane::Restricted)
        && hey_computer::notify_filter::is_notification_enrichment_prompt(&turn.request)
}

pub fn turn_record_to_viewer_turn(
    turn: TurnRecord,
    is_opening: bool,
    thread_title: &str,
    drop_cap_flag: bool,
    schedule_name: Option<String>,
) -> ViewerTurn {
    let hide_prompt = should_hide_turn_prompt(&turn, is_opening);
    let word_count = turn.answer.split_whitespace().count();
    let reading_time = if word_count > 0 {
        format!("{} min read", word_count.div_ceil(210).max(1))
    } else {
        String::new()
    };
    let duration_ms = match (turn.started_at_ms, turn.finished_at_ms) {
        (Some(start), Some(end)) if end >= start => Some((end - start) as u64),
        _ => None,
    };
    let badge = if turn.profile.trim().is_empty() {
        "default".to_string()
    } else {
        turn.profile.clone()
    };
    let morning_brief = is_opening
        && is_morning_brief_turn(turn.origin, turn.schedule_id, &turn.request, thread_title);
    let html = render_markdown(&turn.answer);
    let created_at = store::format_local_timestamp(turn.created_at_ms);

    ViewerTurn {
        id: turn.id.to_string(),
        turn_id: Some(turn.id),
        conversation_id: Some(turn.conversation_id.unwrap_or(turn.id)),
        prompt: turn.request,
        hide_prompt,
        badge,
        profile: turn.profile,
        lane: turn.lane.as_str().to_string(),
        origin: turn.origin.as_str().to_string(),
        status: turn.status.as_str().to_string(),
        error_message: turn.error_message,
        fail_reason: turn.fail_reason,
        suppressed_token: turn.suppressed_token,
        schedule_id: turn.schedule_id,
        schedule_name,
        created_at,
        duration_ms,
        reading_time,
        word_count,
        denied: turn.denied,
        widgets: turn.widgets,
        markdown: turn.answer,
        html,
        is_morning_brief: morning_brief,
        drop_cap_hint: drop_cap_flag,
    }
}

fn build_thread_from_turns(
    turns: Vec<TurnRecord>,
    drop_cap_flag: bool,
    sched_names: &HashMap<u64, String>,
) -> (ViewerThread, u64) {
    let first = &turns[0];
    let last_id = turns.last().map(|t| t.id).unwrap_or(first.id);
    let root_id = first.conversation_id.unwrap_or(first.id);
    let first_sched = first.schedule_id.and_then(|sid| sched_names.get(&sid));
    let title = extract_markdown_title(&first.answer)
        .or_else(|| {
            if hey_computer::notify_filter::is_notification_enrichment_prompt(&first.request) {
                hey_computer::notify_filter::format_notification_title(&first.request, 68)
            } else {
                None
            }
        })
        .or_else(|| first_sched.map(|s| humanize_schedule_name(s)))
        .unwrap_or_else(|| headline(&first.request, 68));
    let badge = if first.profile.trim().is_empty() {
        "default".to_string()
    } else {
        first.profile.clone()
    };
    let created_at = store::format_local_timestamp(first.created_at_ms);

    let viewer_turns: Vec<ViewerTurn> = turns
        .into_iter()
        .enumerate()
        .map(|(idx, t)| {
            let sname = t.schedule_id.and_then(|sid| sched_names.get(&sid).cloned());
            turn_record_to_viewer_turn(t, idx == 0, &title, drop_cap_flag, sname)
        })
        .collect();

    (
        ViewerThread {
            id: format!("thread-{root_id}"),
            title,
            badge,
            created_at,
            drop_cap_hint: drop_cap_flag,
            turns: viewer_turns,
        },
        last_id,
    )
}

/// Load the conversation thread for `turn_id` from `<state_dir>/history.db`.
pub fn load_thread_for_turn(
    state_dir: &Path,
    turn_id: u64,
    drop_cap_flag: bool,
) -> Result<Option<(ViewerThread, u64)>> {
    if turn_id == 0 {
        return Ok(None);
    }
    let mut turns = store::conversation_turns(state_dir, turn_id, true, MAX_THREAD_TURNS)?;
    if turns.is_empty()
        && let Some(single) = store::get_turn(state_dir, turn_id)?
    {
        turns.push(single);
    }
    if turns.is_empty() {
        return Ok(None);
    }
    let sched_names = schedule_name_map(Some(state_dir));
    Ok(Some(build_thread_from_turns(
        turns,
        drop_cap_flag,
        &sched_names,
    )))
}

fn build_thread_from_file(md_path: &Path, drop_cap_flag: bool) -> Result<ViewerThread> {
    let raw = std::fs::read_to_string(md_path)
        .with_context(|| format!("reading markdown file {}", md_path.display()))?;
    let file_name = md_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Document.md");
    let title = extract_markdown_title(&raw).unwrap_or_else(|| file_name.to_string());
    let word_count = raw.split_whitespace().count();
    let reading_time = if word_count > 0 {
        format!("{} min read", word_count.div_ceil(210).max(1))
    } else {
        String::new()
    };
    let morning_brief = is_morning_brief_turn(TurnOrigin::Cli, None, "", &title);
    let html = render_markdown(&raw);

    let turn = ViewerTurn {
        id: "file-1".to_string(),
        turn_id: None,
        conversation_id: None,
        prompt: String::new(),
        hide_prompt: false,
        badge: "file".to_string(),
        profile: String::new(),
        lane: "interactive".to_string(),
        origin: "cli".to_string(),
        status: "completed".to_string(),
        error_message: None,
        fail_reason: None,
        suppressed_token: None,
        schedule_id: None,
        schedule_name: None,
        created_at: file_name.to_string(),
        duration_ms: None,
        reading_time,
        word_count,
        denied: Vec::new(),
        widgets: Vec::new(),
        markdown: raw,
        html,
        is_morning_brief: morning_brief,
        drop_cap_hint: drop_cap_flag,
    };

    Ok(ViewerThread {
        id: "file".to_string(),
        title,
        badge: "file".to_string(),
        created_at: file_name.to_string(),
        drop_cap_hint: drop_cap_flag,
        turns: vec![turn],
    })
}

pub fn empty_thread(drop_cap_flag: bool) -> ViewerThread {
    ViewerThread {
        id: "empty".to_string(),
        title: "Ask Computer".to_string(),
        badge: "default".to_string(),
        created_at: store::local_today_ymd(),
        drop_cap_hint: drop_cap_flag,
        turns: Vec::new(),
    }
}

/// Resolve startup thread according to CLI priority:
/// 1. `--turn-id <ID>` (`ID > 0`) -> DB conversation thread (fallback to `[FILE]` or empty).
/// 2. `[FILE]` -> if numeric (`"42"` / `"#42"`) and `!path.exists()`, treat as turn ID;
///    if `answer-<ms>-<id>.md`, try DB turn `<id>` first; else render Markdown file.
/// 3. Latest completed turn's thread in `history.db`, or clean empty `"Ask Computer"` view.
pub fn resolve_initial_thread(
    state_dir: Option<&Path>,
    turn_id: Option<u64>,
    file: Option<&Path>,
    drop_cap_flag: bool,
) -> Result<(ViewerThread, Option<u64>)> {
    let effective_turn_id = turn_id
        .filter(|&id| id > 0)
        .or_else(|| file.and_then(parse_numeric_turn_arg));
    let file_is_numeric_id = turn_id.is_none() && file.and_then(parse_numeric_turn_arg).is_some();

    if let Some(id) = effective_turn_id {
        if let Some(dir) = state_dir {
            match load_thread_for_turn(dir, id, drop_cap_flag) {
                Ok(Some((thread, latest))) => return Ok((thread, Some(latest))),
                Ok(None) => {}
                Err(err) => tracing::warn!("cannot load turn {id} from history.db: {err:#}"),
            }
        }
        if !file_is_numeric_id
            && let Some(md_path) = file
            && md_path.exists()
        {
            return Ok((build_thread_from_file(md_path, drop_cap_flag)?, None));
        }
        return Ok((empty_thread(drop_cap_flag), None));
    }

    if let Some(md_path) = file {
        if let Some(parsed_id) = parse_answer_filename_turn_id(md_path)
            && let Some(dir) = state_dir
        {
            match load_thread_for_turn(dir, parsed_id, drop_cap_flag) {
                Ok(Some((thread, latest))) => return Ok((thread, Some(latest))),
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!("cannot load turn {parsed_id} from history.db: {err:#}");
                }
            }
        }
        return Ok((build_thread_from_file(md_path, drop_cap_flag)?, None));
    }

    if let Some(dir) = state_dir {
        let filter = ListFilter {
            lane: None,
            status: Some(TurnStatus::Completed),
            include_suppressed: false,
            limit: 20,
        };
        match store::list_turns(dir, &filter) {
            Ok(turns) => {
                if let Some(latest_turn) = turns
                    .into_iter()
                    .find(|t| !t.answer.trim().is_empty() || !t.widgets.is_empty())
                {
                    if let Ok(Some((thread, latest_id))) =
                        load_thread_for_turn(dir, latest_turn.id, drop_cap_flag)
                    {
                        return Ok((thread, Some(latest_id)));
                    }
                    let sched_names = schedule_name_map(Some(dir));
                    let (thread, latest_id) =
                        build_thread_from_turns(vec![latest_turn], drop_cap_flag, &sched_names);
                    return Ok((thread, Some(latest_id)));
                }
            }
            Err(err) => tracing::warn!("cannot list recent turns from history.db: {err:#}"),
        }

        // Legacy `last.json` fallback when `history.db` does not exist yet.
        if let Ok(Some(legacy)) = store::load_last_for_lane(dir, None) {
            let ts_ms = (legacy.timestamp as i64).saturating_mul(1000);
            let rec = TurnRecord {
                id: legacy.id,
                lane: store::Lane::Interactive,
                status: TurnStatus::Completed,
                profile: legacy.profile,
                session_id: None,
                conversation_id: legacy.conversation_id,
                created_at_ms: ts_ms,
                started_at_ms: None,
                finished_at_ms: Some(ts_ms),
                request: legacy.request,
                answer: legacy.text,
                denied: legacy.denied,
                attachments: legacy.attachments,
                fail_reason: None,
                error_message: None,
                suppressed_token: None,
                widgets: legacy.widgets,
                origin: TurnOrigin::Cli,
                schedule_id: None,
            };
            let latest_id = (rec.id > 0).then_some(rec.id);
            let (thread, _) = build_thread_from_turns(vec![rec], drop_cap_flag, &HashMap::new());
            return Ok((thread, latest_id));
        }
    }

    Ok((empty_thread(drop_cap_flag), None))
}

/// Query `history.db` via `hey_computer::store::recall` (BM25 + recency ranking
/// with local timestamps).
pub fn query_history_entries(
    state_dir: Option<&Path>,
    query: &str,
    limit: usize,
) -> Vec<HistoryEntry> {
    let Some(dir) = state_dir else {
        return Vec::new();
    };
    let trimmed = query.trim();
    let has_query = !trimmed.is_empty();
    let opts = RecallOptions {
        query: has_query.then(|| trimmed.to_string()),
        lane: None,
        since_ms: None,
        until_ms: None,
        limit: limit.clamp(1, 50),
        sort: if has_query {
            RecallSort::Relevance
        } else {
            RecallSort::Recent
        },
        match_any: false,
        raw_fts: false,
        user_only: false,
    };
    let sched_names = schedule_name_map(Some(dir));
    match store::recall(dir, &opts) {
        Ok(hits) => hits
            .into_iter()
            .map(|h| recall_hit_to_entry(h, &sched_names))
            .collect(),
        Err(err) => {
            tracing::warn!("history recall query failed: {err:#}");
            Vec::new()
        }
    }
}

fn recall_hit_to_entry(hit: RecallHit, sched_names: &HashMap<u64, String>) -> HistoryEntry {
    let badge = if hit.profile.trim().is_empty() {
        hit.lane.as_str().to_string()
    } else {
        hit.profile.clone()
    };
    let is_notif = hey_computer::notify_filter::is_notification_enrichment_prompt(&hit.request);
    let sched_title = hit
        .schedule_id
        .and_then(|sid| sched_names.get(&sid))
        .map(|s| humanize_schedule_name(s));
    let title = extract_markdown_title(&hit.answer)
        .or_else(|| {
            if is_notif {
                hey_computer::notify_filter::format_notification_title(&hit.request, 68)
            } else {
                None
            }
        })
        .or(sched_title)
        .unwrap_or_else(|| headline(&hit.request, 68));
    let clean_prompt = if is_notif {
        hey_computer::notify_filter::format_notification_preview(&hit.request, 140)
            .unwrap_or_else(|| title.clone())
    } else if hit.schedule_id.is_some()
        || matches!(
            hit.origin,
            TurnOrigin::Schedule | TurnOrigin::ScheduleScript
        )
    {
        String::new()
    } else {
        hit.request.clone()
    };
    let snippet: String = hit
        .ans_snippet
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    HistoryEntry {
        id: format!("turn-{}", hit.id),
        turn_id: hit.id,
        title,
        prompt: clean_prompt.clone(),
        request: clean_prompt,
        answer_snippet: snippet.clone(),
        snippet,
        badge,
        profile: hit.profile,
        lane: hit.lane.as_str().to_string(),
        created_at: hit.created_local,
    }
}

/* --------------------------------------------------------------------------
Scheduled Actions Loading (Overview + Inspector)
-------------------------------------------------------------------------- */

fn schedule_mode(job: &ScheduleRecord) -> &'static str {
    if job.script.contains("distill") || job.name.contains("distill") {
        "distill"
    } else if job.no_agent {
        "no-agent"
    } else if !job.script.trim().is_empty() {
        "collector + agent"
    } else {
        "agent"
    }
}

fn schedule_status(job: &ScheduleRecord, running: bool) -> &'static str {
    if running {
        "running"
    } else if !job.enabled {
        "paused"
    } else if job.consecutive_failures > 0 {
        "failing"
    } else {
        "active"
    }
}

fn build_schedule_summary(
    job: &ScheduleRecord,
    running: bool,
    latest_turn_id: Option<u64>,
) -> ViewerScheduleSummary {
    ViewerScheduleSummary {
        id: job.id,
        name: job.name.clone(),
        title: humanize_schedule_name(&job.name),
        enabled: job.enabled,
        managed: job.managed,
        running,
        status: schedule_status(job, running).to_string(),
        mode: schedule_mode(job).to_string(),
        schedule_spec: job.schedule_spec.clone(),
        human_schedule: humanize_schedule_spec(&job.schedule_spec),
        profile: job.profile.clone(),
        run_count: job.run_count,
        consecutive_failures: job.consecutive_failures,
        next_run_local: job.next_run_at.map(store::format_local_timestamp),
        last_run_local: job.last_run_at.map(store::format_local_timestamp),
        last_status: job.last_status.clone(),
        last_error: job.last_error.clone(),
        latest_turn_id,
    }
}

pub fn load_schedule_summaries(
    state_dir: Option<&Path>,
    running_ids: &HashSet<u64>,
) -> Vec<ViewerScheduleSummary> {
    let Some(dir) = state_dir else {
        return Vec::new();
    };
    let jobs = match store::list_schedules(dir, true) {
        Ok(jobs) => jobs,
        Err(err) => {
            tracing::warn!("cannot list schedules from history.db: {err:#}");
            return Vec::new();
        }
    };
    jobs.into_iter()
        .map(|job| {
            let latest_turn_id = store::list_turns_for_schedule_job(dir, &job, 5)
                .ok()
                .and_then(|turns| {
                    turns
                        .iter()
                        .find(|t| {
                            t.status == TurnStatus::Completed
                                && (!t.answer.trim().is_empty() || !t.widgets.is_empty())
                        })
                        .or_else(|| turns.first())
                        .map(|t| t.id)
                });
            let running = running_ids.contains(&job.id);
            build_schedule_summary(&job, running, latest_turn_id)
        })
        .collect()
}

pub fn load_schedule_detail(
    state_dir: Option<&Path>,
    target: &str,
    running_ids: &HashSet<u64>,
    drop_cap_flag: bool,
) -> Result<Option<ViewerScheduleDetail>> {
    let Some(dir) = state_dir else {
        return Ok(None);
    };
    let Some(job) = store::get_schedule(dir, target)? else {
        return Ok(None);
    };

    let turns = store::list_turns_for_schedule_job(dir, &job, MAX_SCHEDULE_RECENT_RUNS)?;
    let latest_completed = turns
        .iter()
        .find(|t| {
            t.status == TurnStatus::Completed
                && (!t.answer.trim().is_empty() || !t.widgets.is_empty())
        })
        .or_else(|| turns.first())
        .cloned();

    let latest_turn_id = latest_completed.as_ref().map(|t| t.id);
    let running = running_ids.contains(&job.id);
    let summary = build_schedule_summary(&job, running, latest_turn_id);

    let recent_runs: Vec<ViewerScheduleRun> = turns
        .iter()
        .map(|t| {
            let duration_ms = match (t.started_at_ms, t.finished_at_ms) {
                (Some(start), Some(end)) if end >= start => Some((end - start) as u64),
                _ => None,
            };
            let run_headline = extract_markdown_title(&t.answer).unwrap_or_else(|| {
                if !t.answer.trim().is_empty() {
                    headline(&t.answer, 80)
                } else {
                    headline(&t.request, 80)
                }
            });
            ViewerScheduleRun {
                turn_id: t.id,
                status: t.status.as_str().to_string(),
                created_at: store::format_local_timestamp(t.created_at_ms),
                duration_ms,
                headline: run_headline,
                suppressed_token: t.suppressed_token.clone(),
                fail_reason: t.fail_reason.clone(),
                error_message: t.error_message.clone(),
                widget_count: t.widgets.len(),
            }
        })
        .collect();

    let latest_turn = latest_completed.map(|t| {
        let title = summary.title.clone();
        turn_record_to_viewer_turn(t, true, &title, drop_cap_flag, Some(job.name.clone()))
    });

    Ok(Some(ViewerScheduleDetail {
        summary,
        prompt: job.prompt,
        script: job.script,
        no_agent: job.no_agent,
        suppress_token: job.suppress_token,
        failure_deliver: job.failure_deliver.as_str().to_string(),
        max_runs: job.max_runs,
        expires_at_local: job.expires_at.map(store::format_local_timestamp),
        recent_runs,
        latest_turn,
    }))
}

/// Scan `<state_dir>/proactive/day-plan-YYYY-MM-DD.json` and return sorted
/// unique `YYYY-MM-DD` dates for already-activated day plans so reopened
/// viewer threads remember `Plan Activated ✓` across restarts.
pub fn load_activated_day_plan_dates(state_dir: Option<&Path>) -> Vec<String> {
    let Some(dir) = state_dir else {
        return Vec::new();
    };
    let proactive_dir = dir.join("proactive");
    let Ok(entries) = std::fs::read_dir(&proactive_dir) else {
        return Vec::new();
    };
    let mut dates = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().into_string().ok() else {
            continue;
        };
        let Some(date) = name
            .strip_prefix("day-plan-")
            .and_then(|rest| rest.strip_suffix(".json"))
        else {
            continue;
        };
        if is_valid_ymd(date) {
            dates.push(date.to_string());
        }
    }
    dates.sort();
    dates.dedup();
    dates
}

fn is_valid_ymd(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[0..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..10].iter().all(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hey_computer::store::FailureDeliver;

    fn sample_schedule(name: &str, script: &str, no_agent: bool) -> ScheduleRecord {
        ScheduleRecord {
            id: 1,
            name: name.to_string(),
            enabled: true,
            managed: true,
            schedule_spec: "0 18 * * 0".to_string(),
            prompt: String::new(),
            profile: String::new(),
            script: script.to_string(),
            no_agent,
            suppress_token: None,
            failure_deliver: FailureDeliver::Notify,
            max_runs: None,
            expires_at: None,
            run_count: 0,
            consecutive_failures: 0,
            next_run_at: None,
            last_run_at: None,
            last_status: None,
            last_error: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn schedule_mode_prioritizes_distill_over_no_agent() {
        let weekly_distill = sample_schedule(
            "weekly-distill",
            "hey-computer distill --since 7d --background >/dev/null",
            true,
        );
        assert_eq!(schedule_mode(&weekly_distill), "distill");

        let script_distill = sample_schedule("cleanup", "hey-computer distill --since 3d", true);
        assert_eq!(schedule_mode(&script_distill), "distill");

        let plain_no_agent = sample_schedule("reminder", "echo hi", true);
        assert_eq!(schedule_mode(&plain_no_agent), "no-agent");

        let collector_agent = sample_schedule("morning-digest", "collect_morning_digest", false);
        assert_eq!(schedule_mode(&collector_agent), "collector + agent");

        let agent_only = sample_schedule("pre-daily", "", false);
        assert_eq!(schedule_mode(&agent_only), "agent");
    }

    #[test]
    fn hides_scheduled_and_notification_system_prompts_while_keeping_user_followups() {
        let sched_turn = TurnRecord {
            id: 10,
            lane: store::Lane::Background,
            status: TurnStatus::Completed,
            profile: "deep".to_string(),
            session_id: None,
            conversation_id: Some(10),
            created_at_ms: 1_700_000_000_000,
            started_at_ms: Some(1_700_000_000_000),
            finished_at_ms: Some(1_700_000_005_000),
            request: "Prepare today's Morning Brief...\n\n<untrusted_collector_output>\ndate: 2026-11-26\n</untrusted_collector_output>".to_string(),
            answer: "- Standup at 10:00".to_string(),
            denied: vec![],
            attachments: vec![],
            fail_reason: None,
            error_message: None,
            suppressed_token: None,
            widgets: vec![],
            origin: TurnOrigin::ScheduleScript,
            schedule_id: Some(2),
        };
        let mut sched_names = HashMap::new();
        sched_names.insert(2, "pre-daily".to_string());
        let (sched_thread, _) = build_thread_from_turns(vec![sched_turn], true, &sched_names);
        assert_eq!(sched_thread.title, "Pre-Daily Brief");
        assert!(sched_thread.turns[0].hide_prompt);

        // Legacy (`origin = Cli`, `lane = Restricted`) notification root + interactive user follow-up:
        let notif_prompt = "<notification_task>\nYou are helping...\n</notification_task>\n\n\
            <untrusted_notification>\nchannel: whatsapp\napp: whatsapp-electron\nsender: alice\n\
            preview: Are we meeting this weekend at the harbor?\n</untrusted_notification>";
        let root_notif = TurnRecord {
            id: 20,
            lane: store::Lane::Restricted,
            status: TurnStatus::Completed,
            profile: "notify-enrich".to_string(),
            session_id: None,
            conversation_id: Some(20),
            created_at_ms: 1_700_000_010_000,
            started_at_ms: Some(1_700_000_010_000),
            finished_at_ms: Some(1_700_000_012_000),
            request: notif_prompt.to_string(),
            answer: "CONTEXT: Saturday free\nDRAFT: Saturday afternoon works!".to_string(),
            denied: vec![],
            attachments: vec![],
            fail_reason: None,
            error_message: None,
            suppressed_token: None,
            widgets: vec![],
            origin: TurnOrigin::Cli,
            schedule_id: None,
        };
        let followup = TurnRecord {
            id: 21,
            lane: store::Lane::Restricted,
            status: TurnStatus::Completed,
            profile: "notify-enrich".to_string(),
            session_id: None,
            conversation_id: Some(20),
            created_at_ms: 1_700_000_020_000,
            started_at_ms: Some(1_700_000_020_000),
            finished_at_ms: Some(1_700_000_021_000),
            request: "Make the reply a bit more casual".to_string(),
            answer: "DRAFT: Saturday works great — see you there!".to_string(),
            denied: vec![],
            attachments: vec![],
            fail_reason: None,
            error_message: None,
            suppressed_token: None,
            widgets: vec![],
            origin: TurnOrigin::Typed,
            schedule_id: None,
        };
        let (notif_thread, _) =
            build_thread_from_turns(vec![root_notif, followup], false, &sched_names);
        assert_eq!(
            notif_thread.title,
            "WhatsApp · alice: Are we meeting this weekend at the harbor?"
        );
        assert!(notif_thread.turns[0].hide_prompt);
        assert!(!notif_thread.turns[1].hide_prompt);
    }

    #[test]
    fn scans_activated_day_plan_dates_from_proactive_dir() {
        let dir = std::env::temp_dir().join(format!("hc-loader-dayplan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(load_activated_day_plan_dates(Some(&dir)).is_empty());

        let proactive = dir.join("proactive");
        std::fs::create_dir_all(&proactive).unwrap();
        std::fs::write(proactive.join("day-plan-2026-11-28.json"), "{}").unwrap();
        std::fs::write(proactive.join("day-plan-2026-11-26.json"), "{}").unwrap();
        std::fs::write(proactive.join("day-plan-draft.json"), "{}").unwrap();
        std::fs::write(proactive.join("workday.json"), "{}").unwrap();
        std::fs::create_dir_all(proactive.join("day-plan-2026-11-27.json")).unwrap();

        assert_eq!(
            load_activated_day_plan_dates(Some(&dir)),
            vec!["2026-11-26".to_string(), "2026-11-28".to_string()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
