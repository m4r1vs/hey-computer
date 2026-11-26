//! Production GTK4 + WebKitGTK 6.0 Editorial Answer Viewer (`hey-computer-viewer`).
//!
//! Serves the viewer shell and assets over a custom `hc://app/…` URI scheme,
//! injects the `window.heyComputer` / `window.sendPrompt` bridge and uncaught
//! widget error boundary at document start, enforces GTK4 + JS 5-step `Escape`
//! precedence and VIM navigation, provides hybrid `webkit6::FindController`
//! smartcase in-page search, streams on-demand TTS (`ReadAloud` + offline local
//! fallback), syncs `$HOME/.theme/mode`, GTK `prefer-dark-theme`, and
//! `$HOME/.theme/palette.json` via GIO `FileMonitor`, renders Scheduled Actions
//! overview/inspector views, streams live follow-up turns over
//! `hey-computer.sock`, and executes host actions.

pub mod actions;
pub mod loader;
pub mod palette;
pub mod stream;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use gtk4::gdk;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use hey_computer::config::Config;
use hey_computer::ipc;
use hey_computer::store;
use serde::Deserialize;
use serde_json::{Value, json};
use webkit6::prelude::*;
use webkit6::{
    FindOptions, LoadEvent, NavigationPolicyDecision, NavigationType, PolicyDecisionType,
    Settings as WebSettings, SnapshotOptions, SnapshotRegion, URISchemeRequest,
    UserContentInjectedFrames, UserContentManager, UserScript, UserScriptInjectionTime, WebContext,
    WebView,
};

use self::actions::{
    SnippetOutcome, approve_distill_changes, execute_snippet_with_timeout, plan_openhue_commands,
    run_openhue_plan, save_skill_files, save_to_vault, toggle_vault_journal_task,
};
use self::loader::{
    ViewerThread, load_schedule_detail, load_schedule_summaries, load_thread_for_turn,
    query_history_entries, resolve_initial_thread,
};
use self::palette::{WallpaperPalette, detect_system_dark, load_wallpaper_palette};
use self::stream::{
    StreamUiEvent, TtsUiEvent, normalize_ask_profile, query_running_schedule_ids,
    send_schedule_reload_best_effort, send_schedule_run, spawn_ask_stream, spawn_tts_stream,
};

const DEFAULT_APP_ID: &str = "hey-computer.answer";

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

type StreamHandler = Box<dyn FnMut(StreamUiEvent)>;
type TtsHandler = Box<dyn FnMut(TtsUiEvent)>;

thread_local! {
    static STREAM_HANDLERS: RefCell<HashMap<u64, StreamHandler>> =
        RefCell::new(HashMap::new());
    static TTS_HANDLERS: RefCell<HashMap<u64, TtsHandler>> =
        RefCell::new(HashMap::new());
}

fn register_stream_handler(handler: impl FnMut(StreamUiEvent) + 'static) -> u64 {
    let seq = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    STREAM_HANDLERS.with(|map| {
        map.borrow_mut().insert(seq, Box::new(handler));
    });
    seq
}

fn deliver_stream_event(seq: u64, event: StreamUiEvent) {
    let is_terminal = matches!(
        event,
        StreamUiEvent::Done { .. } | StreamUiEvent::Error { .. }
    );
    let mut maybe_cb = STREAM_HANDLERS.with(|map| {
        if is_terminal {
            map.borrow_mut().remove(&seq)
        } else {
            None
        }
    });

    if let Some(ref mut cb) = maybe_cb {
        cb(event);
        return;
    }

    // Borrow the callback outside `RefCell` in case the callback registers another stream.
    let mut taken = STREAM_HANDLERS.with(|map| map.borrow_mut().remove(&seq));
    if let Some(ref mut cb) = taken {
        cb(event);
    }
    if !is_terminal && let Some(cb) = taken {
        STREAM_HANDLERS.with(|map| {
            map.borrow_mut().insert(seq, cb);
        });
    }
}

fn register_tts_handler(handler: impl FnMut(TtsUiEvent) + 'static) -> u64 {
    let seq = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    TTS_HANDLERS.with(|map| {
        map.borrow_mut().insert(seq, Box::new(handler));
    });
    seq
}

fn deliver_tts_event(seq: u64, event: TtsUiEvent) {
    let is_terminal = matches!(event, TtsUiEvent::Finished | TtsUiEvent::Error(_));
    let mut maybe_cb = TTS_HANDLERS.with(|map| {
        if is_terminal {
            map.borrow_mut().remove(&seq)
        } else {
            None
        }
    });
    if let Some(ref mut cb) = maybe_cb {
        cb(event);
        return;
    }
    let mut taken = TTS_HANDLERS.with(|map| map.borrow_mut().remove(&seq));
    if let Some(ref mut cb) = taken {
        cb(event);
    }
    if !is_terminal && let Some(cb) = taken {
        TTS_HANDLERS.with(|map| {
            map.borrow_mut().insert(seq, cb);
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ForceTheme {
    Light,
    Dark,
}

impl ForceTheme {
    fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "hey-computer-viewer",
    version,
    about = "Editorial GTK4 + WebKitGTK 6.0 answer viewer for hey-computer"
)]
pub struct Cli {
    /// Optional Markdown file to render directly (`answer-<ms>-<id>.md`), or a
    /// numeric turn ID (`42` / `#42`) when no matching file exists.
    pub file: Option<PathBuf>,

    /// Path to `config.toml` (defaults to `~/.config/hey-computer/config.toml`).
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Daemon Unix socket path (defaults to `$HEY_COMPUTER_SOCKET` or `$XDG_RUNTIME_DIR/hey-computer.sock`).
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// State directory containing `history.db` (overrides `config.state_dir`).
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,

    /// Load the conversation thread for this turn ID from `history.db`.
    #[arg(long, value_name = "ID")]
    pub turn_id: Option<u64>,

    /// Open the Scheduled Actions overview.
    #[arg(long)]
    pub schedules: bool,

    /// Open the Schedule Inspector for a schedule ID or name (plus latest output).
    #[arg(long, value_name = "TARGET")]
    pub schedule: Option<String>,

    /// Open the History & FTS5 Recall drawer on startup.
    #[arg(long)]
    pub history: bool,

    /// Open the History & FTS5 Recall drawer pre-filtered with this query.
    #[arg(long, value_name = "QUERY")]
    pub recall: Option<String>,

    /// Enable editorial drop caps initially (`D` toggles interactively).
    #[arg(long)]
    pub drop_cap: bool,

    /// Auto-focus the bottom "Ask Computer" follow-up composer on load.
    #[arg(long)]
    pub focus_composer: bool,

    /// Hidden theme override used strictly for deterministic headless `--snapshot` runs.
    #[arg(long, hide = true, value_enum, value_name = "MODE")]
    pub force_theme: Option<ForceTheme>,

    /// Hidden search query pre-opened on load strictly for deterministic `--snapshot` runs.
    #[arg(long, hide = true, value_name = "QUERY")]
    pub initial_find: Option<String>,

    /// Headless screenshot mode: wait for load + 350ms settle, save PNG, and exit 0.
    #[arg(long, value_name = "OUT.png")]
    pub snapshot: Option<PathBuf>,

    /// Initial window width in logical pixels.
    #[arg(long, default_value_t = 1000)]
    pub width: i32,

    /// Initial window height in logical pixels.
    #[arg(long, default_value_t = 1100)]
    pub height: i32,

    /// Optional GTK/Wayland application ID override.
    #[arg(long, default_value = DEFAULT_APP_ID, value_name = "APP_ID")]
    pub name: String,
}

struct AppState {
    cli: Cli,
    config: Config,
    state_dir: Option<PathBuf>,
    socket: Option<PathBuf>,
    vault: Option<PathBuf>,
    skills_dir: Option<PathBuf>,
    assets_dir: PathBuf,
    active_thread: RefCell<ViewerThread>,
    latest_turn_id: Cell<Option<u64>>,
    active_schedule_target: RefCell<Option<String>>,
    boot_json: String,
    initial_palette: Option<WallpaperPalette>,
    theme_monitors: RefCell<Vec<gio::FileMonitor>>,
    tts_cancel: RefCell<Option<tokio::sync::watch::Sender<bool>>>,
    tts_gen: Cell<u64>,
}

const START_BRIDGE_JS: &str = r#"
(() => {
  if (window.heyComputer) return;

  const listeners = new Set();

  function post(payload) {
    try {
      const msg = typeof payload === "string" ? payload : JSON.stringify(payload);
      if (window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.hc) {
        window.webkit.messageHandlers.hc.postMessage(msg);
      }
    } catch (err) {
      console.error("[hc-bridge] post failed", err);
    }
  }

  function dispatchFromHost(msg) {
    for (const fn of Array.from(listeners)) {
      try {
        fn(msg);
      } catch (err) {
        console.error("[hc-bridge] listener error", err);
      }
    }
    if (typeof window.onHeyComputerMessage === "function") {
      try {
        window.onHeyComputerMessage(msg);
      } catch (err) {
        console.error("[hc-bridge] onHeyComputerMessage error", err);
      }
    }
    window.dispatchEvent(new CustomEvent("hc:message", { detail: msg }));
    window.dispatchEvent(new CustomEvent("hc:event", { detail: msg }));
    if (msg && typeof msg.type === "string") {
      window.dispatchEvent(new CustomEvent("hc:" + msg.type, { detail: msg }));
      const dashed = msg.type.replace(/_/g, "-");
      if (dashed !== msg.type) {
        window.dispatchEvent(new CustomEvent("hc:" + dashed, { detail: msg }));
      }
    }
  }

  window.__hcDispatch = dispatchFromHost;

  window.heyComputer = {
    post,
    onMessage(fn) {
      if (typeof fn === "function") listeners.add(fn);
      return () => listeners.delete(fn);
    },
    subscribe(fn) {
      return this.onMessage(fn);
    },
    copy(textOrOpts, label) {
      if (textOrOpts && typeof textOrOpts === "object") {
        post({ type: "copy", ...textOrOpts });
      } else {
        post({ type: "copy", text: String(textOrOpts ?? ""), label: label || "" });
      }
    },
    runCode(id, lang, code) {
      if (id && typeof id === "object") {
        post({ type: "run_code", ...id });
      } else {
        const parsedIdx = Number(id);
        post({
          type: "run_code",
          id: String(id ?? "0"),
          codeIndex: Number.isFinite(parsedIdx) ? parsedIdx : 0,
          lang: String(lang || "sh"),
          code: String(code ?? ""),
        });
      }
    },
    sendPrompt(promptOrOpts, context) {
      if (promptOrOpts && typeof promptOrOpts === "object") {
        post({ type: "send_prompt", ...promptOrOpts });
      } else if (context && typeof context === "object") {
        post({ type: "send_prompt", prompt: String(promptOrOpts ?? ""), ...context });
      } else {
        post({
          type: "send_prompt",
          prompt: String(promptOrOpts ?? ""),
        });
      }
    },
    saveToVault(markdown, mode, title) {
      if (markdown && typeof markdown === "object") {
        post({ type: "save_vault", ...markdown });
      } else {
        post({
          type: "save_vault",
          markdown: String(markdown ?? ""),
          mode: String(mode || "append"),
          title: String(title || ""),
        });
      }
    },
    saveSkillTemplate(skillNameOrOpts, skillMarkdown, templateHtml) {
      if (skillNameOrOpts && typeof skillNameOrOpts === "object") {
        post({ type: "save_skill_template", ...skillNameOrOpts });
      } else {
        post({
          type: "save_skill_template",
          skillName: String(skillNameOrOpts ?? ""),
          skillMarkdown: String(skillMarkdown ?? ""),
          templateHtml: String(templateHtml ?? ""),
        });
      }
    },
    searchHistory(queryOrOpts) {
      if (queryOrOpts && typeof queryOrOpts === "object") {
        post({ type: "search_history", ...queryOrOpts });
      } else {
        post({ type: "search_history", query: String(queryOrOpts ?? "") });
      }
    },
    openHistoryTurn(turnIdOrOpts) {
      if (turnIdOrOpts && typeof turnIdOrOpts === "object") {
        post({ type: "open_history_turn", ...turnIdOrOpts });
      } else {
        post({ type: "open_history_turn", turnId: turnIdOrOpts });
      }
    },
    speakText(text) {
      post({ type: "speak_text", text: String(text ?? "") });
    },
    stopTts() {
      post({ type: "stop_tts" });
    },
    loadSchedules() {
      post({ type: "load_schedules" });
    },
    openSchedule(target) {
      post({ type: "open_schedule", target: String(target ?? "") });
    },
    widgetAction(widget, actionOrPayload, payload) {
      if (widget && typeof widget === "object") {
        post({ type: "widget_action", ...widget });
      } else if (actionOrPayload && typeof actionOrPayload === "object") {
        post({
          type: "widget_action",
          widgetKind: String(widget ?? ""),
          ...actionOrPayload,
        });
      } else {
        post({
          type: "widget_action",
          widgetKind: String(widget ?? ""),
          action: String(actionOrPayload ?? ""),
          payload: payload ?? null,
        });
      }
    },
    closeWindow() {
      post({ type: "close" });
    },
  };

  window.sendPrompt = (prompt, context) => window.heyComputer.sendPrompt(prompt, context);

  window.addEventListener("error", (ev) => {
    const detail = {
      type: "widget_error",
      kind: "error",
      message: String(ev.message || (ev.error && ev.error.message) || "Uncaught widget script error"),
      filename: String(ev.filename || "widget"),
      lineno: Number(ev.lineno || 0),
      colno: Number(ev.colno || 0),
      source: String(ev.filename || ""),
      line: Number(ev.lineno || 0),
      column: Number(ev.colno || 0),
      stack: ev.error && ev.error.stack ? String(ev.error.stack) : "",
    };
    post(detail);
    window.dispatchEvent(new CustomEvent("hc:widget-error", { detail }));
  });

  window.addEventListener("unhandledrejection", (ev) => {
    const reason = ev.reason;
    const detail = {
      type: "widget_error",
      kind: "unhandledrejection",
      message:
        reason && reason.message
          ? String(reason.message)
          : String(reason ?? "Unhandled promise rejection"),
      filename: "promise",
      lineno: 0,
      colno: 0,
      source: "",
      line: 0,
      column: 0,
      stack: reason && reason.stack ? String(reason.stack) : "",
    };
    post(detail);
    window.dispatchEvent(new CustomEvent("hc:widget-error", { detail }));
  });
})();
"#;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IncomingMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default)]
    id: Option<Value>,
    #[serde(default, alias = "turn_id")]
    turn_id: Option<Value>,
    #[serde(default, alias = "latest_turn_id")]
    latest_turn_id: Option<Value>,
    #[serde(default, alias = "reply_to")]
    reply_to: Option<Value>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    lang: Option<String>,
    #[serde(default)]
    code_index: Option<usize>,
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    block_id: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    markdown: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    thread: Option<Value>,
    #[serde(default)]
    skill_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    skill_markdown: Option<String>,
    #[serde(default)]
    template_html: Option<String>,
    #[serde(default)]
    widget_kind: Option<String>,
    #[serde(default)]
    widget: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    payload: Option<Value>,
}

fn parse_optional_u64(val: Option<&Value>) -> Option<u64> {
    let v = val?;
    if let Some(n) = v.as_u64() {
        return (n > 0).then_some(n);
    }
    if let Some(i) = v.as_i64() {
        return (i > 0).then_some(i as u64);
    }
    if let Some(s) = v.as_str() {
        let trimmed = s.trim().strip_prefix("turn-").unwrap_or(s.trim());
        if let Ok(n) = trimmed.parse::<u64>() {
            return (n > 0).then_some(n);
        }
    }
    None
}

/// Compute Vim `smartcase` `webkit6::FindOptions`: case-insensitive + wrap
/// around unless `query` contains any uppercase character.
pub fn smartcase_find_options(query: &str) -> FindOptions {
    let has_upper = query.chars().any(char::is_uppercase);
    let mut opts = FindOptions::WRAP_AROUND;
    if !has_upper {
        opts |= FindOptions::CASE_INSENSITIVE;
    }
    opts
}

fn format_window_title(subtitle: &str) -> String {
    let trimmed = subtitle.trim();
    if trimmed.is_empty() || trimmed == "Ask Computer" {
        "Ask Computer".to_string()
    } else {
        format!("Ask Computer — {trimmed}")
    }
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();

    let mut config = match Config::load(cli.config.as_deref()) {
        Ok(cfg) => cfg,
        Err(err) if cli.config.is_some() => return Err(err),
        Err(err) => {
            eprintln!("hey-computer-viewer: warning loading default config: {err:#}");
            Config::default()
        }
    };
    if let Some(ref sd) = cli.state_dir {
        config.state_dir = sd.clone();
    }

    let state_dir = cli.state_dir.clone().or_else(|| config.state_dir().ok());
    let socket = ipc::socket_path(cli.socket.as_deref()).ok();
    let vault = (!config.instructions.vault.as_os_str().is_empty())
        .then(|| config.instructions.vault.clone());
    let skills_dir = config.user_skills_dir();
    let assets_dir = resolve_assets_dir();

    let (initial_thread, mut latest_turn_id) = resolve_initial_thread(
        state_dir.as_deref(),
        cli.turn_id,
        cli.file.as_deref(),
        cli.drop_cap,
    )?;
    let initial_recall_query = cli.recall.as_deref().unwrap_or("");
    let history_entries = query_history_entries(state_dir.as_deref(), initial_recall_query, 30);

    let running_ids = if cli.schedules || cli.schedule.is_some() {
        query_running_schedule_ids(socket.as_deref())
    } else {
        Default::default()
    };
    let schedules = load_schedule_summaries(state_dir.as_deref(), &running_ids);

    let mut initial_view = "thread";
    let mut schedule_detail = None;
    let mut initial_schedule_target = None;
    if let Some(ref target) = cli.schedule {
        match load_schedule_detail(state_dir.as_deref(), target, &running_ids, cli.drop_cap) {
            Ok(Some(detail)) => {
                if let Some(lt_id) = detail.latest_turn.as_ref().and_then(|t| t.turn_id) {
                    latest_turn_id = Some(lt_id);
                }
                initial_schedule_target = Some(detail.summary.name.clone());
                schedule_detail = Some(detail);
                initial_view = "schedule_inspector";
            }
            Ok(None) => {
                eprintln!(
                    "hey-computer-viewer: schedule `{target}` not found; opening schedules overview"
                );
                initial_view = "schedules_overview";
            }
            Err(err) => {
                tracing::warn!("cannot load schedule `{target}`: {err:#}");
                initial_view = "schedules_overview";
            }
        }
    } else if cli.schedules {
        initial_view = "schedules_overview";
    }

    let system_dark = detect_system_dark();
    let resolved_dark = resolve_is_dark(cli.force_theme, system_dark);
    let theme_mode_str = cli.force_theme.map_or("auto", ForceTheme::as_str);
    let initial_palette = load_wallpaper_palette();
    let today_journal = format!("Journal/{}.md", store::local_today_ymd());

    let boot_payload = json!({
        "theme": theme_mode_str,
        "systemDark": system_dark,
        "resolvedTheme": if resolved_dark { "dark" } else { "light" },
        "palette": initial_palette,
        "dropCap": cli.drop_cap,
        "focusComposer": cli.focus_composer,
        "openHistory": cli.history || cli.recall.is_some(),
        "recallQuery": initial_recall_query,
        "initialFindQuery": cli.initial_find.as_deref().unwrap_or(""),
        "initialView": initial_view,
        "scheduleTarget": initial_schedule_target,
        "schedules": schedules,
        "scheduleDetail": schedule_detail,
        "vaultConfigured": vault.is_some(),
        "skillsDirConfigured": skills_dir.is_some(),
        "stateDirConfigured": state_dir.is_some(),
        "todayJournalPath": today_journal,
        "headless": cli.snapshot.is_some(),
        "thread": initial_thread,
        "latestTurnId": latest_turn_id,
        "history": history_entries,
    });
    let boot_json = escape_json_for_inline_script(&serde_json::to_string(&boot_payload)?);

    let app_id = if gio::Application::id_is_valid(&cli.name) {
        cli.name.clone()
    } else {
        eprintln!(
            "hey-computer-viewer: invalid application ID `{}`; falling back to `{DEFAULT_APP_ID}`",
            cli.name
        );
        DEFAULT_APP_ID.to_string()
    };
    glib::set_prgname(Some(&app_id));

    let state = Rc::new(AppState {
        cli,
        config,
        state_dir,
        socket,
        vault,
        skills_dir,
        assets_dir,
        active_thread: RefCell::new(initial_thread),
        latest_turn_id: Cell::new(latest_turn_id),
        active_schedule_target: RefCell::new(initial_schedule_target),
        boot_json,
        initial_palette,
        theme_monitors: RefCell::new(Vec::new()),
        tts_cancel: RefCell::new(None),
        tts_gen: Cell::new(0),
    });

    let app = gtk4::Application::builder()
        .application_id(&app_id)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let state_for_activate = Rc::clone(&state);
    app.connect_activate(move |app| {
        build_ui(app, &state_for_activate);
    });

    // Pass empty argv so GTK does not reject clap's custom flags (`--turn-id`, `--snapshot`, etc.).
    let exit = app.run_with_args::<&str>(&[]);
    if exit != glib::ExitCode::SUCCESS {
        std::process::exit(1);
    }
    Ok(())
}

fn build_ui(app: &gtk4::Application, state: &Rc<AppState>) {
    let web_context = WebContext::new();
    let assets_dir = state.assets_dir.clone();
    let boot_json = state.boot_json.clone();
    let force_theme = state.cli.force_theme;
    let fallback_palette = state.initial_palette.clone();

    web_context.register_uri_scheme("hc", move |request| {
        serve_hc_scheme_request(
            request,
            &assets_dir,
            &boot_json,
            force_theme,
            fallback_palette.as_ref(),
        );
    });

    let ucm = UserContentManager::new();
    let bridge_script = UserScript::new(
        START_BRIDGE_JS,
        UserContentInjectedFrames::AllFrames,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    );
    ucm.add_script(&bridge_script);
    let _ = ucm.register_script_message_handler("hc", None);

    let settings = WebSettings::new();
    settings.set_enable_javascript(true);
    settings.set_enable_developer_extras(state.cli.snapshot.is_none());

    let webview = WebView::builder()
        .web_context(&web_context)
        .user_content_manager(&ucm)
        .settings(&settings)
        .hexpand(true)
        .vexpand(true)
        .build();

    let initial_dark = resolve_is_dark(state.cli.force_theme, detect_system_dark());
    apply_webview_bg(&webview, state.cli.force_theme, initial_dark);

    let window_title = if let Some(ref target) = *state.active_schedule_target.borrow() {
        format_window_title(target)
    } else if state.cli.schedules {
        format_window_title("Scheduled Actions")
    } else {
        let t = state.active_thread.borrow();
        format_window_title(&t.title)
    };

    let window = gtk4::ApplicationWindow::builder()
        .application(app)
        .title(&window_title)
        .default_width(state.cli.width)
        .default_height(state.cli.height)
        .child(&webview)
        .build();

    // Stop any active TTS stream cleanly if the window is closed via WM or shortcut.
    let state_for_close = Rc::clone(state);
    let wv_for_close = webview.clone();
    window.connect_close_request(move |_| {
        stop_active_tts(&state_for_close, &wv_for_close);
        glib::Propagation::Proceed
    });

    // GTK4 defense-in-depth keyboard controllers:
    // 1. Capture: handles `Escape` before `page_ready` and `Ctrl+q`/`Ctrl+w` always.
    // 2. Bubble: fallback on unhandled `Escape` so the window always closes cleanly.
    let page_ready = Rc::new(Cell::new(false));
    {
        let capture = gtk4::EventControllerKey::new();
        capture.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let win_cap = window.clone();
        let ready_cap = Rc::clone(&page_ready);
        capture.connect_key_pressed(move |_ctrl, key, _keycode, mods| {
            let ctrl_only = mods.contains(gdk::ModifierType::CONTROL_MASK)
                && !mods.contains(gdk::ModifierType::ALT_MASK)
                && !mods.contains(gdk::ModifierType::SUPER_MASK);
            if ctrl_only && matches!(key, gdk::Key::q | gdk::Key::Q | gdk::Key::w | gdk::Key::W) {
                win_cap.close();
                return glib::Propagation::Stop;
            }
            if key == gdk::Key::Escape && !ready_cap.get() {
                win_cap.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        window.add_controller(capture);

        let bubble = gtk4::EventControllerKey::new();
        bubble.set_propagation_phase(gtk4::PropagationPhase::Bubble);
        let win_bub = window.clone();
        bubble.connect_key_pressed(move |_ctrl, key, _keycode, _mods| {
            if key == gdk::Key::Escape {
                win_bub.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        window.add_controller(bubble);
    }

    // Wire WebKitGTK FindController signals (`/` hybrid search pill).
    if let Some(find_ctrl) = webview.find_controller() {
        let wv_found = webview.clone();
        find_ctrl.connect_found_text(move |_fc, match_count| {
            dispatch_to_js(
                &wv_found,
                &json!({
                    "type": "find_status",
                    "found": true,
                    "count": match_count,
                }),
            );
        });
        let wv_counted = webview.clone();
        find_ctrl.connect_counted_matches(move |_fc, match_count| {
            dispatch_to_js(
                &wv_counted,
                &json!({
                    "type": "find_status",
                    "found": match_count > 0,
                    "count": match_count,
                }),
            );
        });
        let wv_failed = webview.clone();
        find_ctrl.connect_failed_to_find_text(move |_fc| {
            dispatch_to_js(
                &wv_failed,
                &json!({
                    "type": "find_status",
                    "found": false,
                    "count": 0,
                }),
            );
        });
    }

    // Disarm external navigations: keep `hc://app/…` inside WebView, open clicked links externally.
    let is_headless = state.cli.snapshot.is_some();
    let wv_for_policy = webview.clone();
    webview.connect_decide_policy(move |_wv, decision, decision_type| {
        if (decision_type == PolicyDecisionType::NavigationAction
            || decision_type == PolicyDecisionType::NewWindowAction)
            && let Ok(nav_decision) = decision.clone().downcast::<NavigationPolicyDecision>()
            && let Some(action) = nav_decision.navigation_action()
            && let Some(req) = action.request()
            && let Some(uri) = req.uri()
        {
            let uri_str = uri.as_str();
            if uri_str.starts_with("hc://") || uri_str.starts_with("about:blank") {
                decision.use_();
                return true;
            }
            decision.ignore();
            if !is_headless
                && (action.navigation_type() == NavigationType::LinkClicked
                    || decision_type == PolicyDecisionType::NewWindowAction)
                && (uri_str.starts_with("https://")
                    || uri_str.starts_with("http://")
                    || uri_str.starts_with("mailto:"))
            {
                let _ = gio::AppInfo::launch_default_for_uri(uri_str, gio::AppLaunchContext::NONE);
                dispatch_to_js(
                    &wv_for_policy,
                    &json!({
                        "type": "toast",
                        "message": format!("Opened external link: {uri_str}"),
                        "kind": "info"
                    }),
                );
            }
            return true;
        }
        false
    });

    let state_for_ipc = Rc::clone(state);
    let wv_for_ipc = webview.clone();
    let win_for_ipc = window.clone();
    ucm.connect_script_message_received(Some("hc"), move |_ucm, js_val| {
        let raw_json = if js_val.is_string() {
            js_val.to_str().to_string()
        } else {
            js_val.to_json(0).map(|s| s.to_string()).unwrap_or_default()
        };
        if let Ok(msg) = serde_json::from_str::<IncomingMessage>(&raw_json) {
            handle_ipc_message(&msg, &state_for_ipc, &wv_for_ipc, &win_for_ipc);
        }
    });

    setup_live_theme_watcher(&webview, state);

    // Always grab focus on LoadEvent::Finished and mark page_ready for Escape precedence.
    let focus_composer = state.cli.focus_composer;
    let ready_for_load = Rc::clone(&page_ready);
    webview.connect_load_changed(move |wv, event| {
        if event == LoadEvent::Finished {
            ready_for_load.set(true);
            wv.grab_focus();
            if focus_composer {
                dispatch_to_js(wv, &json!({ "type": "focus_composer" }));
            }
        }
    });

    if let Some(snapshot_out) = state.cli.snapshot.clone() {
        let app_clone = app.clone();
        let captured = Rc::new(Cell::new(false));
        let captured_for_load = Rc::clone(&captured);
        webview.connect_load_changed(move |wv, event| {
            if event != LoadEvent::Finished || captured_for_load.get() {
                return;
            }
            captured_for_load.set(true);
            let wv = wv.clone();
            let out = snapshot_out.clone();
            let app_inner = app_clone.clone();
            glib::timeout_add_local_once(Duration::from_millis(350), move || {
                capture_snapshot_and_quit(&wv, &out, &app_inner);
            });
        });
    }

    webview.load_uri("hc://app/index.html");
    window.present();
}

fn stop_active_tts(state: &AppState, webview: &WebView) {
    let next_gen = state.tts_gen.get().wrapping_add(1);
    state.tts_gen.set(next_gen);
    if let Some(tx) = state.tts_cancel.borrow_mut().take() {
        let _ = tx.send(true);
    }
    dispatch_to_js(webview, &json!({ "type": "tts_state", "speaking": false }));
}

fn start_tts(state: &Rc<AppState>, webview: &WebView, text: String) {
    stop_active_tts(state, webview);
    if text.trim().is_empty() {
        return;
    }

    let generation = state.tts_gen.get().wrapping_add(1);
    state.tts_gen.set(generation);

    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    *state.tts_cancel.borrow_mut() = Some(cancel_tx);

    dispatch_to_js(webview, &json!({ "type": "tts_state", "speaking": true }));

    let wv = webview.clone();
    let state_clone = Rc::clone(state);
    let seq = register_tts_handler(move |ev| {
        if state_clone.tts_gen.get() != generation {
            return;
        }
        match ev {
            TtsUiEvent::Started => {
                dispatch_to_js(&wv, &json!({ "type": "tts_state", "speaking": true }));
            }
            TtsUiEvent::Finished => {
                let _ = state_clone.tts_cancel.borrow_mut().take();
                dispatch_to_js(&wv, &json!({ "type": "tts_state", "speaking": false }));
            }
            TtsUiEvent::Error(err_msg) => {
                let _ = state_clone.tts_cancel.borrow_mut().take();
                dispatch_to_js(&wv, &json!({ "type": "tts_state", "speaking": false }));
                dispatch_to_js(
                    &wv,
                    &json!({
                        "type": "toast",
                        "kind": "warn",
                        "message": err_msg,
                    }),
                );
            }
        }
    });

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TtsUiEvent>();
    spawn_tts_stream(
        state.socket.clone(),
        state.config.present.speak.clone(),
        text,
        cancel_rx,
        tx,
    );

    let _ = std::thread::Builder::new()
        .name("hc-viewer-tts-bridge".to_string())
        .spawn(move || {
            while let Some(ev) = rx.blocking_recv() {
                glib::idle_add_once(move || {
                    deliver_tts_event(seq, ev);
                });
            }
        });
}

fn dispatch_schedules_refresh(state: &Rc<AppState>, webview: &WebView) {
    let state_dir = state.state_dir.clone();
    let socket = state.socket.clone();
    let wv = webview.clone();
    glib::spawn_future_local(async move {
        let schedules = gio::spawn_blocking(move || {
            let running = query_running_schedule_ids(socket.as_deref());
            load_schedule_summaries(state_dir.as_deref(), &running)
        })
        .await
        .unwrap_or_default();
        dispatch_to_js(
            &wv,
            &json!({
                "type": "schedules_loaded",
                "schedules": schedules,
            }),
        );
    });
}

fn dispatch_open_schedule(
    state: &Rc<AppState>,
    webview: &WebView,
    window: &gtk4::ApplicationWindow,
    target: String,
) {
    let state_dir = state.state_dir.clone();
    let socket = state.socket.clone();
    let drop_cap = state.cli.drop_cap;
    let wv = webview.clone();
    let win = window.clone();
    let state_clone = Rc::clone(state);
    let target_for_err = target.clone();
    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(move || {
            let running = query_running_schedule_ids(socket.as_deref());
            load_schedule_detail(state_dir.as_deref(), &target, &running, drop_cap)
        })
        .await;

        match result {
            Ok(Ok(Some(detail))) => {
                if let Some(lt_id) = detail.latest_turn.as_ref().and_then(|t| t.turn_id) {
                    state_clone.latest_turn_id.set(Some(lt_id));
                }
                *state_clone.active_schedule_target.borrow_mut() =
                    Some(detail.summary.name.clone());
                win.set_title(Some(&format_window_title(&detail.summary.title)));
                dispatch_to_js(
                    &wv,
                    &json!({
                        "type": "schedule_loaded",
                        "target": detail.summary.name,
                        "detail": detail,
                    }),
                );
            }
            Ok(Ok(None)) => {
                dispatch_to_js(
                    &wv,
                    &json!({
                        "type": "toast",
                        "kind": "warn",
                        "message": format!("Schedule `{target_for_err}` not found"),
                    }),
                );
                dispatch_schedules_refresh(&state_clone, &wv);
            }
            Ok(Err(err)) => {
                dispatch_to_js(
                    &wv,
                    &json!({
                        "type": "toast",
                        "kind": "error",
                        "message": format!("Failed loading schedule `{target_for_err}`: {err:#}"),
                    }),
                );
            }
            Err(_) => {}
        }
    });
}

fn handle_ipc_message(
    msg: &IncomingMessage,
    state: &Rc<AppState>,
    webview: &WebView,
    window: &gtk4::ApplicationWindow,
) {
    match msg.msg_type.as_str() {
        "copy" => {
            let text = msg.text.as_deref().unwrap_or("");
            if let Some(display) = gdk::Display::default() {
                display.clipboard().set_text(text);
            }
            let label = msg
                .label
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("Copied to clipboard");
            dispatch_to_js(
                webview,
                &json!({
                    "type": "toast",
                    "message": label,
                    "kind": "ok"
                }),
            );
        }
        "find_search" => {
            if let Some(fc) = webview.find_controller() {
                let query = msg.query.as_deref().unwrap_or("");
                if query.is_empty() {
                    fc.search_finish();
                } else {
                    let opts = smartcase_find_options(query).bits();
                    fc.search(query, opts, 1000);
                    fc.count_matches(query, opts, 1000);
                }
            }
        }
        "find_next" => {
            if let Some(fc) = webview.find_controller() {
                fc.search_next();
            }
        }
        "find_prev" => {
            if let Some(fc) = webview.find_controller() {
                fc.search_previous();
            }
        }
        "find_close" => {
            if let Some(fc) = webview.find_controller() {
                fc.search_finish();
            }
        }
        "speak_text" => {
            let text = msg.text.clone().unwrap_or_default();
            start_tts(state, webview, text);
        }
        "stop_tts" => {
            stop_active_tts(state, webview);
        }
        "sync_active_thread" => {
            if let Some(ref th_val) = msg.thread
                && let Ok(parsed) = serde_json::from_value::<ViewerThread>(th_val.clone())
            {
                *state.active_thread.borrow_mut() = parsed;
            }
            if let Some(latest) = parse_optional_u64(msg.latest_turn_id.as_ref()) {
                state.latest_turn_id.set(Some(latest));
            }
            if let Some(ref title) = msg.title {
                window.set_title(Some(&format_window_title(title)));
            }
        }
        "load_schedules" => {
            stop_active_tts(state, webview);
            dispatch_schedules_refresh(state, webview);
        }
        "open_schedule" => {
            stop_active_tts(state, webview);
            if let Some(target) = msg.target.as_deref().filter(|s| !s.trim().is_empty()) {
                dispatch_open_schedule(state, webview, window, target.to_string());
            }
        }
        "schedule_action" => {
            let action = msg.action.as_deref().unwrap_or("").to_string();
            let target = msg.target.as_deref().unwrap_or("").trim().to_string();
            if target.is_empty() {
                return;
            }
            let state_dir = state.state_dir.clone();
            let socket = state.socket.clone();
            let wv = webview.clone();
            let win = window.clone();
            let state_clone = Rc::clone(state);
            glib::spawn_future_local(async move {
                let target_clone = target.clone();
                let action_clone = action.clone();
                let outcome = gio::spawn_blocking(move || -> Result<(String, bool)> {
                    match action_clone.as_str() {
                        "run" => {
                            let message = send_schedule_run(socket.as_deref(), &target_clone)?;
                            Ok((message, false))
                        }
                        "pause" | "resume" => {
                            let Some(dir) = state_dir.as_deref() else {
                                anyhow::bail!("No state directory configured");
                            };
                            let enable = action_clone == "resume";
                            let next_run_at = if enable {
                                let Some(existing) = store::get_schedule(dir, &target_clone)?
                                else {
                                    anyhow::bail!("Schedule `{target_clone}` not found");
                                };
                                let spec = hey_computer::scheduler::ScheduleSpec::parse(
                                    &existing.schedule_spec,
                                )?;
                                Some(spec.first_run_after(store::unix_timestamp_ms())?)
                            } else {
                                None
                            };
                            let job = store::set_schedule_enabled(
                                dir,
                                &target_clone,
                                enable,
                                next_run_at,
                            )?;
                            send_schedule_reload_best_effort(socket.as_deref());
                            Ok((
                                format!(
                                    "{} schedule `{}`",
                                    if enable { "Resumed" } else { "Paused" },
                                    job.name
                                ),
                                false,
                            ))
                        }
                        "remove" => {
                            let Some(dir) = state_dir.as_deref() else {
                                anyhow::bail!("No state directory configured");
                            };
                            let Some(job) = store::get_schedule(dir, &target_clone)? else {
                                anyhow::bail!("Schedule `{target_clone}` not found");
                            };
                            if job.managed {
                                anyhow::bail!("Managed in config.toml — pause instead of removing");
                            }
                            let _ = store::delete_schedule(dir, &target_clone)?;
                            send_schedule_reload_best_effort(socket.as_deref());
                            Ok((format!("Removed schedule `{}`", job.name), true))
                        }
                        other => anyhow::bail!("Unknown schedule action `{other}`"),
                    }
                })
                .await;

                match outcome {
                    Ok(Ok((message, removed))) => {
                        dispatch_to_js(
                            &wv,
                            &json!({
                                "type": "toast",
                                "kind": "ok",
                                "message": message,
                            }),
                        );
                        dispatch_schedules_refresh(&state_clone, &wv);
                        let inspecting = state_clone.active_schedule_target.borrow().clone();
                        if removed && inspecting.as_deref() == Some(target.as_str()) {
                            *state_clone.active_schedule_target.borrow_mut() = None;
                            win.set_title(Some(&format_window_title("Scheduled Actions")));
                        } else if !removed && inspecting.as_deref() == Some(target.as_str()) {
                            dispatch_open_schedule(&state_clone, &wv, &win, target);
                        }
                    }
                    Ok(Err(err)) => {
                        dispatch_to_js(
                            &wv,
                            &json!({
                                "type": "toast",
                                "kind": "warn",
                                "message": format!("{err:#}"),
                            }),
                        );
                        dispatch_schedules_refresh(&state_clone, &wv);
                    }
                    Err(_) => {}
                }
            });
        }
        "run_code" => {
            let code = msg.code.clone().unwrap_or_default();
            let lang = msg
                .lang
                .clone()
                .unwrap_or_else(|| "sh".to_string())
                .to_ascii_lowercase();
            let code_index = msg.code_index.or(msg.index).unwrap_or(0);
            let block_id = msg.block_id.clone();
            let run_id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let caller_key = msg.id.clone().unwrap_or_else(|| json!(code_index));

            dispatch_to_js(
                webview,
                &json!({
                    "type": "code_run_started",
                    "id": caller_key,
                    "runId": run_id,
                    "codeIndex": code_index,
                    "index": code_index,
                    "blockId": block_id,
                    "lang": lang,
                }),
            );

            let wv = webview.clone();
            let lang_for_done = lang.clone();
            let block_for_done = block_id;
            let key_for_done = caller_key;
            glib::spawn_future_local(async move {
                let outcome = gio::spawn_blocking(move || {
                    execute_snippet_with_timeout(&code, &lang, Duration::from_secs(15))
                })
                .await
                .unwrap_or_else(|_| SnippetOutcome {
                    stdout: String::new(),
                    stderr: "Snippet worker thread panicked".to_string(),
                    exit_code: -1,
                    duration_ms: 0,
                    timed_out: false,
                });
                dispatch_to_js(
                    &wv,
                    &json!({
                        "type": "code_run_finished",
                        "id": key_for_done,
                        "runId": run_id,
                        "codeIndex": code_index,
                        "index": code_index,
                        "blockId": block_for_done,
                        "lang": lang_for_done,
                        "stdout": outcome.stdout,
                        "stderr": outcome.stderr,
                        "exitCode": outcome.exit_code,
                        "durationMs": outcome.duration_ms,
                        "timedOut": outcome.timed_out,
                    }),
                );
            });
        }
        "send_prompt" => {
            let prompt = msg
                .prompt
                .as_deref()
                .or(msg.text.as_deref())
                .unwrap_or("")
                .trim()
                .to_string();
            if prompt.is_empty() {
                return;
            }
            let explicit_reply = parse_optional_u64(msg.reply_to.as_ref());
            let profile = normalize_ask_profile(msg.profile.as_deref());
            start_live_stream(state, webview, prompt, profile, explicit_reply, false);
        }
        "save_vault" => {
            let markdown = msg
                .markdown
                .as_deref()
                .or(msg.content.as_deref())
                .or(msg.text.as_deref())
                .unwrap_or("");
            let mode = msg.mode.as_deref().unwrap_or("append");
            let title = msg
                .title
                .as_deref()
                .or(msg.path.as_deref())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("Ask Computer");

            match save_to_vault(state.vault.as_deref(), mode, title, markdown) {
                Ok(out) => {
                    dispatch_to_js(
                        webview,
                        &json!({
                            "type": "vault_saved",
                            "saved": out.saved,
                            "previewOnly": out.preview_only,
                            "quiet": out.quiet,
                            "path": out.path,
                            "preview": out.preview,
                            "message": out.message,
                        }),
                    );
                    dispatch_to_js(
                        webview,
                        &json!({
                            "type": "toast",
                            "message": out.message,
                            "kind": if out.saved { "ok" } else { "info" }
                        }),
                    );
                }
                Err(err) => {
                    dispatch_to_js(
                        webview,
                        &json!({
                            "type": "toast",
                            "message": format!("Vault write failed: {err:#}"),
                            "kind": "error"
                        }),
                    );
                }
            }
        }
        "save_skill_template" => {
            let raw_name = msg
                .skill_name
                .as_deref()
                .or(msg.name.as_deref())
                .unwrap_or("widget-custom");
            let skill_markdown = msg
                .skill_markdown
                .as_deref()
                .or(msg.markdown.as_deref())
                .unwrap_or("");
            let template_html = msg.template_html.as_deref().unwrap_or("");

            match save_skill_files(
                state.skills_dir.as_deref(),
                raw_name,
                skill_markdown,
                template_html,
                true,
            ) {
                Ok(out) => {
                    dispatch_to_js(
                        webview,
                        &json!({
                            "type": "skill_template_saved",
                            "saved": out.saved,
                            "previewOnly": out.preview_only,
                            "skillName": out.skill_name,
                            "path": out.path,
                            "message": out.message,
                            "skillMarkdown": skill_markdown,
                            "templateHtml": template_html,
                        }),
                    );
                    dispatch_to_js(
                        webview,
                        &json!({
                            "type": "toast",
                            "message": out.message,
                            "kind": if out.saved { "ok" } else { "info" }
                        }),
                    );
                }
                Err(err) => {
                    dispatch_to_js(
                        webview,
                        &json!({
                            "type": "toast",
                            "message": format!("Failed saving skill template: {err:#}"),
                            "kind": "error"
                        }),
                    );
                }
            }
        }
        "search_history" => {
            let query = msg.query.as_deref().unwrap_or("");
            let results = query_history_entries(state.state_dir.as_deref(), query, 30);
            dispatch_to_js(
                webview,
                &json!({
                    "type": "history_results",
                    "query": query,
                    "results": results,
                    "items": results,
                }),
            );
        }
        "open_history_turn" => {
            stop_active_tts(state, webview);
            let Some(turn_id) = parse_optional_u64(msg.turn_id.as_ref().or(msg.id.as_ref())) else {
                return;
            };
            let Some(ref dir) = state.state_dir else {
                return;
            };
            match load_thread_for_turn(dir, turn_id, state.cli.drop_cap) {
                Ok(Some((thread, latest_id))) => {
                    state.latest_turn_id.set(Some(latest_id));
                    *state.active_schedule_target.borrow_mut() = None;
                    window.set_title(Some(&format_window_title(&thread.title)));
                    dispatch_to_js(
                        webview,
                        &json!({
                            "type": "thread_loaded",
                            "turnId": turn_id,
                            "latestTurnId": latest_id,
                            "thread": &thread,
                        }),
                    );
                    *state.active_thread.borrow_mut() = thread;
                }
                Ok(None) => {
                    dispatch_to_js(
                        webview,
                        &json!({
                            "type": "toast",
                            "message": format!("Turn #{turn_id} was not found in history.db"),
                            "kind": "warn"
                        }),
                    );
                }
                Err(err) => {
                    dispatch_to_js(
                        webview,
                        &json!({
                            "type": "toast",
                            "message": format!("Failed loading turn #{turn_id}: {err:#}"),
                            "kind": "error"
                        }),
                    );
                }
            }
        }
        "widget_action" => {
            handle_widget_action(msg, state, webview);
        }
        "widget_error" => {
            let err_msg = msg
                .message
                .as_deref()
                .unwrap_or("Uncaught widget script error");
            tracing::warn!("isolated widget JS error: {err_msg}");
        }
        "close" => {
            stop_active_tts(state, webview);
            window.close();
        }
        _ => {}
    }
}

fn handle_widget_action(msg: &IncomingMessage, state: &Rc<AppState>, webview: &WebView) {
    let widget_kind = msg
        .widget_kind
        .as_deref()
        .or(msg.widget.as_deref())
        .or(msg.kind.as_deref())
        .unwrap_or("widget");
    let action = msg.action.as_deref().unwrap_or("update");
    let payload = msg.payload.as_ref();

    match (widget_kind, action) {
        ("distill_diff", "approve_changes") => {
            let empty_obj = json!({});
            let p = payload.unwrap_or(&empty_obj);
            let fallback_data = find_active_widget_data(state, "distill_diff", p);
            match approve_distill_changes(
                &state.config,
                state.state_dir.as_deref(),
                state.skills_dir.as_deref(),
                p,
                fallback_data.as_ref(),
            ) {
                Ok(out) => {
                    dispatch_to_js(
                        webview,
                        &json!({
                            "type": "widget_ack",
                            "ok": true,
                            "widgetKind": "distill_diff",
                            "action": "approve_changes",
                            "targetFile": out.target_file,
                            "skillSavedPath": out.skill_saved_path,
                            "appliedHunks": out.applied_hunks,
                            "combinedChars": out.combined_chars,
                            "maxChars": out.max_chars,
                            "message": out.message,
                        }),
                    );
                    dispatch_to_js(
                        webview,
                        &json!({
                            "type": "toast",
                            "message": out.message,
                            "kind": "ok"
                        }),
                    );
                }
                Err(err) => {
                    let err_msg = format!("{err:#}");
                    dispatch_to_js(
                        webview,
                        &json!({
                            "type": "widget_ack",
                            "ok": false,
                            "widgetKind": "distill_diff",
                            "action": "approve_changes",
                            "error": err_msg,
                        }),
                    );
                    dispatch_to_js(
                        webview,
                        &json!({
                            "type": "toast",
                            "message": format!("Distill apply failed: {err_msg}"),
                            "kind": "error"
                        }),
                    );
                }
            }
        }
        ("hue_rooms", "activate_scene" | "toggle_room" | "set_brightness" | "set_temp") => {
            let plan = match plan_openhue_commands(action, payload) {
                Ok(plan) => plan,
                Err(err) => {
                    dispatch_to_js(
                        webview,
                        &json!({
                            "type": "toast",
                            "message": format!("Hue action error: {err:#}"),
                            "kind": "error"
                        }),
                    );
                    return;
                }
            };
            let wv = webview.clone();
            let state_clone = Rc::clone(state);
            let action_owned = action.to_string();
            glib::spawn_future_local(async move {
                let plan_for_run = plan.clone();
                let res = gio::spawn_blocking(move || {
                    run_openhue_plan(&plan_for_run, Duration::from_secs(8))
                })
                .await;

                match res {
                    Ok(Ok(())) => {
                        dispatch_to_js(
                            &wv,
                            &json!({
                                "type": "widget_ack",
                                "ok": true,
                                "widgetKind": "hue_rooms",
                                "action": action_owned,
                                "message": plan.summary,
                            }),
                        );
                        dispatch_to_js(
                            &wv,
                            &json!({
                                "type": "toast",
                                "message": plan.summary,
                                "kind": "ok"
                            }),
                        );
                    }
                    Ok(Err(_)) | Err(_) => {
                        start_live_stream(
                            &state_clone,
                            &wv,
                            plan.fallback_prompt,
                            None,
                            None,
                            true,
                        );
                    }
                }
            });
        }
        ("tasks_journal", "toggle_journal_task") => {
            let text = payload
                .and_then(|p| p.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let done = payload.and_then(|p| p.get("done")).and_then(Value::as_bool);
            match toggle_vault_journal_task(state.vault.as_deref(), text, done) {
                Ok((path, new_done)) => {
                    let msg_text = format!(
                        "{} task in {}",
                        if new_done { "Completed" } else { "Reopened" },
                        path.display()
                    );
                    dispatch_to_js(
                        webview,
                        &json!({
                            "type": "widget_ack",
                            "ok": true,
                            "widgetKind": "tasks_journal",
                            "action": "toggle_journal_task",
                            "done": new_done,
                            "message": msg_text,
                        }),
                    );
                    dispatch_to_js(
                        webview,
                        &json!({
                            "type": "toast",
                            "message": msg_text,
                            "kind": "ok"
                        }),
                    );
                }
                Err(err) => {
                    let msg_text = if state.vault.is_none() {
                        "Preview only (configure instructions.vault to update Journal tasks)"
                            .to_string()
                    } else {
                        format!("Journal task update failed: {err:#}")
                    };
                    dispatch_to_js(
                        webview,
                        &json!({
                            "type": "toast",
                            "message": msg_text,
                            "kind": if state.vault.is_none() { "info" } else { "warn" }
                        }),
                    );
                }
            }
        }
        ("tasks_journal", "toggle_dated_task") => {
            let text = payload
                .and_then(|p| p.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("task");
            let id = payload
                .and_then(|p| p.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let done = payload
                .and_then(|p| p.get("done"))
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let status_word = if done { "completed" } else { "incomplete" };
            let id_clause = if id.is_empty() {
                String::new()
            } else {
                format!(" (task id `{id}`)")
            };
            let prompt = format!("Mark Google Task \"{text}\"{id_clause} as {status_word}.");
            start_live_stream(state, webview, prompt, None, None, true);
        }
        ("transit_board", "refresh") => {
            let origin = payload
                .and_then(|p| p.get("origin"))
                .and_then(Value::as_str)
                .unwrap_or("origin");
            let destination = payload
                .and_then(|p| p.get("destination"))
                .and_then(Value::as_str)
                .unwrap_or("destination");
            let prompt = format!(
                "Refresh live transit departures from {origin} to {destination} and display an updated transit_board widget."
            );
            start_live_stream(state, webview, prompt, None, None, true);
        }
        ("dev_triage", "move_issue_review") => {
            let ident = payload
                .and_then(|p| p.get("identifier").or_else(|| p.get("id")))
                .and_then(Value::as_str)
                .unwrap_or("issue");
            let prompt = format!("Move Linear issue {ident} to In Review and summarize readiness.");
            start_live_stream(state, webview, prompt, None, None, true);
        }
        _ => {
            dispatch_to_js(
                webview,
                &json!({
                    "type": "widget_ack",
                    "ok": true,
                    "widgetKind": widget_kind,
                    "action": action,
                }),
            );
        }
    }
}

fn find_active_widget_data(state: &AppState, widget_kind: &str, payload: &Value) -> Option<Value> {
    let target_id = payload
        .get("widgetId")
        .or_else(|| payload.get("widget_id"))
        .and_then(Value::as_str);
    let thread = state.active_thread.borrow();
    for turn in thread.turns.iter().rev() {
        for w in turn.widgets.iter().rev() {
            if w.kind == widget_kind && (target_id.is_none() || target_id == Some(w.id.as_str())) {
                return Some(w.data.clone());
            }
        }
    }
    None
}

fn start_live_stream(
    state: &Rc<AppState>,
    webview: &WebView,
    prompt: String,
    profile: Option<String>,
    explicit_reply_to: Option<u64>,
    emit_optimistic_card: bool,
) {
    if emit_optimistic_card {
        dispatch_to_js(
            webview,
            &json!({
                "type": "stream_start",
                "optimistic": true,
                "prompt": prompt,
                "profile": profile.as_deref().unwrap_or("default"),
                "createdAt": store::format_local_timestamp(store::unix_timestamp_ms()),
            }),
        );
    }

    let Some(socket_path) = state.socket.clone() else {
        let msg = "No daemon socket path available (set --socket or XDG_RUNTIME_DIR)";
        dispatch_to_js(webview, &json!({ "type": "stream_error", "message": msg }));
        dispatch_to_js(
            webview,
            &json!({ "type": "toast", "kind": "error", "message": msg }),
        );
        return;
    };

    let reply_to = explicit_reply_to.or_else(|| state.latest_turn_id.get());
    let wv = webview.clone();
    let state_clone = Rc::clone(state);
    let prompt_for_ui = prompt.clone();

    let seq = register_stream_handler(move |ev| match ev {
        StreamUiEvent::Started { id } => {
            dispatch_to_js(
                &wv,
                &json!({
                    "type": "stream_start",
                    "id": id,
                    "turnId": id,
                    "prompt": prompt_for_ui,
                    "createdAt": store::format_local_timestamp(store::unix_timestamp_ms()),
                }),
            );
        }
        StreamUiEvent::Progress { id, status } => {
            dispatch_to_js(
                &wv,
                &json!({
                    "type": "stream_progress",
                    "id": id,
                    "turnId": id,
                    "status": status,
                }),
            );
        }
        StreamUiEvent::Chunk {
            id,
            delta,
            markdown,
            html,
        } => {
            dispatch_to_js(
                &wv,
                &json!({
                    "type": "stream_chunk",
                    "id": id,
                    "turnId": id,
                    "chunk": delta,
                    "delta": delta,
                    "markdown": markdown,
                    "html": html,
                }),
            );
        }
        StreamUiEvent::Widget {
            id,
            widget,
            widgets,
        } => {
            dispatch_to_js(
                &wv,
                &json!({
                    "type": "stream_widget",
                    "id": id,
                    "turnId": id,
                    "widget": widget,
                    "widgets": widgets,
                }),
            );
        }
        StreamUiEvent::Reset { id } => {
            dispatch_to_js(
                &wv,
                &json!({
                    "type": "stream_reset",
                    "id": id,
                    "turnId": id,
                }),
            );
        }
        StreamUiEvent::Done {
            id,
            conversation_id,
            markdown,
            html,
            widgets,
            denied,
            profile,
            duration_ms,
        } => {
            state_clone.latest_turn_id.set(Some(id));
            let created_at = store::format_local_timestamp(store::unix_timestamp_ms());
            let word_count = markdown.split_whitespace().count();
            let reading_time = if word_count > 0 {
                format!("{} min read", word_count.div_ceil(210).max(1))
            } else {
                String::new()
            };
            let prof_str = profile.clone().unwrap_or_default();
            let badge = if prof_str.is_empty() {
                "default".to_string()
            } else {
                prof_str.clone()
            };

            state_clone
                .active_thread
                .borrow_mut()
                .turns
                .push(loader::ViewerTurn {
                    id: id.to_string(),
                    turn_id: Some(id),
                    conversation_id: Some(conversation_id),
                    prompt: prompt_for_ui.clone(),
                    hide_prompt: false,
                    badge: badge.clone(),
                    profile: prof_str,
                    lane: "interactive".to_string(),
                    origin: "typed".to_string(),
                    status: "completed".to_string(),
                    error_message: None,
                    fail_reason: None,
                    suppressed_token: None,
                    schedule_id: None,
                    schedule_name: None,
                    created_at: created_at.clone(),
                    duration_ms: Some(duration_ms),
                    reading_time,
                    word_count,
                    denied: denied.clone(),
                    widgets: widgets.clone(),
                    markdown: markdown.clone(),
                    html: html.clone(),
                    is_morning_brief: false,
                    drop_cap_hint: state_clone.cli.drop_cap,
                });

            dispatch_to_js(
                &wv,
                &json!({
                    "type": "stream_done",
                    "id": id,
                    "turnId": id,
                    "conversationId": conversation_id,
                    "prompt": prompt_for_ui,
                    "markdown": markdown,
                    "html": html,
                    "widgets": widgets,
                    "denied": denied,
                    "profile": profile,
                    "badge": badge,
                    "durationMs": duration_ms,
                    "createdAt": created_at,
                }),
            );

            let history = query_history_entries(state_clone.state_dir.as_deref(), "", 30);
            dispatch_to_js(
                &wv,
                &json!({
                    "type": "history_results",
                    "query": "",
                    "results": history,
                    "items": history,
                }),
            );
        }
        StreamUiEvent::Error { id, message } => {
            dispatch_to_js(
                &wv,
                &json!({
                    "type": "stream_error",
                    "id": id,
                    "message": message,
                }),
            );
            dispatch_to_js(
                &wv,
                &json!({
                    "type": "toast",
                    "kind": "error",
                    "message": message,
                }),
            );
        }
    });

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamUiEvent>();
    spawn_ask_stream(socket_path, prompt, profile, reply_to, tx);

    let _ = std::thread::Builder::new()
        .name("hc-viewer-idle-bridge".to_string())
        .spawn(move || {
            while let Some(ev) = rx.blocking_recv() {
                glib::idle_add_once(move || {
                    deliver_stream_event(seq, ev);
                });
            }
        });
}

fn serve_hc_scheme_request(
    request: &URISchemeRequest,
    assets_dir: &Path,
    boot_json: &str,
    force_theme: Option<ForceTheme>,
    fallback_palette: Option<&WallpaperPalette>,
) {
    let raw_path = request
        .path()
        .map(|p| p.to_string())
        .unwrap_or_else(|| "/index.html".to_string());
    let rel = raw_path.trim_start_matches('/');

    if rel.is_empty() || rel == "index.html" {
        let system_dark = detect_system_dark();
        let resolved = if resolve_is_dark(force_theme, system_dark) {
            "dark"
        } else {
            "light"
        };
        let theme_attr = force_theme.map_or("auto", ForceTheme::as_str);
        let live_palette = load_wallpaper_palette();
        let palette = live_palette.as_ref().or(fallback_palette);
        let html = build_shell_html(boot_json, theme_attr, resolved, system_dark, palette);
        finish_bytes(request, html.into_bytes(), "text/html; charset=utf-8");
        return;
    }

    if let Some(asset_path) = safe_join_asset(assets_dir, rel)
        && let Ok(bytes) = std::fs::read(&asset_path)
    {
        finish_bytes(request, bytes, mime_for_path(rel));
        return;
    }

    match rel {
        "viewer.css" => finish_bytes(
            request,
            b":root { color-scheme: light dark; }\n".to_vec(),
            "text/css; charset=utf-8",
        ),
        "widgets.js" | "viewer.js" => finish_bytes(
            request,
            b"/* asset fallback */\n".to_vec(),
            "text/javascript; charset=utf-8",
        ),
        _ => {
            let mut err = glib::Error::new(gio::IOErrorEnum::NotFound, "Asset not found");
            request.finish_error(&mut err);
        }
    }
}

pub fn build_shell_html(
    boot_json: &str,
    theme_attr: &str,
    resolved_theme: &str,
    system_dark: bool,
    palette: Option<&WallpaperPalette>,
) -> String {
    let dark_class = if resolved_theme == "dark" {
        " class=\"theme-dark dark\""
    } else {
        " class=\"theme-light light\""
    };
    let sys_dark_attr = if system_dark { "true" } else { "false" };
    let style_attr = match palette {
        Some(p) => format!(
            " style=\"--primary-color: {}; --secondary-color: {}; --on-primary-color: {};\"",
            p.primary_rgb, p.secondary_rgb, p.on_primary_hex
        ),
        None => String::new(),
    };
    format!(
        "<!doctype html>\n<html lang=\"en\" data-theme=\"{theme_attr}\" data-system-dark=\"{sys_dark_attr}\" data-resolved-theme=\"{resolved_theme}\"{dark_class}{style_attr}>\n<head>\n  <meta charset=\"utf-8\">\n  <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n  <title>Ask Computer</title>\n  <link rel=\"stylesheet\" href=\"hc://app/viewer.css\">\n</head>\n<body>\n  <div id=\"app\"></div>\n  <script>window.__HC_BOOT__ = {boot_json};</script>\n  <script src=\"hc://app/widgets.js\"></script>\n  <script src=\"hc://app/viewer.js\"></script>\n</body>\n</html>\n"
    )
}

fn finish_bytes(request: &URISchemeRequest, bytes: Vec<u8>, content_type: &str) {
    let len = bytes.len() as i64;
    let gbytes = glib::Bytes::from_owned(bytes);
    let stream = gio::MemoryInputStream::from_bytes(&gbytes);
    request.finish(&stream, len, Some(content_type));
}

pub fn safe_join_asset(base: &Path, rel: &str) -> Option<PathBuf> {
    let rel_path = Path::new(rel);
    let mut out = base.to_path_buf();
    for comp in rel_path.components() {
        match comp {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

fn mime_for_path(rel: &str) -> &'static str {
    let ext = Path::new(rel)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => "application/octet-stream",
    }
}

pub fn resolve_assets_dir() -> PathBuf {
    if let Ok(env_dir) = std::env::var("HC_VIEWER_ASSETS_DIR")
        && !env_dir.is_empty()
    {
        let p = PathBuf::from(env_dir);
        if p.is_dir() {
            return p;
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        let installed = exe_dir.join("../share/hey-computer/assets");
        if installed.is_dir() {
            return installed;
        }
    }
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/assets"))
}

fn dispatch_palette_changed(webview: &WebView, pal: &WallpaperPalette) {
    dispatch_to_js(
        webview,
        &json!({
            "type": "palette_changed",
            "primaryHex": pal.primary_hex,
            "secondaryHex": pal.secondary_hex,
            "primaryRgb": pal.primary_rgb,
            "secondaryRgb": pal.secondary_rgb,
            "onPrimaryHex": pal.on_primary_hex,
        }),
    );
}

fn setup_live_theme_watcher(webview: &WebView, state: &Rc<AppState>) {
    let force_theme = state.cli.force_theme;
    let last_dark = Rc::new(Cell::new(detect_system_dark()));
    let last_palette = Rc::new(RefCell::new(state.initial_palette.clone()));
    let debounce_pending = Rc::new(Cell::new(false));

    let sync_theme_and_palette: Rc<dyn Fn()> = {
        let wv = webview.clone();
        let last_dark = Rc::clone(&last_dark);
        let last_palette = Rc::clone(&last_palette);
        Rc::new(move || {
            let now_dark = detect_system_dark();
            if now_dark != last_dark.get() {
                last_dark.set(now_dark);
                apply_webview_bg(&wv, force_theme, resolve_is_dark(force_theme, now_dark));
                dispatch_to_js(
                    &wv,
                    &json!({ "type": "theme_changed", "systemDark": now_dark }),
                );
            }
            if let Some(pal) = load_wallpaper_palette() {
                let changed = last_palette.borrow().as_ref() != Some(&pal);
                if changed {
                    *last_palette.borrow_mut() = Some(pal.clone());
                    dispatch_palette_changed(&wv, &pal);
                }
            }
        })
    };

    if let Some(gtk_settings) = gtk4::Settings::default() {
        let sync_fn = Rc::clone(&sync_theme_and_palette);
        gtk_settings.connect_gtk_application_prefer_dark_theme_notify(move |_| {
            sync_fn();
        });
    }

    if let Some(home) = dirs::home_dir() {
        let theme_dir = home.join(".theme");
        if let Some(monitor) =
            attach_theme_dir_monitor(&theme_dir, &debounce_pending, &sync_theme_and_palette)
        {
            state.theme_monitors.borrow_mut().push(monitor);
        } else {
            let state_weak = Rc::downgrade(state);
            let debounce = Rc::clone(&debounce_pending);
            let sync_fn = Rc::clone(&sync_theme_and_palette);
            glib::timeout_add_local(Duration::from_millis(2500), move || {
                let Some(st) = state_weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                sync_fn();
                if theme_dir.is_dir()
                    && let Some(mon) = attach_theme_dir_monitor(&theme_dir, &debounce, &sync_fn)
                {
                    st.theme_monitors.borrow_mut().push(mon);
                    return glib::ControlFlow::Break;
                }
                glib::ControlFlow::Continue
            });
        }
    }
}

fn attach_theme_dir_monitor(
    theme_dir: &Path,
    debounce_pending: &Rc<Cell<bool>>,
    sync_fn: &Rc<dyn Fn()>,
) -> Option<gio::FileMonitor> {
    if !theme_dir.is_dir() {
        return None;
    }
    let dir_file = gio::File::for_path(theme_dir);
    let monitor = dir_file
        .monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
        .ok()?;
    let pending = Rc::clone(debounce_pending);
    let sync_cb = Rc::clone(sync_fn);
    monitor.connect_changed(move |_mon, file, other_file, _event| {
        let relevant = [Some(file), other_file].into_iter().flatten().any(|f| {
            f.basename()
                .and_then(|n| n.into_os_string().into_string().ok())
                .is_some_and(|name| {
                    name == "palette.json" || name == "mode" || name.starts_with('.')
                })
        });
        if !relevant || pending.get() {
            return;
        }
        pending.set(true);
        let pending_inner = Rc::clone(&pending);
        let sync_inner = Rc::clone(&sync_cb);
        glib::timeout_add_local_once(Duration::from_millis(60), move || {
            pending_inner.set(false);
            sync_inner();
        });
    });
    Some(monitor)
}

fn resolve_is_dark(force_theme: Option<ForceTheme>, system_dark: bool) -> bool {
    match force_theme {
        Some(ForceTheme::Dark) => true,
        Some(ForceTheme::Light) => false,
        None => system_dark,
    }
}

fn apply_webview_bg(webview: &WebView, force_theme: Option<ForceTheme>, is_dark: bool) {
    let use_dark = resolve_is_dark(force_theme, is_dark);
    let bg = if use_dark {
        // Flexoki Black (`#100F0F`)
        gdk::RGBA::new(16.0 / 255.0, 15.0 / 255.0, 15.0 / 255.0, 1.0)
    } else {
        // Flexoki Paper (`#F2F0E5`)
        gdk::RGBA::new(242.0 / 255.0, 240.0 / 255.0, 229.0 / 255.0, 1.0)
    };
    webview.set_background_color(&bg);
}

fn capture_snapshot_and_quit(webview: &WebView, out_path: &Path, app: &gtk4::Application) {
    let out = out_path.to_path_buf();
    let app_clone = app.clone();
    let wv_fallback = webview.clone();

    webview.snapshot(
        SnapshotRegion::Visible,
        SnapshotOptions::NONE,
        gio::Cancellable::NONE,
        move |res| match res {
            Ok(texture) => {
                save_texture_and_quit(&texture, &out, &app_clone);
            }
            Err(_) => {
                let out2 = out.clone();
                let app2 = app_clone.clone();
                wv_fallback.snapshot(
                    SnapshotRegion::FullDocument,
                    SnapshotOptions::NONE,
                    gio::Cancellable::NONE,
                    move |res2| match res2 {
                        Ok(texture) => save_texture_and_quit(&texture, &out2, &app2),
                        Err(err) => {
                            eprintln!("hey-computer-viewer: snapshot failed: {err}");
                            std::process::exit(1);
                        }
                    },
                );
            }
        },
    );
}

fn save_texture_and_quit(texture: &gdk::Texture, out_path: &Path, app: &gtk4::Application) {
    if let Some(parent) = out_path.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(err) = texture.save_to_png(out_path) {
        eprintln!(
            "hey-computer-viewer: failed to write PNG {}: {err}",
            out_path.display()
        );
        std::process::exit(1);
    }
    app.quit();
}

pub fn dispatch_to_js(webview: &WebView, payload: &Value) {
    if let Ok(serialized) = serde_json::to_string(payload) {
        let safe = escape_json_for_inline_script(&serialized);
        let script = format!("window.__hcDispatch && window.__hcDispatch({safe});");
        webview.evaluate_javascript(&script, None, None, gio::Cancellable::NONE, |_| {});
    }
}

pub fn escape_json_for_inline_script(json_str: &str) -> String {
    json_str
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

#[cfg(test)]
mod tests {
    use super::actions::{
        DistillHunk, apply_distill_hunks, approve_distill_changes, execute_snippet_with_timeout,
        insert_journal_task_prepend, plan_openhue_commands, sanitize_skill_slug, save_skill_files,
        save_to_vault, toggle_journal_task_line,
    };
    use super::loader::{
        is_morning_brief_turn, load_schedule_detail, load_schedule_summaries,
        parse_answer_filename_turn_id, parse_numeric_turn_arg, query_history_entries,
        resolve_initial_thread,
    };
    use super::palette::parse_palette_json;
    use super::stream::normalize_ask_profile;
    use super::{
        Cli, ForceTheme, build_shell_html, escape_json_for_inline_script, parse_optional_u64,
        safe_join_asset, smartcase_find_options,
    };
    use clap::Parser;
    use hey_computer::config::{Config, History as HistoryConfig};
    use hey_computer::protocol::{FailReason, WidgetSpec};
    use hey_computer::store::{
        FailureDeliver, Lane, NewSchedule, NewTurn, Store, TurnOrigin, TurnStatus,
    };
    use serde_json::json;
    use std::collections::HashSet;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use webkit6::FindOptions;

    static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(1);

    fn temp_test_dir(tag: &str) -> PathBuf {
        let n = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("hc-viewer-test-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp test dir");
        dir
    }

    #[test]
    fn cli_parses_all_production_flags() {
        let cli = Cli::try_parse_from([
            "hey-computer-viewer",
            "/tmp/answer-1764060000000-42.md",
            "--config",
            "/tmp/config.toml",
            "--socket",
            "/tmp/hc.sock",
            "--state-dir",
            "/tmp/state",
            "--turn-id",
            "42",
            "--schedules",
            "--schedule",
            "morning-digest",
            "--history",
            "--recall",
            "transit",
            "--drop-cap",
            "--focus-composer",
            "--force-theme",
            "dark",
            "--snapshot",
            "/tmp/out.png",
            "--width",
            "1200",
            "--height",
            "900",
            "--name",
            "hey-computer.custom",
        ])
        .expect("parse production viewer CLI");

        assert_eq!(
            cli.file.as_deref(),
            Some(Path::new("/tmp/answer-1764060000000-42.md"))
        );
        assert_eq!(cli.turn_id, Some(42));
        assert!(cli.schedules);
        assert_eq!(cli.schedule.as_deref(), Some("morning-digest"));
        assert!(cli.history);
        assert_eq!(cli.recall.as_deref(), Some("transit"));
        assert!(cli.drop_cap);
        assert!(cli.focus_composer);
        assert_eq!(cli.force_theme, Some(ForceTheme::Dark));
        assert_eq!(cli.width, 1200);
        assert_eq!(cli.height, 900);
        assert_eq!(cli.name, "hey-computer.custom");
    }

    #[test]
    fn smartcase_find_options_respects_uppercase() {
        let lower = smartcase_find_options("morning brief");
        assert!(lower.contains(FindOptions::WRAP_AROUND));
        assert!(lower.contains(FindOptions::CASE_INSENSITIVE));

        let mixed = smartcase_find_options("Morning");
        assert!(mixed.contains(FindOptions::WRAP_AROUND));
        assert!(!mixed.contains(FindOptions::CASE_INSENSITIVE));
    }

    #[test]
    fn numeric_turn_arg_parses_nonexistent_ids() {
        assert_eq!(parse_numeric_turn_arg(Path::new("42")), Some(42));
        assert_eq!(parse_numeric_turn_arg(Path::new("#42")), Some(42));
        assert_eq!(parse_numeric_turn_arg(Path::new("0")), None);
        assert_eq!(parse_numeric_turn_arg(Path::new("notes.md")), None);
    }

    #[test]
    fn blocks_parent_traversal_and_escapes_script_tags() {
        let base = Path::new("/tmp/assets");
        assert_eq!(
            safe_join_asset(base, "viewer.css").as_deref(),
            Some(Path::new("/tmp/assets/viewer.css"))
        );
        assert!(safe_join_asset(base, "../Cargo.toml").is_none());
        assert!(safe_join_asset(base, "fonts/../../secret").is_none());
        assert!(safe_join_asset(base, "/etc/passwd").is_none());

        let raw = "{\"html\":\"</script><script>alert(1)</script>\"}";
        let escaped = escape_json_for_inline_script(raw);
        assert!(!escaped.contains("</script>"));
        assert!(escaped.contains("\\u003c/script\\u003e"));
    }

    #[test]
    fn parses_answer_filename_turn_ids() {
        assert_eq!(
            parse_answer_filename_turn_id(Path::new("/tmp/answer-1764060000123-77.md")),
            Some(77)
        );
        assert_eq!(
            parse_answer_filename_turn_id(Path::new("answer-999-1.md")),
            Some(1)
        );
        assert_eq!(
            parse_answer_filename_turn_id(Path::new("answer-999-0.md")),
            None
        );
        assert_eq!(
            parse_answer_filename_turn_id(Path::new("answer-bad-12.md")),
            None
        );
        assert_eq!(parse_answer_filename_turn_id(Path::new("notes.md")), None);
    }

    #[test]
    fn detects_morning_brief_drop_cap_turns() {
        assert!(is_morning_brief_turn(
            TurnOrigin::Schedule,
            Some(3),
            "Collect overnight alerts",
            "Overnight Check"
        ));
        assert!(is_morning_brief_turn(
            TurnOrigin::ScheduleScript,
            None,
            "run collector",
            "Collector"
        ));
        assert!(is_morning_brief_turn(
            TurnOrigin::Cli,
            None,
            "Give me my Morning Brief and transit options",
            "Agenda"
        ));
        assert!(is_morning_brief_turn(
            TurnOrigin::Voice,
            None,
            "Run morning-digest",
            "Digest"
        ));
        assert!(!is_morning_brief_turn(
            TurnOrigin::Typed,
            None,
            "Explain Rust lifetimes",
            "Rust Lifetimes"
        ));
    }

    #[test]
    fn parses_wallpaper_palette_rgb_hex_fallback_and_wcag_contrast() {
        let dark_primary = r##"{
            "primary_hex": "#597805",
            "secondary_hex": "#d89d3f",
            "primary_rgb": "89, 120, 5",
            "secondary_rgb": "216, 157, 63"
        }"##;
        let pal = parse_palette_json(dark_primary).expect("valid dark palette");
        assert_eq!(pal.primary_rgb, "89, 120, 5");
        assert_eq!(pal.secondary_rgb, "216, 157, 63");
        assert_eq!(pal.on_primary_hex, "#FFFCF0");

        let html = build_shell_html("{}", "auto", "dark", true, Some(&pal));
        assert!(html.contains(
            "style=\"--primary-color: 89, 120, 5; --secondary-color: 216, 157, 63; --on-primary-color: #FFFCF0;\""
        ));

        let bright_hex_only = r##"{
            "primary_hex": "#F2D24B",
            "secondary_hex": "#3AA99F"
        }"##;
        let bright = parse_palette_json(bright_hex_only).expect("hex fallback palette");
        assert_eq!(bright.primary_rgb, "242, 210, 75");
        assert_eq!(bright.on_primary_hex, "#100F0F");
        assert!(parse_palette_json("{\"primary_rgb\":\"999, 0, 1\"}").is_none());
    }

    #[test]
    fn prepends_and_toggles_journal_tasks_in_place() {
        let existing_with_tasks = "# 2026-11-25\n\n## Tasks\n- [x] Done item\n- [ ] Second item\n\n## Notes\nMet at noon.\n";
        let updated = insert_journal_task_prepend(existing_with_tasks, "New top task");
        assert_eq!(
            updated,
            "# 2026-11-25\n\n## Tasks\n- [ ] New top task\n- [x] Done item\n- [ ] Second item\n\n## Notes\nMet at noon.\n"
        );

        let heading_only = "# 2026-11-25\n\nQuiet morning notes.\n";
        let after_heading = insert_journal_task_prepend(heading_only, "- [ ] First task");
        assert_eq!(
            after_heading,
            "# 2026-11-25\n\n- [ ] First task\n\nQuiet morning notes.\n"
        );

        let no_heading = "Plain note paragraph.\n";
        let at_top = insert_journal_task_prepend(no_heading, "Top task");
        assert_eq!(at_top, "- [ ] Top task\n\nPlain note paragraph.\n");

        let with_tag = "# Journal\n\n- [ ] Send summary #studio\n";
        let (checked, is_done) =
            toggle_journal_task_line(with_tag, "Send summary", None).expect("toggle on");
        assert!(is_done);
        assert_eq!(checked, "# Journal\n\n- [x] Send summary #studio\n");

        let (unchecked, is_done2) =
            toggle_journal_task_line(&checked, "Send summary", None).expect("toggle off");
        assert!(!is_done2);
        assert_eq!(unchecked, "# Journal\n\n- [ ] Send summary #studio\n");

        let dir = temp_test_dir("vault");
        let out = save_to_vault(Some(&dir), "prepend_task", "Journal", "Review MR")
            .expect("save task to vault");
        assert!(out.saved);
        assert!(out.path.ends_with(&format!(
            "Journal/{}.md",
            hey_computer::store::local_today_ymd()
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saves_widget_skill_template_with_private_modes_and_prefix() {
        assert_eq!(
            sanitize_skill_slug("../Focus Timer!!", true),
            "widget-focus-timer"
        );
        assert_eq!(
            sanitize_skill_slug("widget-latency-card", true),
            "widget-latency-card"
        );
        assert_eq!(sanitize_skill_slug("ci-log-triage", false), "ci-log-triage");

        let dir = temp_test_dir("skills");
        let skills_root = dir.join("skills");
        let out = save_skill_files(
            Some(&skills_root),
            "pomodoro-timer",
            "---\nname: widget-pomodoro-timer\n---\n# Timer",
            "<div>{{mins}}</div>",
            true,
        )
        .expect("save widget template");
        assert!(out.saved);
        assert_eq!(out.skill_name, "widget-pomodoro-timer");

        let skill_dir = skills_root.join("widget-pomodoro-timer");
        let dir_mode = std::fs::metadata(&skill_dir).unwrap().permissions().mode() & 0o777;
        let md_mode = std::fs::metadata(skill_dir.join("SKILL.md"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let html_mode = std::fs::metadata(skill_dir.join("template.html"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(md_mode, 0o600);
        assert_eq!(html_mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn applies_distill_hunks_enforces_vault_boundary_and_memory_budget() {
        let dir = temp_test_dir("distill");
        let vault_dir = dir.join("vault");
        let state_dir = dir.join("state");
        let skills_dir = dir.join("skills");
        std::fs::create_dir_all(&vault_dir).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();

        let base_mem = dir.join("base-memory.md");
        std::fs::write(&base_mem, "## Base\n- Static NixOS preference.\n").unwrap();

        let target_mem = state_dir.join("memory.md");
        std::fs::write(
            &target_mem,
            "## Commute\n- Old Thursday check.\n\n## Temp\n- Expired bridge note.\n",
        )
        .unwrap();

        let mut cfg = Config {
            state_dir: state_dir.clone(),
            ..Config::default()
        };
        cfg.instructions.vault = vault_dir.clone();
        cfg.instructions.memory_file = base_mem.clone();
        cfg.instructions.memory_files = vec![target_mem.clone()];
        cfg.instructions.memory_max_chars = 500;

        let widget_data = json!({
            "targetFile": target_mem.display().to_string(),
            "memoryHunks": [
                {
                    "id": "h1",
                    "kind": "add",
                    "section": "## Preferences",
                    "before": null,
                    "after": "- Prefer Reforma 1969 serif prose."
                },
                {
                    "id": "h2",
                    "kind": "update",
                    "section": "## Commute",
                    "before": "- Old Thursday check.",
                    "after": "- Wednesday 12:00 check via U3."
                },
                {
                    "id": "h3",
                    "kind": "prune",
                    "section": "## Temp",
                    "before": "- Expired bridge note.",
                    "after": null
                }
            ],
            "skillDraft": {
                "name": "ci-log-triage",
                "path": "skills/ci-log-triage/SKILL.md",
                "previewMarkdown": "---\nname: ci-log-triage\n---\n# CI Log Triage\n"
            }
        });

        let payload = json!({
            "targetFile": target_mem.display().to_string(),
            "acceptedHunks": ["h1", "h2", "h3"],
            "includeSkill": true
        });

        let res = approve_distill_changes(
            &cfg,
            Some(&state_dir),
            Some(&skills_dir),
            &payload,
            Some(&widget_data),
        )
        .expect("approve distill changes");
        assert_eq!(res.applied_hunks, 3);
        let written = std::fs::read_to_string(&target_mem).unwrap();
        assert!(written.contains("- Wednesday 12:00 check via U3."));
        assert!(written.contains("- Prefer Reforma 1969 serif prose."));
        assert!(!written.contains("Old Thursday check"));
        assert!(!written.contains("Expired bridge note"));
        let mem_mode = std::fs::metadata(&target_mem).unwrap().permissions().mode() & 0o777;
        assert_eq!(mem_mode, 0o600);
        assert!(skills_dir.join("ci-log-triage/SKILL.md").is_file());

        let vault_target = vault_dir.join("memory.md");
        let bad_vault = approve_distill_changes(
            &cfg,
            Some(&state_dir),
            Some(&skills_dir),
            &json!({
                "targetFile": vault_target.display().to_string(),
                "acceptedHunks": ["h1"]
            }),
            Some(&widget_data),
        );
        assert!(bad_vault.is_err());

        let sneaky_dir = dir.join("sneaky-link");
        std::os::unix::fs::symlink(&vault_dir, &sneaky_dir).unwrap();
        let symlink_escape = approve_distill_changes(
            &cfg,
            Some(&state_dir),
            Some(&skills_dir),
            &json!({
                "targetFile": sneaky_dir.join("memory.md").display().to_string(),
                "acceptedHunks": ["h1"]
            }),
            Some(&widget_data),
        );
        assert!(symlink_escape.is_err());

        cfg.instructions.memory_max_chars = 60;
        let over_budget = approve_distill_changes(
            &cfg,
            Some(&state_dir),
            Some(&skills_dir),
            &json!({
                "targetFile": target_mem.display().to_string(),
                "hunks": [DistillHunk {
                    id: "big".to_string(),
                    kind: "add".to_string(),
                    section: "## Extra".to_string(),
                    before: None,
                    after: Some("- ".to_string() + &"x".repeat(80)),
                }]
            }),
            None,
        );
        assert!(over_budget.is_err());

        let _ = apply_distill_hunks("", &[]).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runs_code_languages_and_plans_openhue_commands() {
        let sh_out = execute_snippet_with_timeout("printf 'ok-sh'", "sh", Duration::from_secs(5));
        assert_eq!(sh_out.exit_code, 0);
        assert_eq!(sh_out.stdout, "ok-sh");

        let bash_out = execute_snippet_with_timeout(
            "printf '%s' \"${BASH_VERSION:+bash}\"",
            "bash",
            Duration::from_secs(5),
        );
        assert_eq!(bash_out.exit_code, 0);
        assert_eq!(bash_out.stdout, "bash");

        let timed_out = execute_snippet_with_timeout("sleep 10", "sh", Duration::from_millis(120));
        assert!(timed_out.timed_out);
        assert_eq!(timed_out.exit_code, 124);
        assert!(timed_out.duration_ms < 3_000);

        let plan_b = plan_openhue_commands(
            "set_brightness",
            Some(&json!({ "room": "Study", "on": true, "brightness": 75 })),
        )
        .unwrap();
        assert_eq!(
            plan_b.commands,
            vec![vec![
                "openhue",
                "set",
                "room",
                "Study",
                "--on",
                "--brightness",
                "75"
            ]]
        );

        let plan_t = plan_openhue_commands(
            "set_temp",
            Some(&json!({ "room": "Study", "tempMirek": 250 })),
        )
        .unwrap();
        assert_eq!(
            plan_t.commands,
            vec![vec!["openhue", "set", "room", "Study", "--on", "-t", "250"]]
        );

        let plan_s = plan_openhue_commands(
            "activate_scene",
            Some(&json!({ "room": "Study", "scene": "Focus" })),
        )
        .unwrap();
        assert_eq!(
            plan_s.commands,
            vec![vec!["openhue", "set", "scene", "--room", "Study", "Focus"]]
        );

        assert!(plan_openhue_commands("toggle_room", Some(&json!({ "room": "--evil" }))).is_err());
    }

    #[tokio::test]
    async fn loads_threads_schedules_and_recall_from_store_v4() {
        let dir = temp_test_dir("store");
        let (store, _) = Store::open(&dir, &HistoryConfig::default()).expect("open store");
        let sched = hey_computer::store::create_schedule(
            &dir,
            &NewSchedule {
                name: "morning-digest".to_string(),
                schedule_spec: "daily 07:30".to_string(),
                prompt: "Write the editorial morning brief.".to_string(),
                profile: "deep".to_string(),
                script: "hey-computer-proactive collect-morning-digest".to_string(),
                no_agent: false,
                suppress_token: Some("HEARTBEAT_OK".to_string()),
                failure_deliver: FailureDeliver::Notify,
                max_runs: None,
                expires_at: None,
                next_run_at: 1_764_140_000_000,
            },
            32,
        )
        .expect("create morning-digest schedule");

        store.create_turn(NewTurn {
            id: 1,
            lane: Lane::Interactive,
            status: TurnStatus::Queued,
            profile: "deep".to_string(),
            created_at_ms: 1_764_060_000_000,
            request: "Morning brief for Harbor Quay".to_string(),
            attachments: Vec::new(),
            origin: TurnOrigin::Schedule,
            schedule_id: Some(sched.id),
        });
        store.mark_running(
            1,
            Lane::Interactive,
            "s1".to_string(),
            Some(1),
            "deep".to_string(),
            1_764_060_000_100,
        );
        store.finish_completed_with_widgets(
            1,
            "# Morning Brief — Wednesday\n\nTake the **U3** at `11:26`.".to_string(),
            vec!["sudo reboot".to_string()],
            vec![WidgetSpec {
                id: "w-transit".to_string(),
                kind: "transit_board".to_string(),
                title: "Live Departures".to_string(),
                data: json!({ "origin": "Westpark", "destination": "Harbor Quay", "routes": [] }),
            }],
            1_764_060_001_600,
        );

        store.create_turn(NewTurn {
            id: 2,
            lane: Lane::Background,
            status: TurnStatus::Queued,
            profile: "fast".to_string(),
            created_at_ms: 1_764_070_000_000,
            request: "Scheduled silent check".to_string(),
            attachments: Vec::new(),
            origin: TurnOrigin::Schedule,
            schedule_id: Some(sched.id),
        });
        store.finish_suppressed(2, "HEARTBEAT_OK".to_string(), 1_764_070_000_500);

        store.create_turn(NewTurn {
            id: 3,
            lane: Lane::Background,
            status: TurnStatus::Queued,
            profile: "deep".to_string(),
            created_at_ms: 1_764_080_000_000,
            request: "Scheduled failing check".to_string(),
            attachments: Vec::new(),
            origin: TurnOrigin::Schedule,
            schedule_id: Some(sched.id),
        });
        store.finish_failed(
            3,
            FailReason::Timeout,
            "Agent timed out after 120s".to_string(),
            1_764_080_010_000,
        );

        store.flush().await;
        store.shutdown().await;

        let answer_file = dir.join("answer-1764060001600-1.md");
        std::fs::write(&answer_file, "fallback markdown").unwrap();

        let (thread, latest) = resolve_initial_thread(Some(&dir), None, Some(&answer_file), false)
            .expect("resolve answer-<ms>-<id>.md from DB");
        assert_eq!(latest, Some(1));
        assert_eq!(thread.turns.len(), 1);
        assert!(thread.turns[0].is_morning_brief);
        assert_eq!(
            thread.turns[0].schedule_name.as_deref(),
            Some("morning-digest")
        );
        assert_eq!(thread.turns[0].status, "completed");
        assert_eq!(thread.turns[0].duration_ms, Some(1500));
        assert_eq!(thread.turns[0].denied, vec!["sudo reboot"]);
        assert_eq!(thread.turns[0].widgets.len(), 1);
        assert!(thread.turns[0].html.contains("<strong>U3</strong>"));

        // Numeric turn ID fallback (`#3` -> failed turn with error metadata).
        let (failed_thread, failed_latest) =
            resolve_initial_thread(Some(&dir), None, Some(Path::new("#3")), false)
                .expect("resolve numeric #3 from DB");
        assert_eq!(failed_latest, Some(3));
        assert_eq!(failed_thread.turns[0].status, "failed");
        assert_eq!(
            failed_thread.turns[0].error_message.as_deref(),
            Some("Agent timed out after 120s")
        );

        // Schedule overview + inspector loading.
        let summaries = load_schedule_summaries(Some(&dir), &HashSet::new());
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].title, "Morning Brief");
        assert_eq!(summaries[0].latest_turn_id, Some(1));

        let detail = load_schedule_detail(Some(&dir), "morning-digest", &HashSet::new(), false)
            .expect("load schedule detail")
            .expect("morning-digest detail present");
        assert_eq!(detail.recent_runs.len(), 3);
        assert_eq!(detail.latest_turn.as_ref().and_then(|t| t.turn_id), Some(1));
        assert!(detail.latest_turn.as_ref().unwrap().is_morning_brief);

        let hits = query_history_entries(Some(&dir), "Harbor U3", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].turn_id, 1);

        assert_eq!(normalize_ask_profile(Some("auto")), None);
        assert_eq!(normalize_ask_profile(Some("deep")).as_deref(), Some("deep"));
        assert_eq!(parse_optional_u64(Some(&json!("19"))), Some(19));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
