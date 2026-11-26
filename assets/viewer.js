/* ==========================================================================
   Ask Computer — Editorial Answer Viewer Controller
   Prose-first layout, 5-step Escape precedence, VIM motions & JumpList,
   hybrid FindController `/` search, on-demand TTS (`s`), Scheduled Actions
   overview & inspector (`S`), inert-hardened History & Schedules drawer,
   live wallpaper palette sync, Info (`I`) metadata toggle, and streaming.
   ========================================================================== */

(function () {
  "use strict";

  const WIDGET_PROVENANCE_FALLBACK = {
    calendar_agenda: ["google-workspace: list_events"],
    transit_board: ["transit: departures"],
    tasks_journal: ["vault: daily_note", "google-workspace: list_tasks"],
    dev_triage: ["linear: list_issues", "gitlab: list_merge_requests"],
    hue_rooms: ["openhue: list_rooms"],
    distill_diff: ["history.db: distill_diff"],
    day_plan: ["day-plan: propose"],
    custom_html: ["mcp-ui: show_widget"],
    show_widget: ["mcp-ui: show_widget"],
    skill_template_draft: ["mcp-ui: propose_widget_template"]
  };

  const VALID_THEMES = ["auto", "light", "dark"];
  const NON_TEXT_INPUT_TYPES = {
    checkbox: true,
    radio: true,
    range: true,
    button: true,
    submit: true,
    reset: true,
    color: true,
    file: true,
    image: true,
    hidden: true
  };
  const MAX_JUMP_LIST = 100;
  const SCROLL_LERP_MS = 90;

  function el(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  }

  function escapeHtml(str) {
    return String(str || "")
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
  }

  function localTodayJournalPath() {
    const now = new Date();
    const y = String(now.getFullYear());
    const m = String(now.getMonth() + 1).padStart(2, "0");
    const d = String(now.getDate()).padStart(2, "0");
    return "Journal/" + y + "-" + m + "-" + d + ".md";
  }

  function hexToRgbTriplet(hex) {
    const clean = String(hex || "").trim().replace(/^#/, "");
    if (clean.length === 3) {
      const r = parseInt(clean[0] + clean[0], 16);
      const g = parseInt(clean[1] + clean[1], 16);
      const b = parseInt(clean[2] + clean[2], 16);
      if (!Number.isNaN(r) && !Number.isNaN(g) && !Number.isNaN(b)) {
        return r + ", " + g + ", " + b;
      }
    }
    if (clean.length === 6) {
      const r = parseInt(clean.slice(0, 2), 16);
      const g = parseInt(clean.slice(2, 4), 16);
      const b = parseInt(clean.slice(4, 6), 16);
      if (!Number.isNaN(r) && !Number.isNaN(g) && !Number.isNaN(b)) {
        return r + ", " + g + ", " + b;
      }
    }
    return null;
  }

  function renderFallbackMarkdown(md) {
    const lines = String(md || "").split("\n");
    const out = [];
    let inCode = false;
    let codeBuf = [];
    let codeLang = "";

    function inlineFormat(s) {
      return escapeHtml(s)
        .replace(/`([^`]+)`/g, "<code>$1</code>")
        .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
        .replace(/\*([^*]+)\*/g, "<em>$1</em>");
    }

    for (let i = 0; i < lines.length; i += 1) {
      const line = lines[i];
      if (line.startsWith("```")) {
        if (!inCode) {
          inCode = true;
          codeLang = line.slice(3).trim();
          codeBuf = [];
        } else {
          const highlighted = codeBuf
            .map(function (l) {
              return '<span class="line">' + escapeHtml(l) + "</span>";
            })
            .join("\n");
          out.push(
            '<div class="code-block" data-lang="' +
              escapeHtml(codeLang || "text") +
              '" data-runnable="false"><pre class="shiki-flexoki"><code>' +
              highlighted +
              "</code></pre></div>"
          );
          inCode = false;
        }
        continue;
      }
      if (inCode) {
        codeBuf.push(line);
        continue;
      }
      if (!line.trim()) continue;
      if (line.startsWith("### ")) {
        out.push("<h3>" + inlineFormat(line.slice(4)) + "</h3>");
      } else if (line.startsWith("## ")) {
        out.push("<h2>" + inlineFormat(line.slice(3)) + "</h2>");
      } else if (line.startsWith("# ")) {
        out.push("<h1>" + inlineFormat(line.slice(2)) + "</h1>");
      } else if (/^[-*]\s+/.test(line)) {
        out.push("<ul><li>" + inlineFormat(line.replace(/^[-*]\s+/, "")) + "</li></ul>");
      } else {
        out.push("<p>" + inlineFormat(line) + "</p>");
      }
    }
    return '<div class="markdown-content">' + out.join("\n") + "</div>";
  }

  function ensureMarkdownWrapper(html, fallbackMd) {
    const trimmed = String(html || "").trim();
    if (!trimmed && fallbackMd) {
      return renderFallbackMarkdown(fallbackMd);
    }
    if (!trimmed) {
      return '<div class="markdown-content"></div>';
    }
    if (trimmed.indexOf("markdown-content") !== -1) {
      return trimmed;
    }
    return '<div class="markdown-content">' + trimmed + "</div>";
  }

  /* ------------------------------------------------------------------------
     Host Bridge Invocation Helpers
     ------------------------------------------------------------------------ */
  const rawHost = window.heyComputer || null;
  const prevDispatch =
    typeof window.__hcDispatch === "function" ? window.__hcDispatch : null;
  const nativeSendPrompt =
    rawHost && typeof rawHost.sendPrompt === "function"
      ? rawHost.sendPrompt.bind(rawHost)
      : null;

  function hostPost(msg) {
    if (rawHost && typeof rawHost.post === "function") {
      rawHost.post(msg);
      return true;
    }
    if (
      window.webkit &&
      window.webkit.messageHandlers &&
      window.webkit.messageHandlers.hc &&
      typeof window.webkit.messageHandlers.hc.postMessage === "function"
    ) {
      window.webkit.messageHandlers.hc.postMessage(JSON.stringify(msg));
      return true;
    }
    return false;
  }

  function hostCopy(text, label) {
    if (!text) {
      showToast("Nothing to copy yet", "warn");
      return;
    }
    if (rawHost && typeof rawHost.copy === "function") {
      rawHost.copy({ text: text, label: label || "Copied to clipboard" });
      return;
    }
    if (hostPost({ type: "copy", text: text, label: label || "Copied to clipboard" })) {
      return;
    }
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).catch(function () {});
    }
    showToast(label || "Copied to clipboard", "ok");
  }

  function hostRunCode(runKey, numericIndex, lang, code) {
    if (rawHost && typeof rawHost.runCode === "function") {
      rawHost.runCode({
        code: code,
        lang: lang,
        codeIndex: numericIndex,
        blockId: String(runKey)
      });
      return;
    }
    if (
      hostPost({
        type: "run_code",
        code: code,
        lang: lang,
        codeIndex: numericIndex,
        blockId: String(runKey)
      })
    ) {
      return;
    }
    window.__hcDispatch({
      type: "code_run_finished",
      blockId: String(runKey),
      codeIndex: numericIndex,
      exitCode: 1,
      durationMs: 0,
      stdout: "",
      stderr: "Host bridge unavailable."
    });
  }

  function hostSaveVault(markdown, mode, title) {
    if (!markdown || !markdown.trim()) {
      showToast("Nothing to save yet", "warn");
      return;
    }
    if (rawHost && typeof rawHost.saveToVault === "function") {
      rawHost.saveToVault({
        markdown: markdown,
        mode: mode || "append",
        title: title || ""
      });
      return;
    }
    if (
      hostPost({
        type: "save_vault",
        markdown: markdown,
        mode: mode || "append",
        title: title || ""
      })
    ) {
      return;
    }
    window.__hcDispatch({
      type: "vault_saved",
      saved: false,
      previewOnly: true,
      path: "<vault>/" + state.todayJournalPath,
      preview: "## Ask Computer · " + (title || "Note") + "\n\n" + markdown
    });
  }

  function hostSearchHistory(query) {
    if (rawHost && typeof rawHost.searchHistory === "function") {
      rawHost.searchHistory({ query: query });
      return true;
    }
    return hostPost({ type: "search_history", query: query });
  }

  function hostOpenHistoryTurn(turnId) {
    if (rawHost && typeof rawHost.openHistoryTurn === "function") {
      rawHost.openHistoryTurn({ turnId: turnId });
      return true;
    }
    return hostPost({ type: "open_history_turn", turnId: turnId });
  }

  function hostFindSearch(query) {
    return hostPost({ type: "find_search", query: String(query || "") });
  }

  function hostFindNext() {
    return hostPost({ type: "find_next" });
  }

  function hostFindPrev() {
    return hostPost({ type: "find_prev" });
  }

  function hostFindClose() {
    return hostPost({ type: "find_close" });
  }

  function hostSpeakText(text) {
    return hostPost({ type: "speak_text", text: String(text || "") });
  }

  function hostStopTts() {
    return hostPost({ type: "stop_tts" });
  }

  function hostSyncActiveThread(thread, latestTurnId, title) {
    return hostPost({
      type: "sync_active_thread",
      thread: thread || null,
      latestTurnId: latestTurnId !== undefined ? latestTurnId : null,
      title: title || ""
    });
  }

  function hostLoadSchedules() {
    return hostPost({ type: "load_schedules" });
  }

  function hostOpenSchedule(target) {
    return hostPost({ type: "open_schedule", target: String(target || "") });
  }

  function hostScheduleAction(action, target) {
    return hostPost({
      type: "schedule_action",
      action: String(action || ""),
      target: String(target || "")
    });
  }

  function hostSaveViewState() {
    return hostPost({
      type: "save_view_state",
      viewMode: state.viewMode || "thread",
      turnId: state.latestTurnId !== undefined ? state.latestTurnId : null,
      scheduleTarget: state.activeScheduleTarget || null,
      scrollY: Math.max(0, Math.round(window.scrollY || 0))
    });
  }

  function hostCloseWindow() {
    if (bootRestoreComplete) {
      hostSaveViewState();
    }
    if (rawHost && typeof rawHost.closeWindow === "function") {
      rawHost.closeWindow();
      return;
    }
    hostPost({ type: "close" });
  }

  /* ------------------------------------------------------------------------
     Boot Data & Palette Normalization
     ------------------------------------------------------------------------ */
  function normalizePalette(rawPalette) {
    if (!rawPalette || typeof rawPalette !== "object") return null;
    const primaryHex = rawPalette.primaryHex || rawPalette.primary_hex || "";
    const secondaryHex = rawPalette.secondaryHex || rawPalette.secondary_hex || "";
    const primaryRgb =
      rawPalette.primaryRgb || rawPalette.primary_rgb || hexToRgbTriplet(primaryHex) || "";
    const secondaryRgb =
      rawPalette.secondaryRgb || rawPalette.secondary_rgb || hexToRgbTriplet(secondaryHex) || "";
    const onPrimaryHex =
      rawPalette.onPrimaryHex || rawPalette.on_primary_hex || "#FFFCF0";
    if (!primaryRgb && !secondaryRgb) return null;
    return {
      primaryHex: primaryHex,
      secondaryHex: secondaryHex,
      primaryRgb: primaryRgb,
      secondaryRgb: secondaryRgb,
      onPrimaryHex: onPrimaryHex
    };
  }

  function normalizeProvenance(turn) {
    const raw =
      turn.toolCalls || turn.tool_calls || turn.tools || turn.provenance || [];
    if (Array.isArray(raw) && raw.length > 0) {
      return raw.map(function (item) {
        if (typeof item === "string") return item;
        if (item && typeof item === "object") {
          const srv = item.server || item.mcp || item.kind || "";
          const tool = item.tool || item.name || item.summary || "";
          return srv && tool ? srv + ": " + tool : (srv || tool || "tool");
        }
        return String(item);
      });
    }
    const crumbs = [];
    const widgets = Array.isArray(turn.widgets) ? turn.widgets : [];
    widgets.forEach(function (w) {
      const mapped = WIDGET_PROVENANCE_FALLBACK[w.kind || w.type] || [];
      mapped.forEach(function (c) {
        if (crumbs.indexOf(c) === -1) crumbs.push(c);
      });
    });
    return crumbs;
  }

  function parsePositiveTurnId(val) {
    if (typeof val === "number" && Number.isFinite(val) && val > 0) {
      return Math.floor(val);
    }
    if (typeof val === "string") {
      const cleaned = val.trim().replace(/^turn-/, "");
      if (/^\d+$/.test(cleaned)) {
        const n = Number(cleaned);
        if (n > 0) return n;
      }
    }
    return null;
  }

  function isMorningBriefHeuristic(turn, threadTitle) {
    if (turn.isMorningBrief || turn.is_morning_brief) return true;
    const origin = String(turn.origin || "").toLowerCase();
    if (origin === "schedule" || origin === "schedule_script") return true;
    if (turn.scheduleId !== undefined && turn.scheduleId !== null) return true;
    const combined =
      String(turn.prompt || turn.request || "") + " " + String(threadTitle || "");
    return /morning[\s-]+(brief|digest)/i.test(combined);
  }

  function isSystemNotificationPrompt(prompt) {
    if (!prompt) return false;
    const trimmed = String(prompt).trimStart();
    return (
      trimmed.startsWith("<notification_task>") ||
      trimmed.startsWith("<notification_triage>") ||
      (trimmed.includes("<untrusted_notification>") &&
        trimmed.includes("</untrusted_notification>"))
    );
  }

  function shouldHideTurnPrompt(turn) {
    if (!turn) return false;
    if (turn.hidePrompt || turn.hide_prompt) return true;
    const origin = String(turn.origin || "").toLowerCase();
    if (
      origin === "schedule" ||
      origin === "schedule_script" ||
      origin === "notification" ||
      (turn.scheduleId !== undefined && turn.scheduleId !== null)
    ) {
      return true;
    }
    return (
      String(turn.lane || "").toLowerCase() === "restricted" &&
      isSystemNotificationPrompt(turn.prompt)
    );
  }

  function normalizeTurn(rawTurn, idx, threadDefaults) {
    const t = rawTurn || {};
    const html =
      t.html || t.renderedHtml || t.rendered_html || t.answerHtml || t.answer_html || "";
    const markdown =
      t.markdown || t.rawMarkdown || t.raw_markdown || t.answer || "";
    const wordCount =
      t.wordCount ||
      t.word_count ||
      (markdown ? markdown.trim().split(/\s+/).filter(Boolean).length : 0);
    const readingTime =
      t.readingTime ||
      t.reading_time ||
      (wordCount > 0 ? Math.max(1, Math.ceil(wordCount / 210)) + " min read" : "");
    const numericTurnId = parsePositiveTurnId(
      t.turnId !== undefined ? t.turnId : (t.turn_id !== undefined ? t.turn_id : t.id)
    );

    return {
      id:
        t.id !== undefined
          ? String(t.id)
          : (numericTurnId !== null
              ? String(numericTurnId)
              : (threadDefaults.id + "-" + (idx + 1))),
      turnId: numericTurnId,
      conversationId: parsePositiveTurnId(t.conversationId || t.conversation_id),
      prompt:
        t.prompt ||
        t.request ||
        t.question ||
        t.userPrompt ||
        threadDefaults.prompt ||
        "",
      hidePrompt: Boolean(t.hidePrompt || t.hide_prompt),
      badge: t.badge || t.profile || threadDefaults.badge || "default",
      profile: t.profile || "",
      lane: t.lane || threadDefaults.lane || "interactive",
      origin: t.origin || "cli",
      status: String(t.status || "completed").toLowerCase(),
      errorMessage: t.errorMessage || t.error_message || null,
      failReason: t.failReason || t.fail_reason || null,
      suppressedToken: t.suppressedToken || t.suppressed_token || null,
      scheduleId: t.scheduleId !== undefined ? t.scheduleId : (t.schedule_id || null),
      scheduleName: t.scheduleName || t.schedule_name || null,
      durationMs:
        typeof t.durationMs === "number"
          ? t.durationMs
          : (typeof t.duration_ms === "number" ? t.duration_ms : null),
      denied: Array.isArray(t.denied) ? t.denied : [],
      readingTime: readingTime,
      createdAt:
        t.createdAt ||
        t.created_at ||
        t.timestamp ||
        threadDefaults.createdAt ||
        "Now",
      widgets: Array.isArray(t.widgets) ? t.widgets : [],
      toolCalls: normalizeProvenance(t),
      html: ensureMarkdownWrapper(html, markdown),
      markdown: markdown,
      isMorningBrief: idx === 0 && isMorningBriefHeuristic(t, threadDefaults.title),
      dropCapHint: Boolean(t.dropCapHint || t.drop_cap_hint || threadDefaults.dropCapHint)
    };
  }

  function normalizeThread(rawThread, fallbackDropCap) {
    const th = rawThread && typeof rawThread === "object" ? rawThread : {};
    const id = th.id || "empty";
    const title = th.title || "Ask Computer";
    const defaults = {
      id: id,
      title: title,
      prompt: th.prompt || "",
      badge: th.badge || th.profile || "default",
      lane: th.lane || "interactive",
      createdAt: th.createdAt || th.created_at || "",
      dropCapHint: Boolean(th.dropCapHint || th.drop_cap_hint || fallbackDropCap)
    };
    const rawTurns = Array.isArray(th.turns) ? th.turns : [];
    return {
      id: id,
      title: title,
      badge: defaults.badge,
      createdAt: defaults.createdAt,
      dropCapHint: defaults.dropCapHint,
      turns: rawTurns.map(function (t, idx) {
        return normalizeTurn(t, idx, defaults);
      })
    };
  }

  function normalizeScheduleDetail(rawDetail, fallbackDropCap) {
    if (!rawDetail || typeof rawDetail !== "object") return null;
    const summary = rawDetail.summary || {};
    const title = summary.title || summary.name || "Schedule";
    let latestTurn = null;
    if (rawDetail.latestTurn && typeof rawDetail.latestTurn === "object") {
      latestTurn = normalizeTurn(rawDetail.latestTurn, 0, {
        id: "sched-" + (summary.id || "0"),
        title: title,
        prompt: "",
        badge: summary.profile || "default",
        lane: "background",
        createdAt: summary.lastRunLocal || "",
        dropCapHint: fallbackDropCap
      });
    }
    return {
      summary: summary,
      prompt: String(rawDetail.prompt || ""),
      script: String(rawDetail.script || ""),
      noAgent: Boolean(rawDetail.noAgent),
      suppressToken: rawDetail.suppressToken || null,
      failureDeliver: String(rawDetail.failureDeliver || "notify"),
      maxRuns: rawDetail.maxRuns !== undefined ? rawDetail.maxRuns : null,
      expiresAtLocal: rawDetail.expiresAtLocal || null,
      recentRuns: Array.isArray(rawDetail.recentRuns) ? rawDetail.recentRuns : [],
      latestTurn: latestTurn
    };
  }

  function normalizeBootPayload(rawBoot) {
    const boot = rawBoot && typeof rawBoot === "object" ? rawBoot : {};
    const bootTheme = boot.forceTheme || boot.theme || "auto";
    const dropCap = Boolean(boot.dropCap || boot.drop_cap || boot.dropCapEnabled);
    const thread = normalizeThread(boot.thread, dropCap);

    let latestTurnId = parsePositiveTurnId(boot.latestTurnId || boot.latest_turn_id);
    if (latestTurnId === null && thread.turns.length > 0) {
      for (let i = thread.turns.length - 1; i >= 0; i -= 1) {
        if (thread.turns[i].turnId !== null) {
          latestTurnId = thread.turns[i].turnId;
          break;
        }
      }
    }

    const initialView = boot.initialView || boot.viewMode || "thread";
    const scheduleDetail = normalizeScheduleDetail(boot.scheduleDetail, dropCap);

    return {
      theme: VALID_THEMES.indexOf(bootTheme) !== -1 ? bootTheme : "auto",
      systemDark:
        typeof boot.systemDark === "boolean"
          ? boot.systemDark
          : (typeof boot.system_dark === "boolean" ? boot.system_dark : null),
      palette: normalizePalette(boot.palette),
      dropCap: dropCap,
      focusComposer: Boolean(boot.focusComposer || boot.focus_composer),
      openHistory: Boolean(boot.openHistory || boot.open_history),
      recallQuery: typeof boot.recallQuery === "string" ? boot.recallQuery : "",
      initialFindQuery: typeof boot.initialFindQuery === "string" ? boot.initialFindQuery : "",
      initialScrollY:
        typeof boot.initialScrollY === "number" &&
        Number.isFinite(boot.initialScrollY) &&
        boot.initialScrollY > 0
          ? Math.round(boot.initialScrollY)
          : 0,
      todayJournalPath: boot.todayJournalPath || localTodayJournalPath(),
      activatedDayPlanDates: Array.isArray(
        boot.activatedDayPlanDates || boot.activated_day_plan_dates
      )
        ? (boot.activatedDayPlanDates || boot.activated_day_plan_dates).slice()
        : [],
      thread: thread,
      latestTurnId: latestTurnId,
      history: Array.isArray(boot.history || boot.recentTurns || boot.recent_turns)
        ? (boot.history || boot.recentTurns || boot.recent_turns)
        : [],
      schedules: Array.isArray(boot.schedules) ? boot.schedules : [],
      scheduleDetail: scheduleDetail,
      viewMode: initialView,
      activeScheduleTarget:
        boot.scheduleTarget ||
        (scheduleDetail && scheduleDetail.summary && scheduleDetail.summary.name) ||
        null,
      speaking: false,
      findQuery: "",
      findActive: false,
      findCount: null,
      findFound: null,
      drawerTab: "history"
    };
  }

  /* ------------------------------------------------------------------------
     Application State & DOM References
     ------------------------------------------------------------------------ */
  const state = normalizeBootPayload(window.__HC_BOOT__);
  window.__HC_ACTIVATED_DAY_PLAN_DATES__ = state.activatedDayPlanDates.slice();
  let dropCapEnabled = state.dropCap;
  let showTurnInfo = false;

  const DEFAULT_COMPOSER_PLACEHOLDER =
    "Ask Computer a follow-up… (i to focus, Ctrl+Enter to send, Esc for Normal)";

  function updateActivatedDayPlanDates(dates) {
    if (Array.isArray(dates)) {
      const merged = new Set(state.activatedDayPlanDates);
      dates.forEach(function (d) {
        if (typeof d === "string" && d.trim()) merged.add(d.trim());
      });
      state.activatedDayPlanDates = Array.from(merged);
      window.__HC_ACTIVATED_DAY_PLAN_DATES__ = state.activatedDayPlanDates.slice();
    }
    if (window.HCWidgets && typeof window.HCWidgets.syncDayPlanStates === "function") {
      window.HCWidgets.syncDayPlanStates(state.activatedDayPlanDates);
    }
  }

  window.HCViewer = {
    focusComposer: function (placeholderHint) {
      if (!dom.composerInput) return;
      if (typeof placeholderHint === "string" && placeholderHint.trim()) {
        dom.composerInput.placeholder = placeholderHint;
      }
      dom.composerInput.focus();
    }
  };

  const codeBlocksById = new Map();
  let lastRunCodeBlock = null;
  let activeStreamRef = null;
  let lastGKeyTime = 0;

  // VIM smooth-lerp scroll animation state
  let scrollAnimRaf = null;
  let scrollAnimTarget = null;
  let scrollAnimStartY = 0;
  let scrollAnimStartT = 0;

  // 100-entry JumpList (`Ctrl-o` / `Ctrl-i`)
  let jumpList = [];
  let jumpIndex = 0;
  let pendingRestoreScrollY = null;
  let bootRestoreComplete = false;
  let userScrolledSinceBoot = false;
  let viewStateSaveTimer = null;

  function scheduleSaveViewState(immediate) {
    if (!bootRestoreComplete) return;
    if (immediate) {
      if (viewStateSaveTimer !== null) {
        clearTimeout(viewStateSaveTimer);
        viewStateSaveTimer = null;
      }
      hostSaveViewState();
      return;
    }
    if (viewStateSaveTimer !== null) {
      clearTimeout(viewStateSaveTimer);
    }
    viewStateSaveTimer = setTimeout(function () {
      viewStateSaveTimer = null;
      hostSaveViewState();
    }, 180);
  }

  // Find bar origin scroll Y (for recording jump point on committed Enter jump)
  let findOriginScrollY = null;

  // Drawer keyboard list selection state
  let drawerSelectedIdx = 0;

  const dom = {};

  function collectThreadMarkdown() {
    if (
      state.viewMode === "schedule_inspector" &&
      state.scheduleDetail &&
      state.scheduleDetail.latestTurn
    ) {
      const lt = state.scheduleDetail.latestTurn;
      if (lt.markdown && lt.markdown.trim()) return lt.markdown.trim();
    }
    const th = state.thread;
    if (!th || !Array.isArray(th.turns) || th.turns.length === 0) return "";
    const parts = [];
    th.turns.forEach(function (t) {
      if (t.markdown && t.markdown.trim()) {
        parts.push(t.markdown.trim());
      } else {
        const tmp = document.createElement("div");
        tmp.innerHTML = t.html || "";
        if (tmp.textContent && tmp.textContent.trim()) {
          parts.push(tmp.textContent.trim());
        }
      }
    });
    return parts.join("\n\n---\n\n");
  }

  function getSelectedText() {
    const sel = window.getSelection ? window.getSelection() : null;
    return sel ? String(sel.toString() || "").trim() : "";
  }

  function getNativeCopySelection() {
    const active = document.activeElement;
    if (
      active &&
      (active.tagName === "TEXTAREA" ||
        (active.tagName === "INPUT" && !NON_TEXT_INPUT_TYPES[String(active.type || "text").toLowerCase()])) &&
      typeof active.selectionStart === "number" &&
      typeof active.selectionEnd === "number" &&
      active.selectionEnd > active.selectionStart
    ) {
      return String(active.value || "").slice(active.selectionStart, active.selectionEnd);
    }
    const sel = window.getSelection ? window.getSelection() : null;
    return sel ? String(sel.toString() || "") : "";
  }

  // Intercept native Ctrl+C / context-menu copies and hand them to the host's
  // persistent clipboard program (`wl-copy`) so closing the viewer on Wayland
  // does not drop clipboard ownership.
  document.addEventListener("copy", function (ev) {
    const selected = getNativeCopySelection();
    if (!selected) return;
    if (hostPost({ type: "copy", text: selected, silent: true })) {
      ev.preventDefault();
    }
  });

  function copySelectionOrMarkdown(forceFull) {
    const selected = forceFull ? "" : getSelectedText();
    if (selected) {
      hostCopy(selected, "Copied selection");
      return;
    }
    hostCopy(collectThreadMarkdown(), "Copied full Markdown answer");
  }

  function collectSpeechText() {
    const selected = getSelectedText();
    if (selected) return selected;

    if (activeStreamRef && activeStreamRef.rawBuffer && activeStreamRef.rawBuffer.trim()) {
      return activeStreamRef.rawBuffer.trim();
    }

    if (state.viewMode === "schedule_inspector" && state.scheduleDetail) {
      const lt = state.scheduleDetail.latestTurn;
      if (lt) {
        if (lt.markdown && lt.markdown.trim()) return lt.markdown.trim();
        const card = dom.readingColumn && dom.readingColumn.querySelector(".turn-card .turn-widgets");
        if (card && card.innerText && card.innerText.trim()) {
          return card.innerText.trim();
        }
      }
      const insp = dom.readingColumn && dom.readingColumn.querySelector(".inspector-card");
      if (insp && insp.innerText && insp.innerText.trim()) {
        return insp.innerText.trim();
      }
      return "";
    }

    if (state.viewMode === "schedules_overview") {
      if (state.schedules.length === 0) return "No scheduled actions configured.";
      return state.schedules
        .map(function (s) {
          return (s.title || s.name) + ": " + (s.status || "active") + ", " + (s.humanSchedule || s.scheduleSpec || "");
        })
        .join(". ");
    }

    const turns = (state.thread && Array.isArray(state.thread.turns)) ? state.thread.turns : [];
    if (turns.length === 0) return "";

    // Pick the currently visible turn card in viewport when a thread has multiple turns.
    let chosenTurn = turns[turns.length - 1];
    let chosenEl = null;
    if (dom.readingColumn) {
      const cards = Array.prototype.slice.call(dom.readingColumn.querySelectorAll(".turn-card"));
      let bestOverlap = -1;
      const viewTop = 56;
      const viewBottom = window.innerHeight - 80;
      cards.forEach(function (card, idx) {
        const rect = card.getBoundingClientRect();
        const overlap = Math.max(0, Math.min(rect.bottom, viewBottom) - Math.max(rect.top, viewTop));
        if (overlap > bestOverlap && idx < turns.length) {
          bestOverlap = overlap;
          chosenTurn = turns[idx];
          chosenEl = card;
        }
      });
    }

    if (chosenTurn && chosenTurn.markdown && chosenTurn.markdown.trim()) {
      return chosenTurn.markdown.trim();
    }
    if (chosenEl) {
      const widgetsNode = chosenEl.querySelector(".turn-widgets");
      if (widgetsNode && widgetsNode.innerText && widgetsNode.innerText.trim()) {
        return widgetsNode.innerText.trim();
      }
      const bodyNode = chosenEl.querySelector(".turn-body");
      if (bodyNode && bodyNode.innerText && bodyNode.innerText.trim()) {
        return bodyNode.innerText.trim();
      }
    }
    return collectThreadMarkdown();
  }

  function setSpeakingState(speaking) {
    state.speaking = Boolean(speaking);
    if (!dom.speakBtn) return;
    dom.speakBtn.classList.toggle("is-speaking", state.speaking);
    dom.speakBtn.innerHTML = "";
    if (state.speaking) {
      dom.speakBtn.appendChild(el("span", "speak-dot"));
      dom.speakBtn.appendChild(el("span", "", "Stop"));
      dom.speakBtn.appendChild(el("span", "kbd-hint", "s"));
      dom.speakBtn.title = "Stop reading aloud (s)";
    } else {
      dom.speakBtn.appendChild(el("span", "", "Speak"));
      dom.speakBtn.appendChild(el("span", "kbd-hint", "s"));
      dom.speakBtn.title = "Read selection or current turn aloud (s)";
    }
  }

  function toggleSpeakCurrent() {
    if (state.speaking) {
      setSpeakingState(false);
      hostStopTts();
      return;
    }
    const text = collectSpeechText();
    if (!text) {
      showToast("Nothing to read aloud", "warn");
      return;
    }
    setSpeakingState(true);
    if (!hostSpeakText(text)) {
      setSpeakingState(false);
      showToast("TTS host bridge unavailable", "warn");
    }
  }

  function stopTtsIfSpeaking() {
    if (state.speaking) {
      setSpeakingState(false);
      hostStopTts();
    }
  }

  function syncComposerBusy(isBusy) {
    if (dom.sendBtn) {
      dom.sendBtn.disabled = Boolean(isBusy);
      dom.sendBtn.textContent = isBusy ? "Streaming…" : "Send ↵";
    }
  }

  /* ------------------------------------------------------------------------
     VIM Scrolling & 100-Entry Jump List (`Ctrl-o` / `Ctrl-i`)
     ------------------------------------------------------------------------ */
  function cancelScrollAnim() {
    if (scrollAnimRaf !== null) {
      cancelAnimationFrame(scrollAnimRaf);
      scrollAnimRaf = null;
    }
    scrollAnimTarget = null;
  }

  function maxScrollY() {
    return Math.max(0, document.documentElement.scrollHeight - window.innerHeight);
  }

  function vimScrollBy(delta, isRepeat) {
    if (isRepeat) {
      cancelScrollAnim();
      window.scrollBy({ top: delta, behavior: "instant" });
      return;
    }
    const base = scrollAnimTarget !== null ? scrollAnimTarget : window.scrollY;
    const target = Math.max(0, Math.min(maxScrollY(), base + delta));
    scrollAnimStartY = window.scrollY;
    scrollAnimTarget = target;
    scrollAnimStartT = performance.now();

    if (scrollAnimRaf === null) {
      const step = function (now) {
        if (scrollAnimTarget === null) {
          scrollAnimRaf = null;
          return;
        }
        const t = Math.min(1, (now - scrollAnimStartT) / SCROLL_LERP_MS);
        const eased = 1 - Math.pow(1 - t, 3);
        const nextY = scrollAnimStartY + (scrollAnimTarget - scrollAnimStartY) * eased;
        window.scrollTo({ top: nextY, behavior: "instant" });
        if (t < 1) {
          scrollAnimRaf = requestAnimationFrame(step);
        } else {
          window.scrollTo({ top: scrollAnimTarget, behavior: "instant" });
          scrollAnimRaf = null;
          scrollAnimTarget = null;
        }
      };
      scrollAnimRaf = requestAnimationFrame(step);
    }
  }

  function halfPageDelta() {
    return Math.max(180, Math.round((window.innerHeight - 110) * 0.5));
  }

  function captureJumpEntry(overrideScrollY) {
    return {
      viewMode: state.viewMode || "thread",
      scheduleTarget: state.activeScheduleTarget || null,
      thread: state.thread,
      latestTurnId: state.latestTurnId,
      scrollY:
        typeof overrideScrollY === "number"
          ? Math.max(0, Math.round(overrideScrollY))
          : Math.max(0, Math.round(window.scrollY))
    };
  }

  function sameJumpLocation(a, b) {
    if (!a || !b) return false;
    const aThreadId = a.thread ? a.thread.id : "";
    const bThreadId = b.thread ? b.thread.id : "";
    return (
      a.viewMode === b.viewMode &&
      a.scheduleTarget === b.scheduleTarget &&
      aThreadId === bThreadId &&
      Math.abs(a.scrollY - b.scrollY) < 24
    );
  }

  function pushJumpPoint(overrideScrollY) {
    const entry = captureJumpEntry(overrideScrollY);
    if (jumpIndex < jumpList.length) {
      jumpList = jumpList.slice(0, jumpIndex);
    }
    if (jumpList.length > 0 && sameJumpLocation(jumpList[jumpList.length - 1], entry)) {
      jumpIndex = jumpList.length;
      return;
    }
    jumpList.push(entry);
    if (jumpList.length > MAX_JUMP_LIST) {
      jumpList.shift();
    }
    jumpIndex = jumpList.length;
  }

  function restoreJumpEntry(entry) {
    if (!entry) return;
    cancelScrollAnim();
    stopTtsIfSpeaking();

    state.viewMode = entry.viewMode || "thread";
    state.activeScheduleTarget = entry.scheduleTarget || null;
    if (entry.thread) {
      state.thread = entry.thread;
    }
    state.latestTurnId = entry.latestTurnId !== undefined ? entry.latestTurnId : null;
    syncSchedulesBtnActive();

    if (state.viewMode === "schedules_overview") {
      renderSchedulesOverview(true);
      window.scrollTo({ top: entry.scrollY, behavior: "instant" });
      hostSyncActiveThread(state.thread, state.latestTurnId, "Scheduled Actions");
      return;
    }

    if (state.viewMode === "schedule_inspector" && state.activeScheduleTarget) {
      const currentName =
        state.scheduleDetail && state.scheduleDetail.summary && state.scheduleDetail.summary.name;
      if (
        state.scheduleDetail &&
        (currentName === state.activeScheduleTarget ||
          String(state.scheduleDetail.summary.id) === String(state.activeScheduleTarget))
      ) {
        renderScheduleInspector(state.scheduleDetail, true);
        window.scrollTo({ top: entry.scrollY, behavior: "instant" });
        hostSyncActiveThread(
          state.thread,
          state.latestTurnId,
          state.scheduleDetail.summary.title || state.activeScheduleTarget
        );
      } else {
        pendingRestoreScrollY = entry.scrollY;
        hostOpenSchedule(state.activeScheduleTarget);
      }
      return;
    }

    renderActiveThread(null, { preserveScroll: true });
    window.scrollTo({ top: entry.scrollY, behavior: "instant" });
    hostSyncActiveThread(
      state.thread,
      state.latestTurnId,
      (state.thread && state.thread.title) || "Ask Computer"
    );
  }

  function jumpListBack() {
    if (jumpList.length === 0 || jumpIndex <= 0) return;
    if (jumpIndex === jumpList.length) {
      const current = captureJumpEntry();
      if (!sameJumpLocation(jumpList[jumpList.length - 1], current)) {
        jumpList.push(current);
        if (jumpList.length > MAX_JUMP_LIST) {
          jumpList.shift();
          jumpIndex = jumpList.length - 1;
        }
      } else {
        jumpIndex = jumpList.length - 1;
      }
    }
    if (jumpIndex <= 0) return;
    jumpIndex -= 1;
    restoreJumpEntry(jumpList[jumpIndex]);
  }

  function jumpListForward() {
    if (jumpList.length === 0 || jumpIndex >= jumpList.length - 1) return;
    jumpIndex += 1;
    restoreJumpEntry(jumpList[jumpIndex]);
  }

  window.addEventListener("wheel", cancelScrollAnim, { passive: true });
  window.addEventListener("pointerdown", cancelScrollAnim, { passive: true });

  /* ------------------------------------------------------------------------
     Hybrid In-Page Search (`/` or `Ctrl-f` -> `webkit6::FindController`)
     ------------------------------------------------------------------------ */
  function isFindBarOpen() {
    return Boolean(dom.findBar && !dom.findBar.hidden);
  }

  function updateFindCountUi() {
    if (!dom.findCount) return;
    dom.findCount.classList.remove("is-miss");
    if (!state.findQuery) {
      dom.findCount.textContent = "";
      return;
    }
    if (state.findFound === false) {
      dom.findCount.textContent = "No matches";
      dom.findCount.classList.add("is-miss");
    } else if (typeof state.findCount === "number" && state.findCount > 0) {
      dom.findCount.textContent =
        state.findCount === 1 ? "1 match" : state.findCount + " matches";
    } else if (state.findFound === true) {
      dom.findCount.textContent = "Match";
    } else {
      dom.findCount.textContent = "";
    }
  }

  function openFindBar(presetQuery) {
    if (!dom.findBar || !dom.findInput) return;
    findOriginScrollY = Math.round(window.scrollY);
    dom.findBar.hidden = false;
    if (typeof presetQuery === "string") {
      dom.findInput.value = presetQuery;
      state.findQuery = presetQuery;
    }
    dom.findInput.focus();
    dom.findInput.select();
    if (dom.findInput.value) {
      state.findQuery = dom.findInput.value;
      state.findActive = true;
      hostFindSearch(state.findQuery);
    }
  }

  function closeFindBar(clearHighlights) {
    if (dom.findInput && document.activeElement === dom.findInput) {
      dom.findInput.blur();
    }
    if (dom.findBar) {
      dom.findBar.hidden = true;
    }
    findOriginScrollY = null;
    if (clearHighlights) {
      state.findActive = false;
      state.findCount = null;
      state.findFound = null;
      updateFindCountUi();
      hostFindClose();
    }
  }

  /* ------------------------------------------------------------------------
     Theme, Dynamic Wallpaper Palette & Info/DropCap Toggles
     ------------------------------------------------------------------------ */
  function applyTheme(themeMode, systemDarkOverride) {
    if (themeMode && VALID_THEMES.indexOf(themeMode) !== -1) {
      state.theme = themeMode;
    }
    document.documentElement.setAttribute("data-theme", state.theme);
    if (typeof systemDarkOverride === "boolean") {
      state.systemDark = systemDarkOverride;
    }
    if (typeof state.systemDark === "boolean") {
      document.documentElement.setAttribute(
        "data-system-dark",
        state.systemDark ? "true" : "false"
      );
    } else {
      document.documentElement.removeAttribute("data-system-dark");
    }
  }

  function applyPalette(rawPalette) {
    const p = normalizePalette(rawPalette);
    if (!p) return;
    state.palette = p;
    const rootStyle = document.documentElement.style;
    if (p.primaryRgb) {
      rootStyle.setProperty("--primary-color", p.primaryRgb);
    }
    if (p.secondaryRgb) {
      rootStyle.setProperty("--secondary-color", p.secondaryRgb);
    }
    rootStyle.setProperty("--on-primary-color", p.onPrimaryHex || "#FFFCF0");
    refreshMountedPaletteSwatches();
  }

  function syncDropCap(enabled) {
    dropCapEnabled = Boolean(enabled);
    document.body.classList.toggle("drop-cap-enabled", dropCapEnabled);
  }

  function toggleDropCap() {
    syncDropCap(!dropCapEnabled);
    showToast("Editorial Drop Cap " + (dropCapEnabled ? "enabled" : "disabled"), "ok");
  }

  function syncTurnInfo(enabled) {
    showTurnInfo = Boolean(enabled);
    document.body.classList.toggle("show-turn-info", showTurnInfo);
    if (dom.infoBtn) {
      dom.infoBtn.classList.toggle("is-active", showTurnInfo);
    }
  }

  function toggleTurnInfo() {
    syncTurnInfo(!showTurnInfo);
  }

  function syncSchedulesBtnActive() {
    if (dom.schedulesBtn) {
      dom.schedulesBtn.classList.toggle(
        "is-active",
        state.viewMode === "schedules_overview" || state.viewMode === "schedule_inspector"
      );
    }
  }

  /* ------------------------------------------------------------------------
     Toast Stack
     ------------------------------------------------------------------------ */
  function showToast(message, kind) {
    if (!dom.toastStack || !message) return;
    const item = el("div", "toast-item");
    item.dataset.kind = kind || "ok";
    const icon =
      kind === "error" ? "✗" : kind === "warn" ? "⚠" : kind === "info" ? "ℹ" : "✓";
    item.appendChild(el("strong", "", icon));
    item.appendChild(el("span", "", message));
    dom.toastStack.appendChild(item);

    setTimeout(function () {
      if (item.parentNode) item.parentNode.removeChild(item);
    }, 3200);
  }

  /* ------------------------------------------------------------------------
     Markdown Content Hydration (Code Copy/Run + Disarmed Images + Anchors)
     ------------------------------------------------------------------------ */
  function hydrateTurnContent(container, turnIdPrefix) {
    const links = Array.prototype.slice.call(container.querySelectorAll("a"));
    links.forEach(function (a) {
      const href = a.getAttribute("href") || "";
      const txt = (a.textContent || "").trim();
      const lower = txt.toLowerCase();
      if (
        lower.startsWith("[image:") ||
        lower.startsWith("image:") ||
        txt.startsWith("[🖼") ||
        txt.startsWith("🖼") ||
        a.classList.contains("disarmed-image")
      ) {
        a.classList.add("disarmed-image");
        a.title = "Remote image disarmed — click to open externally";
      }
      if (/^https?:\/\//i.test(href)) {
        a.target = "_blank";
        a.rel = "noopener noreferrer";
      } else if (href.startsWith("#") && !a.dataset.jumpBound) {
        a.dataset.jumpBound = "1";
        a.addEventListener("click", function () {
          pushJumpPoint();
        });
      }
    });

    const blocks = Array.prototype.slice.call(container.querySelectorAll(".code-block"));
    blocks.forEach(function (block, idx) {
      const codeIndexAttr = block.getAttribute("data-code-index");
      const numericIndex =
        codeIndexAttr !== null && codeIndexAttr !== "" && !Number.isNaN(Number(codeIndexAttr))
          ? Number(codeIndexAttr)
          : idx;
      const runKey = turnIdPrefix + "-code-" + numericIndex;
      block.dataset.runKey = String(runKey);
      codeBlocksById.set(String(runKey), block);
      codeBlocksById.set("idx:" + String(numericIndex), block);

      const lang = block.getAttribute("data-lang") || "sh";
      const getRawCode = function () {
        const attrRaw = block.getAttribute("data-raw");
        if (attrRaw !== null && attrRaw !== "") return attrRaw;
        const codeNode = block.querySelector("pre code");
        return codeNode ? codeNode.textContent || "" : "";
      };

      const copyBtn = block.querySelector('[data-action="copy-code"]');
      if (copyBtn && !copyBtn.dataset.bound) {
        copyBtn.dataset.bound = "1";
        copyBtn.addEventListener("click", function () {
          const raw = getRawCode();
          hostCopy(raw, "Copied " + lang + " snippet");
          const prev = copyBtn.textContent;
          copyBtn.textContent = "Copied ✓";
          setTimeout(function () {
            copyBtn.textContent = prev;
          }, 1400);
        });
      }

      const runBtn = block.querySelector('[data-action="run-code"]');
      if (runBtn && !runBtn.dataset.bound) {
        runBtn.dataset.bound = "1";
        runBtn.addEventListener("click", function () {
          const raw = getRawCode();
          lastRunCodeBlock = block;
          codeBlocksById.set(String(runKey), block);
          codeBlocksById.set("idx:" + String(numericIndex), block);
          markCodeRunning(block, lang);
          hostRunCode(runKey, numericIndex, lang, raw);
        });
      }
    });
  }

  function markCodeRunning(block, lang) {
    if (!block) return;
    block.classList.add("is-running");
    let out = block.querySelector(".code-output");
    if (!out) {
      out = el("div", "code-output");
      block.appendChild(out);
    }
    out.hidden = false;
    out.innerHTML = "";
    const head = el("div", "code-output-header");
    const status = el("div", "code-output-status");
    status.appendChild(
      el("span", "exit-badge is-running", "◌ Running " + (lang || "snippet") + "…")
    );
    head.appendChild(status);
    out.appendChild(head);
  }

  function renderCodeFinished(block, ev) {
    if (!block) return;
    block.classList.remove("is-running");
    let out = block.querySelector(".code-output");
    if (!out) {
      out = el("div", "code-output");
      block.appendChild(out);
    }
    out.hidden = false;
    out.innerHTML = "";

    const exitCode =
      ev.exitCode !== undefined
        ? Number(ev.exitCode)
        : (ev.exit_code !== undefined ? Number(ev.exit_code) : 0);
    const durationMs =
      ev.durationMs !== undefined
        ? ev.durationMs
        : (ev.duration_ms !== undefined ? ev.duration_ms : null);
    const stdout = ev.stdout !== undefined ? String(ev.stdout) : (ev.output || "");
    const stderr = ev.stderr !== undefined ? String(ev.stderr) : (ev.error || "");

    const head = el("div", "code-output-header");
    const status = el("div", "code-output-status");
    status.appendChild(
      el(
        "span",
        "exit-badge " + (exitCode === 0 ? "is-ok" : "is-err"),
        exitCode === 0 ? "✓ exit 0" : ("✗ exit " + exitCode)
      )
    );
    if (durationMs !== null) {
      status.appendChild(el("span", "duration-label", durationMs + " ms"));
    }

    const actions = el("div", "code-output-actions");
    const followBtn = el("button", "w-btn", "Ask Follow-up with Output");
    followBtn.type = "button";
    followBtn.addEventListener("click", function () {
      const combined = (stdout + (stderr ? "\n" + stderr : "")).trim();
      window.sendPrompt(
        "Analyze this command output (exit " +
          exitCode +
          ") and suggest next steps:\n\n```\n" +
          combined +
          "\n```"
      );
    });

    const clearBtn = el("button", "w-btn", "×");
    clearBtn.type = "button";
    clearBtn.title = "Hide output";
    clearBtn.addEventListener("click", function () {
      out.hidden = true;
    });

    actions.appendChild(followBtn);
    actions.appendChild(clearBtn);
    head.appendChild(status);
    head.appendChild(actions);
    out.appendChild(head);

    const bodyPre = el("pre", "code-output-body");
    bodyPre.textContent = stdout || (stderr ? "" : "(no stdout)");
    if (stderr) {
      bodyPre.appendChild(el("span", "code-output-stderr", stderr));
    }
    out.appendChild(bodyPre);
  }

  /* ------------------------------------------------------------------------
     Turn Info Metadata Strip (`Info I`)
     ------------------------------------------------------------------------ */
  function formatDurationLabel(ms) {
    if (typeof ms !== "number" || !Number.isFinite(ms) || ms < 0) return "";
    if (ms < 1000) return Math.round(ms) + " ms";
    return (ms / 1000).toFixed(1) + " s";
  }

  function populateTurnInfoStrip(stripEl, turn) {
    stripEl.innerHTML = "";

    function addKv(label, value) {
      if (!value) return;
      const kv = el("span", "info-kv");
      kv.appendChild(el("span", "info-kv-label", label + ":"));
      kv.appendChild(el("span", "", value));
      stripEl.appendChild(kv);
    }

    addKv("profile", turn.profile || turn.badge || "default");
    addKv("lane", turn.lane || "interactive");
    if (turn.origin) addKv("origin", turn.origin);
    if (turn.status && turn.status !== "completed") addKv("status", turn.status);
    if (turn.scheduleName) addKv("schedule", turn.scheduleName);
    if (turn.turnId !== null && turn.turnId !== undefined) {
      addKv("turn", "#" + turn.turnId);
    }
    const dur = formatDurationLabel(turn.durationMs);
    if (dur) addKv("time", dur);
    if (turn.readingTime) addKv("read", turn.readingTime);

    if (Array.isArray(turn.denied) && turn.denied.length > 0) {
      addKv("denied", turn.denied.join(", "));
    }

    const crumbs = Array.isArray(turn.toolCalls) ? turn.toolCalls : [];
    if (crumbs.length > 0) {
      addKv("widgets", crumbs.join(" · "));
    }

    const palWrap = el("span", "info-kv");
    palWrap.dataset.paletteSlot = "1";
    renderPaletteIntoSlot(palWrap);
    stripEl.appendChild(palWrap);
  }

  function renderPaletteIntoSlot(slotEl) {
    slotEl.innerHTML = "";
    slotEl.appendChild(el("span", "info-kv-label", "palette:"));
    const p = state.palette;

    const pDot = el("span", "palette-swatch-dot");
    pDot.style.backgroundColor = "rgb(var(--primary-color))";
    slotEl.appendChild(pDot);
    slotEl.appendChild(el("span", "", p && p.primaryHex ? p.primaryHex : "primary"));

    const sDot = el("span", "palette-swatch-dot");
    sDot.style.backgroundColor = "rgb(var(--secondary-color))";
    slotEl.appendChild(sDot);
    slotEl.appendChild(el("span", "", p && p.secondaryHex ? p.secondaryHex : "secondary"));
  }

  function refreshMountedPaletteSwatches() {
    if (!dom.readingColumn) return;
    const slots = Array.prototype.slice.call(
      dom.readingColumn.querySelectorAll("[data-palette-slot]")
    );
    slots.forEach(renderPaletteIntoSlot);
  }

  /* ------------------------------------------------------------------------
     Prose-First Turn Card & Thread Rendering
     ------------------------------------------------------------------------ */
  function appendTurnStatusCallout(bodyEl, turn) {
    const status = String(turn.status || "completed").toLowerCase();
    const hasProse = Boolean(turn.markdown && turn.markdown.trim());

    if (status === "failed" || status === "cancelled") {
      const aside = el(
        "aside",
        "callout " + (status === "failed" ? "callout-caution" : "callout-warning")
      );
      const titleText =
        status === "cancelled"
          ? "Turn Cancelled"
          : ("Turn Failed" + (turn.failReason ? " · " + turn.failReason : ""));
      aside.appendChild(el("div", "callout-title", titleText));
      const inner = el("div", "callout-body");
      inner.appendChild(
        el(
          "p",
          "",
          turn.errorMessage ||
            (status === "cancelled"
              ? "This turn was cancelled before completing."
              : "The agent could not complete this request.")
        )
      );
      aside.appendChild(inner);
      bodyEl.appendChild(aside);
      return;
    }

    if (status === "suppressed" && !hasProse) {
      const aside = el("aside", "callout callout-note");
      aside.appendChild(
        el(
          "div",
          "callout-title",
          "Suppressed" + (turn.suppressedToken ? " · " + turn.suppressedToken : "")
        )
      );
      const inner = el("div", "callout-body");
      inner.appendChild(
        el(
          "p",
          "",
          turn.suppressedToken
            ? "Matched silent token `" + turn.suppressedToken + "` — nothing required attention."
            : "This scheduled check completed silently with nothing to report."
        )
      );
      aside.appendChild(inner);
      bodyEl.appendChild(aside);
      return;
    }

    if ((status === "running" || status === "queued") && !hasProse) {
      const aside = el("aside", "callout callout-note");
      aside.appendChild(
        el("div", "callout-title", status === "running" ? "Running" : "Queued")
      );
      const inner = el("div", "callout-body");
      inner.appendChild(
        el(
          "p",
          "",
          status === "running"
            ? "This turn is still running in the background."
            : "This turn is queued to run."
        )
      );
      aside.appendChild(inner);
      bodyEl.appendChild(aside);
    }
  }

  function createTurnElement(turn, isStreaming) {
    const article = el("article", "turn-card");
    article.dataset.turnId = String(turn.id);

    const header = el("header", "turn-header");
    const metaRow = el("div", "turn-meta-row");
    const metaLeft = el("div", "turn-meta-left");
    metaLeft.appendChild(el("span", "turn-brand", "Ask Computer"));
    metaLeft.appendChild(el("span", "meta-sep", "·"));
    const tsSpan = el("span", "", turn.createdAt || "Now");
    metaLeft.appendChild(tsSpan);

    if (turn.scheduleName || (turn.scheduleId !== null && turn.scheduleId !== undefined)) {
      const schedTarget = turn.scheduleName || String(turn.scheduleId);
      const chip = el("button", "schedule-chip", "⟳ " + schedTarget);
      chip.type = "button";
      chip.title = "Inspect schedule \"" + schedTarget + "\" (Ctrl-o to jump back)";
      chip.addEventListener("click", function () {
        openScheduleInspectorView(schedTarget);
      });
      metaLeft.appendChild(chip);
    }

    const metaRight = el("div", "turn-meta-right");
    let streamIndicator = null;
    let streamStatusSpan = null;
    if (isStreaming) {
      streamIndicator = el("span", "streaming-indicator");
      streamIndicator.appendChild(el("span", "streaming-dot"));
      streamStatusSpan = el("span", "", "Thinking…");
      streamIndicator.appendChild(streamStatusSpan);
      metaRight.appendChild(streamIndicator);
    }

    metaRow.appendChild(metaLeft);
    metaRow.appendChild(metaRight);
    header.appendChild(metaRow);

    if (turn.prompt && !shouldHideTurnPrompt(turn)) {
      const promptBox = el("div", "turn-prompt-box");
      promptBox.appendChild(el("p", "turn-prompt-text", turn.prompt));
      header.appendChild(promptBox);
    } else {
      header.classList.add("turn-header-no-prompt");
    }

    const infoStrip = el("div", "turn-info-strip");
    populateTurnInfoStrip(infoStrip, turn);
    header.appendChild(infoStrip);

    article.appendChild(header);

    const body = el("div", "turn-body");
    body.innerHTML = turn.html || '<div class="markdown-content"></div>';
    if (!isStreaming) {
      appendTurnStatusCallout(body, turn);
    }
    if (Array.isArray(turn.denied) && turn.denied.length > 0) {
      appendDeniedNotice(body, turn.denied);
    }
    hydrateTurnContent(body, turn.id);
    article.appendChild(body);

    const widgetsWrap = el("div", "turn-widgets");
    const widgets = Array.isArray(turn.widgets) ? turn.widgets : [];
    if (window.HCWidgets && typeof window.HCWidgets.renderWidget === "function") {
      widgets.forEach(function (w) {
        widgetsWrap.appendChild(window.HCWidgets.renderWidget(w));
      });
    }
    article.appendChild(widgetsWrap);

    return {
      article: article,
      body: body,
      widgetsWrap: widgetsWrap,
      infoStrip: infoStrip,
      tsSpan: tsSpan,
      streamIndicator: streamIndicator,
      streamStatusSpan: streamStatusSpan
    };
  }

  function appendDeniedNotice(bodyEl, deniedList) {
    if (!Array.isArray(deniedList) || deniedList.length === 0) return;
    const aside = el("aside", "callout callout-warning");
    aside.appendChild(el("div", "callout-title", "Refused Commands"));
    const inner = el("div", "callout-body");
    inner.appendChild(el("p", "", deniedList.join(" · ")));
    aside.appendChild(inner);
    bodyEl.appendChild(aside);
  }

  function renderEmptyState() {
    const empty = el("section", "empty-state");
    empty.appendChild(el("h1", "empty-state-title", "Ask Computer"));
    empty.appendChild(
      el(
        "p",
        "empty-state-hint",
        "Press i to ask a question, h to search past answers, or S to inspect scheduled actions."
      )
    );
    dom.readingColumn.appendChild(empty);
  }

  function renderActiveThread(scrollTargetTurnId, opts) {
    if (!dom.readingColumn) return;
    const options = opts || {};
    state.viewMode = "thread";
    state.activeScheduleTarget = null;
    syncSchedulesBtnActive();

    const thread = state.thread;
    codeBlocksById.clear();
    dom.readingColumn.innerHTML = "";

    if (dom.topbarTitle) {
      dom.topbarTitle.textContent =
        thread && thread.title && thread.title !== "Ask Computer"
          ? thread.title
          : "";
    }

    if (!thread || !Array.isArray(thread.turns) || thread.turns.length === 0) {
      renderEmptyState();
      return;
    }

    let scrollEl = null;
    thread.turns.forEach(function (turn, tIdx) {
      const rendered = createTurnElement(turn, false);
      if (tIdx === 0 && isMorningBriefHeuristic(turn, thread.title)) {
        rendered.article.classList.add("always-drop-cap");
      }
      dom.readingColumn.appendChild(rendered.article);
      if (
        scrollTargetTurnId !== undefined &&
        scrollTargetTurnId !== null &&
        String(turn.turnId || turn.id) === String(scrollTargetTurnId)
      ) {
        scrollEl = rendered.article;
      }
    });

    updateActivatedDayPlanDates(null);

    if (pendingRestoreScrollY !== null) {
      const y = pendingRestoreScrollY;
      pendingRestoreScrollY = null;
      window.scrollTo({ top: y, behavior: "instant" });
    } else if (!options.preserveScroll) {
      if (scrollEl && thread.turns.length > 1) {
        scrollEl.scrollIntoView({ behavior: "instant", block: "start" });
      } else {
        window.scrollTo({ top: 0, behavior: "instant" });
      }
    }
    scheduleSaveViewState(false);
  }

  /* ------------------------------------------------------------------------
     Scheduled Actions Overview & Schedule Inspector Views (`S`, `--schedule`)
     ------------------------------------------------------------------------ */
  function openSchedulesOverviewView() {
    pushJumpPoint();
    stopTtsIfSpeaking();
    state.viewMode = "schedules_overview";
    state.activeScheduleTarget = null;
    syncSchedulesBtnActive();
    renderSchedulesOverview(false);
    hostLoadSchedules();
    hostSyncActiveThread(state.thread, state.latestTurnId, "Scheduled Actions");
  }

  function openScheduleInspectorView(target) {
    if (!target) return;
    pushJumpPoint();
    stopTtsIfSpeaking();
    state.viewMode = "schedule_inspector";
    state.activeScheduleTarget = String(target);
    syncSchedulesBtnActive();
    hostOpenSchedule(target);
  }

  function toggleSchedulesView() {
    if (
      state.viewMode === "schedules_overview" ||
      state.viewMode === "schedule_inspector"
    ) {
      pushJumpPoint();
      stopTtsIfSpeaking();
      renderActiveThread();
      hostSyncActiveThread(
        state.thread,
        state.latestTurnId,
        (state.thread && state.thread.title) || "Ask Computer"
      );
    } else {
      openSchedulesOverviewView();
    }
  }

  function buildScheduleActionButtons(summary, includeInspect) {
    const actions = el("div", "schedule-actions");
    const target = summary.name || String(summary.id);

    if (includeInspect) {
      const inspectBtn = el("button", "w-btn w-btn-primary", "Inspect");
      inspectBtn.type = "button";
      inspectBtn.addEventListener("click", function () {
        openScheduleInspectorView(target);
      });
      actions.appendChild(inspectBtn);
    }

    const runBtn = el("button", "w-btn", "▶ Run Now");
    runBtn.type = "button";
    runBtn.disabled = Boolean(summary.running);
    runBtn.addEventListener("click", function () {
      runBtn.disabled = true;
      runBtn.textContent = "◌ Triggering…";
      hostScheduleAction("run", target);
    });
    actions.appendChild(runBtn);

    const toggleBtn = el(
      "button",
      "w-btn",
      summary.enabled ? "⏸ Pause" : "▶ Resume"
    );
    toggleBtn.type = "button";
    toggleBtn.addEventListener("click", function () {
      hostScheduleAction(summary.enabled ? "pause" : "resume", target);
    });
    actions.appendChild(toggleBtn);

    if (!summary.managed) {
      const rmBtn = el("button", "w-btn w-btn-danger", "Remove");
      rmBtn.type = "button";
      rmBtn.addEventListener("click", function () {
        hostScheduleAction("remove", target);
      });
      actions.appendChild(rmBtn);
    }

    return actions;
  }

  function renderSchedulesOverview(preserveScroll) {
    if (!dom.readingColumn) return;
    state.viewMode = "schedules_overview";
    syncSchedulesBtnActive();
    codeBlocksById.clear();
    dom.readingColumn.innerHTML = "";

    if (dom.topbarTitle) {
      dom.topbarTitle.textContent = "Scheduled Actions";
    }

    const wrap = el("section", "schedules-view");
    const list = Array.isArray(state.schedules) ? state.schedules : [];
    const activeCount = list.filter(function (s) { return s.enabled; }).length;
    const runningCount = list.filter(function (s) { return s.running; }).length;

    const header = el("header", "schedules-header");
    const titleGroup = el("div", "schedules-title-group");
    titleGroup.appendChild(el("h1", "schedules-heading", "Scheduled Actions"));
    const subParts = [list.length + " total", activeCount + " active"];
    if (runningCount > 0) subParts.push(runningCount + " running");
    titleGroup.appendChild(el("p", "schedules-subtitle", subParts.join(" · ")));

    const headerActions = el("div", "schedules-header-actions");
    const refreshBtn = el("button", "w-btn", "↻ Refresh");
    refreshBtn.type = "button";
    refreshBtn.addEventListener("click", function () {
      hostLoadSchedules();
    });
    headerActions.appendChild(refreshBtn);

    if (state.thread && state.thread.turns && state.thread.turns.length > 0) {
      const backBtn = el("button", "w-btn w-btn-ghost", "← Back to Chat");
      backBtn.type = "button";
      backBtn.addEventListener("click", function () {
        toggleSchedulesView();
      });
      headerActions.appendChild(backBtn);
    }

    header.appendChild(titleGroup);
    header.appendChild(headerActions);
    wrap.appendChild(header);

    if (list.length === 0) {
      const empty = el("div", "schedule-card");
      empty.appendChild(
        el(
          "div",
          "w-muted",
          "No scheduled actions found in history.db. Use `hey-computer schedule add` or `[scheduler.jobs]` in config.toml to add one."
        )
      );
      wrap.appendChild(empty);
      dom.readingColumn.appendChild(wrap);
      return;
    }

    const cardsList = el("div", "schedules-list");
    list.forEach(function (s) {
      const card = el("article", "schedule-card" + (s.enabled ? "" : " is-paused"));
      const top = el("div", "schedule-card-top");
      const titleRow = el("div", "schedule-title-row");

      const h2 = el("h2", "schedule-card-title", s.title || s.name);
      h2.addEventListener("click", function () {
        openScheduleInspectorView(s.name || String(s.id));
      });
      titleRow.appendChild(h2);
      if (s.name && s.name !== s.title) {
        titleRow.appendChild(el("span", "schedule-slug", s.name));
      }

      const statusPill = el("span", "schedule-status-pill", s.status || (s.enabled ? "active" : "paused"));
      statusPill.dataset.status = String(s.status || (s.enabled ? "active" : "paused")).toLowerCase();
      titleRow.appendChild(statusPill);

      if (s.managed) {
        titleRow.appendChild(el("span", "schedule-managed-pill", "managed"));
      }

      top.appendChild(titleRow);
      top.appendChild(buildScheduleActionButtons(s, true));
      card.appendChild(top);

      const grid = el("div", "schedule-meta-grid");
      function addMeta(label, value) {
        const item = el("div", "schedule-meta-item");
        item.appendChild(el("span", "schedule-meta-label", label));
        item.appendChild(el("span", "schedule-meta-value", value || "—"));
        grid.appendChild(item);
      }

      const specText =
        s.humanSchedule && s.humanSchedule !== s.scheduleSpec
          ? s.humanSchedule + " (" + s.scheduleSpec + ")"
          : (s.scheduleSpec || "—");
      addMeta("Schedule", specText);
      addMeta("Mode", (s.mode || "agent") + (s.profile ? " · " + s.profile : ""));
      addMeta("Next Run", s.enabled ? (s.nextRunLocal || "—") : "paused");
      addMeta(
        "Last Run",
        (s.lastRunLocal || "never") +
          (s.lastStatus ? " · " + s.lastStatus : "") +
          (s.runCount > 0 ? " (" + s.runCount + " runs)" : "")
      );
      card.appendChild(grid);

      if (s.lastError) {
        card.appendChild(
          el(
            "div",
            "schedule-error-line",
            "Last error" +
              (s.consecutiveFailures > 1 ? " (" + s.consecutiveFailures + "×)" : "") +
              ": " +
              s.lastError
          )
        );
      }

      cardsList.appendChild(card);
    });

    wrap.appendChild(cardsList);
    dom.readingColumn.appendChild(wrap);

    if (pendingRestoreScrollY !== null) {
      const y = pendingRestoreScrollY;
      pendingRestoreScrollY = null;
      window.scrollTo({ top: y, behavior: "instant" });
    } else if (!preserveScroll) {
      window.scrollTo({ top: 0, behavior: "instant" });
    }
    scheduleSaveViewState(false);
  }

  function renderScheduleInspector(detail, preserveScroll) {
    if (!dom.readingColumn || !detail) return;
    state.viewMode = "schedule_inspector";
    state.scheduleDetail = detail;
    const s = detail.summary || {};
    state.activeScheduleTarget = s.name || String(s.id || "");
    syncSchedulesBtnActive();

    codeBlocksById.clear();
    dom.readingColumn.innerHTML = "";

    if (dom.topbarTitle) {
      dom.topbarTitle.textContent = s.title || s.name || "Schedule";
    }

    const wrap = el("section", "schedule-inspector");

    const nav = el("div", "inspector-nav");
    const allBtn = el("button", "w-btn", "← All Schedules");
    allBtn.type = "button";
    allBtn.addEventListener("click", function () {
      openSchedulesOverviewView();
    });
    nav.appendChild(allBtn);

    if (state.thread && state.thread.turns && state.thread.turns.length > 0) {
      const chatBtn = el("button", "w-btn w-btn-ghost", "Back to Chat");
      chatBtn.type = "button";
      chatBtn.addEventListener("click", function () {
        pushJumpPoint();
        stopTtsIfSpeaking();
        renderActiveThread();
        hostSyncActiveThread(
          state.thread,
          state.latestTurnId,
          (state.thread && state.thread.title) || "Ask Computer"
        );
      });
      nav.appendChild(chatBtn);
    }

    const refreshBtn = el("button", "w-btn w-btn-ghost", "↻ Refresh");
    refreshBtn.type = "button";
    refreshBtn.addEventListener("click", function () {
      hostOpenSchedule(state.activeScheduleTarget);
    });
    nav.appendChild(refreshBtn);
    wrap.appendChild(nav);

    const card = el("div", "inspector-card");
    const top = el("div", "inspector-card-top");
    const titleRow = el("div", "schedule-title-row");
    titleRow.appendChild(el("h1", "schedules-heading", s.title || s.name || "Schedule"));
    if (s.name && s.name !== s.title) {
      titleRow.appendChild(el("span", "schedule-slug", s.name));
    }
    const statusPill = el(
      "span",
      "schedule-status-pill",
      s.status || (s.enabled ? "active" : "paused")
    );
    statusPill.dataset.status = String(s.status || (s.enabled ? "active" : "paused")).toLowerCase();
    titleRow.appendChild(statusPill);
    if (s.managed) {
      titleRow.appendChild(el("span", "schedule-managed-pill", "managed"));
    }

    top.appendChild(titleRow);
    top.appendChild(buildScheduleActionButtons(s, false));
    card.appendChild(top);

    const grid = el("div", "inspector-meta-grid");
    function addMeta(label, value) {
      const item = el("div", "schedule-meta-item");
      item.appendChild(el("span", "schedule-meta-label", label));
      item.appendChild(el("span", "schedule-meta-value", value || "—"));
      grid.appendChild(item);
    }

    const specText =
      s.humanSchedule && s.humanSchedule !== s.scheduleSpec
        ? s.humanSchedule + " (" + s.scheduleSpec + ")"
        : (s.scheduleSpec || "—");
    addMeta("Schedule", specText);
    addMeta("Next Run", s.enabled ? (s.nextRunLocal || "—") : "paused");
    addMeta(
      "Last Run",
      (s.lastRunLocal || "never") + (s.lastStatus ? " · " + s.lastStatus : "")
    );
    addMeta(
      "Runs / Failures",
      String(s.runCount || 0) +
        " total" +
        (detail.maxRuns ? " / max " + detail.maxRuns : "") +
        (s.consecutiveFailures > 0 ? " · " + s.consecutiveFailures + " failing" : "")
    );
    addMeta("Mode / Profile", (s.mode || "agent") + (s.profile ? " · " + s.profile : ""));
    addMeta(
      "Delivery",
      "failure: " +
        (detail.failureDeliver || "notify") +
        (detail.suppressToken ? " · silent: " + detail.suppressToken : "") +
        (detail.expiresAtLocal ? " · expires " + detail.expiresAtLocal : "")
    );
    card.appendChild(grid);

    if (s.lastError) {
      card.appendChild(el("div", "schedule-error-line", "Last error: " + s.lastError));
    }

    if (detail.prompt && detail.prompt.trim()) {
      const det = el("details", "inspector-details");
      det.appendChild(el("summary", "", "▾ Prompt"));
      det.appendChild(el("pre", "inspector-pre", detail.prompt));
      card.appendChild(det);
    }

    if (detail.script && detail.script.trim()) {
      const det = el("details", "inspector-details");
      det.appendChild(el("summary", "", "▾ Pre-collector Script"));
      det.appendChild(el("pre", "inspector-pre", detail.script));
      card.appendChild(det);
    }

    const runsSection = el("div", "inspector-runs-section");
    runsSection.appendChild(
      el(
        "div",
        "inspector-runs-title",
        "Recent Runs (" + detail.recentRuns.length + ")"
      )
    );

    if (detail.recentRuns.length === 0) {
      runsSection.appendChild(
        el("div", "w-muted", "No linked turns recorded in history.db yet.")
      );
    } else {
      const runsList = el("div", "inspector-runs-list");
      detail.recentRuns.forEach(function (run) {
        const row = el("button", "inspector-run-row");
        row.type = "button";
        const rTop = el("div", "inspector-run-top");
        const rLeft = el("div", "inspector-run-left");
        const statusText =
          run.status === "suppressed" && run.suppressedToken
            ? "suppressed · " + run.suppressedToken
            : (run.status || "completed");
        const pill = el("span", "run-status-pill", statusText);
        pill.dataset.status = String(run.status || "completed").toLowerCase();
        rLeft.appendChild(pill);
        rLeft.appendChild(el("strong", "", "#" + run.turnId));
        rLeft.appendChild(el("span", "w-muted", run.createdAt || ""));
        if (typeof run.durationMs === "number") {
          rLeft.appendChild(el("span", "w-muted", formatDurationLabel(run.durationMs)));
        }
        rTop.appendChild(rLeft);

        if (run.widgetCount > 0) {
          rTop.appendChild(
            el("span", "w-pill", run.widgetCount + (run.widgetCount === 1 ? " widget" : " widgets"))
          );
        }
        row.appendChild(rTop);

        const summaryLine =
          run.errorMessage ||
          run.headline ||
          (run.status === "suppressed" ? "Silent check passed" : "");
        if (summaryLine) {
          row.appendChild(el("div", "inspector-run-headline", summaryLine));
        }

        row.addEventListener("click", function () {
          pushJumpPoint();
          stopTtsIfSpeaking();
          hostOpenHistoryTurn(run.turnId);
        });
        runsList.appendChild(row);
      });
      runsSection.appendChild(runsList);
    }

    card.appendChild(runsSection);
    wrap.appendChild(card);

    if (detail.latestTurn) {
      const dividerLabel =
        "Latest Output" +
        (detail.latestTurn.turnId ? " · #" + detail.latestTurn.turnId : "");
      wrap.appendChild(el("div", "inspector-output-divider", dividerLabel));
      const rendered = createTurnElement(detail.latestTurn, false);
      if (isMorningBriefHeuristic(detail.latestTurn, s.title || s.name || "")) {
        rendered.article.classList.add("always-drop-cap");
      }
      wrap.appendChild(rendered.article);
    } else {
      const noOut = el("div", "schedule-card");
      noOut.appendChild(
        el("div", "w-muted", "No completed output recorded yet. Click ▶ Run Now to trigger a run.")
      );
      wrap.appendChild(noOut);
    }

    dom.readingColumn.appendChild(wrap);
    updateActivatedDayPlanDates(null);

    if (pendingRestoreScrollY !== null) {
      const y = pendingRestoreScrollY;
      pendingRestoreScrollY = null;
      window.scrollTo({ top: y, behavior: "instant" });
    } else if (!preserveScroll) {
      window.scrollTo({ top: 0, behavior: "instant" });
    }
    scheduleSaveViewState(false);
  }

  /* ------------------------------------------------------------------------
     Follow-Up Prompt & Live Socket Streaming Engine
     ------------------------------------------------------------------------ */
  function clearEmptyPlaceholder() {
    if (!dom.readingColumn) return;
    const empty = dom.readingColumn.querySelector(".empty-state");
    if (empty) empty.remove();
  }

  window.sendPrompt = function (text, opts) {
    const cleanText = String(text || "").trim();
    if (!cleanText) return;
    if (activeStreamRef) {
      showToast("Wait for the current answer to finish streaming", "warn");
      return;
    }

    if (state.viewMode !== "thread") {
      pushJumpPoint();
      renderActiveThread(null, { preserveScroll: true });
    }

    const options = opts && typeof opts === "object" ? opts : {};
    const profile = options.profile || "";
    const explicitReplyTo = parsePositiveTurnId(options.replyTo);
    const replyTo =
      explicitReplyTo !== null ? explicitReplyTo : state.latestTurnId;

    let promptToSend = cleanText;
    if (
      !options.skipDayPlanDelta &&
      window.HCWidgets &&
      typeof window.HCWidgets.getActiveDayPlanDelta === "function"
    ) {
      const delta = window.HCWidgets.getActiveDayPlanDelta();
      if (delta && delta.text) {
        promptToSend = cleanText + "\n\n" + delta.text;
      }
    }

    clearEmptyPlaceholder();

    const tempTurn = {
      id: "stream-" + Date.now(),
      turnId: null,
      prompt: cleanText,
      badge: profile || "default",
      profile: profile || "",
      lane: "interactive",
      origin: "typed",
      status: "running",
      durationMs: null,
      denied: [],
      readingTime: "",
      createdAt: "Just now",
      widgets: [],
      toolCalls: [],
      html: '<div class="markdown-content"><p><em>Thinking…</em></p></div>',
      markdown: ""
    };

    const refs = createTurnElement(tempTurn, true);
    dom.readingColumn.appendChild(refs.article);
    activeStreamRef = {
      turn: tempTurn,
      refs: refs,
      boundId: null,
      rawBuffer: ""
    };
    syncComposerBusy(true);
    refs.article.scrollIntoView({ behavior: "instant", block: "end" });

    const payload = {
      prompt: promptToSend,
      text: promptToSend,
      profile: profile || undefined,
      replyTo: replyTo !== null ? replyTo : undefined
    };

    if (nativeSendPrompt) {
      nativeSendPrompt(payload);
      return;
    }
    if (hostPost({ type: "send_prompt", ...payload })) {
      return;
    }

    window.__hcDispatch({
      type: "stream_error",
      message: "Viewer host bridge is not available."
    });
  };

  if (rawHost) {
    rawHost.sendPrompt = window.sendPrompt;
  }

  function handleStreamStart(ev) {
    if (ev.optimistic && !activeStreamRef) {
      if (state.viewMode !== "thread") {
        pushJumpPoint();
        renderActiveThread(null, { preserveScroll: true });
      }
      clearEmptyPlaceholder();
      const tempTurn = {
        id: "stream-" + Date.now(),
        turnId: null,
        prompt: ev.prompt || "Follow-up",
        badge: ev.profile || "default",
        profile: ev.profile || "",
        lane: "interactive",
        origin: "typed",
        status: "running",
        durationMs: null,
        denied: [],
        readingTime: "",
        createdAt: ev.createdAt || "Just now",
        widgets: [],
        toolCalls: [],
        html: '<div class="markdown-content"><p><em>Thinking…</em></p></div>',
        markdown: ""
      };
      const refs = createTurnElement(tempTurn, true);
      dom.readingColumn.appendChild(refs.article);
      activeStreamRef = {
        turn: tempTurn,
        refs: refs,
        boundId: null,
        rawBuffer: ""
      };
      syncComposerBusy(true);
      refs.article.scrollIntoView({ behavior: "instant", block: "end" });
      return;
    }

    const incomingId =
      ev.id || ev.turnId || ev.turn_id || ev.replyId || ("stream-" + Date.now());
    const parsedNum = parsePositiveTurnId(incomingId);

    if (activeStreamRef && !activeStreamRef.boundId) {
      activeStreamRef.boundId = String(incomingId);
      activeStreamRef.turn.id = String(incomingId);
      activeStreamRef.turn.turnId = parsedNum;
      activeStreamRef.refs.article.dataset.turnId = String(incomingId);
      if (ev.profile || ev.badge) {
        activeStreamRef.turn.badge = ev.profile || ev.badge;
        activeStreamRef.turn.profile = ev.profile || "";
      }
      if (ev.createdAt || ev.created_at) {
        activeStreamRef.turn.createdAt = ev.createdAt || ev.created_at;
        activeStreamRef.refs.tsSpan.textContent = activeStreamRef.turn.createdAt;
      }
      if (activeStreamRef.refs.streamStatusSpan) {
        activeStreamRef.refs.streamStatusSpan.textContent = "Streaming…";
      }
      populateTurnInfoStrip(activeStreamRef.refs.infoStrip, activeStreamRef.turn);
      return;
    }

    if (activeStreamRef && activeStreamRef.boundId === String(incomingId)) {
      return;
    }

    if (state.viewMode !== "thread") {
      pushJumpPoint();
      renderActiveThread(null, { preserveScroll: true });
    }
    clearEmptyPlaceholder();
    const turn = {
      id: String(incomingId),
      turnId: parsedNum,
      prompt: ev.prompt || "Follow-up",
      badge: ev.profile || ev.badge || "default",
      profile: ev.profile || "",
      lane: "interactive",
      origin: "typed",
      status: "running",
      durationMs: null,
      denied: [],
      readingTime: "",
      createdAt: ev.createdAt || ev.created_at || "Just now",
      widgets: [],
      toolCalls: [],
      html: '<div class="markdown-content"></div>',
      markdown: ""
    };
    const refs = createTurnElement(turn, true);
    dom.readingColumn.appendChild(refs.article);
    activeStreamRef = {
      turn: turn,
      refs: refs,
      boundId: String(incomingId),
      rawBuffer: ""
    };
    syncComposerBusy(true);
    refs.article.scrollIntoView({ behavior: "instant", block: "end" });
  }

  function handleStreamProgress(ev) {
    if (!activeStreamRef) return;
    const statusText = String(ev.status || "").trim();
    if (statusText && activeStreamRef.refs.streamStatusSpan) {
      activeStreamRef.refs.streamStatusSpan.textContent = statusText;
    }
  }

  function handleStreamChunk(ev) {
    if (!activeStreamRef) {
      handleStreamStart(ev);
    }
    const html = ev.html || ev.renderedHtml || ev.rendered_html || ev.answerHtml || "";
    const fullMd = ev.markdown || ev.accumulated || "";
    const delta = ev.chunk || ev.delta || ev.text || "";

    if (fullMd) {
      activeStreamRef.rawBuffer = fullMd;
    } else if (delta) {
      activeStreamRef.rawBuffer += delta;
    }

    activeStreamRef.turn.markdown = activeStreamRef.rawBuffer;
    activeStreamRef.turn.html = html
      ? ensureMarkdownWrapper(html, activeStreamRef.rawBuffer)
      : renderFallbackMarkdown(activeStreamRef.rawBuffer);

    activeStreamRef.refs.body.innerHTML = activeStreamRef.turn.html;
  }

  function handleStreamWidget(ev) {
    if (!activeStreamRef || !window.HCWidgets) return;
    if (Array.isArray(ev.widgets)) {
      activeStreamRef.turn.widgets = ev.widgets;
      activeStreamRef.refs.widgetsWrap.innerHTML = "";
      ev.widgets.forEach(function (w) {
        activeStreamRef.refs.widgetsWrap.appendChild(window.HCWidgets.renderWidget(w));
      });
    } else if (ev.widget) {
      activeStreamRef.turn.widgets.push(ev.widget);
      activeStreamRef.refs.widgetsWrap.appendChild(
        window.HCWidgets.renderWidget(ev.widget)
      );
    }
    updateActivatedDayPlanDates(null);
  }

  function handleStreamReset() {
    if (!activeStreamRef) return;
    activeStreamRef.rawBuffer = "";
    activeStreamRef.turn.markdown = "";
    activeStreamRef.turn.widgets = [];
    activeStreamRef.turn.html =
      '<div class="markdown-content"><p><em>Retrying with deep profile…</em></p></div>';
    activeStreamRef.refs.body.innerHTML = activeStreamRef.turn.html;
    activeStreamRef.refs.widgetsWrap.innerHTML = "";
    if (activeStreamRef.refs.streamStatusSpan) {
      activeStreamRef.refs.streamStatusSpan.textContent = "Escalating to deep…";
    }
  }

  function handleStreamDone(ev) {
    if (!activeStreamRef) return;
    const html = ev.html || ev.renderedHtml || ev.rendered_html || "";
    const md = ev.markdown !== undefined ? ev.markdown : activeStreamRef.rawBuffer;
    const finishedTurnId = parsePositiveTurnId(ev.turnId || ev.id);

    activeStreamRef.turn.status = "completed";
    if (finishedTurnId !== null) {
      activeStreamRef.turn.turnId = finishedTurnId;
      activeStreamRef.turn.id = String(finishedTurnId);
      activeStreamRef.refs.article.dataset.turnId = String(finishedTurnId);
      state.latestTurnId = finishedTurnId;
    }
    if (ev.profile) {
      activeStreamRef.turn.profile = ev.profile;
      activeStreamRef.turn.badge = ev.profile;
    }
    if (typeof ev.durationMs === "number") {
      activeStreamRef.turn.durationMs = ev.durationMs;
    }
    if (Array.isArray(ev.denied)) {
      activeStreamRef.turn.denied = ev.denied;
    }
    if (ev.createdAt) {
      activeStreamRef.turn.createdAt = ev.createdAt;
      activeStreamRef.refs.tsSpan.textContent = ev.createdAt;
    }

    activeStreamRef.turn.markdown = md || "";
    const wc = activeStreamRef.turn.markdown.trim().split(/\s+/).filter(Boolean).length;
    activeStreamRef.turn.readingTime =
      wc > 0 ? Math.max(1, Math.ceil(wc / 210)) + " min read" : "";

    activeStreamRef.turn.html = html
      ? ensureMarkdownWrapper(html, activeStreamRef.turn.markdown)
      : ensureMarkdownWrapper("", activeStreamRef.turn.markdown);

    activeStreamRef.refs.body.innerHTML = activeStreamRef.turn.html;
    if (activeStreamRef.turn.denied.length > 0) {
      appendDeniedNotice(activeStreamRef.refs.body, activeStreamRef.turn.denied);
    }
    hydrateTurnContent(activeStreamRef.refs.body, activeStreamRef.turn.id);

    if (Array.isArray(ev.widgets) && window.HCWidgets) {
      activeStreamRef.turn.widgets = ev.widgets;
      activeStreamRef.refs.widgetsWrap.innerHTML = "";
      ev.widgets.forEach(function (w) {
        activeStreamRef.refs.widgetsWrap.appendChild(window.HCWidgets.renderWidget(w));
      });
    }

    activeStreamRef.turn.toolCalls = normalizeProvenance(activeStreamRef.turn);
    populateTurnInfoStrip(activeStreamRef.refs.infoStrip, activeStreamRef.turn);

    if (activeStreamRef.refs.streamIndicator) {
      activeStreamRef.refs.streamIndicator.remove();
    }

    state.thread.turns.push(activeStreamRef.turn);
    if (
      (!state.thread.title || state.thread.title === "Ask Computer") &&
      activeStreamRef.turn.prompt
    ) {
      state.thread.title = activeStreamRef.turn.prompt.slice(0, 68);
      if (dom.topbarTitle) dom.topbarTitle.textContent = state.thread.title;
    }

    activeStreamRef.refs.article.scrollIntoView({ behavior: "instant", block: "end" });
    activeStreamRef = null;
    syncComposerBusy(false);
    updateActivatedDayPlanDates(ev.activatedDayPlanDates || ev.activated_day_plan_dates || null);
  }

  function handleStreamError(ev) {
    const errMsg = String(ev.message || "Request failed.");
    if (activeStreamRef) {
      if (activeStreamRef.refs.streamIndicator) {
        activeStreamRef.refs.streamIndicator.remove();
      }
      if (!activeStreamRef.rawBuffer) {
        activeStreamRef.refs.body.innerHTML = "";
        const callout = el("aside", "callout callout-caution");
        callout.appendChild(el("div", "callout-title", "Request Failed"));
        const body = el("div", "callout-body");
        body.appendChild(el("p", "", errMsg));
        callout.appendChild(body);
        activeStreamRef.refs.body.appendChild(callout);
      }
      activeStreamRef = null;
    }
    syncComposerBusy(false);
  }

  /* ------------------------------------------------------------------------
     Inert-Hardened History & Schedules Drawer (2 Tabs + Keyboard List Nav)
     ------------------------------------------------------------------------ */
  function blurActiveInsideOverlay(container) {
    const active = document.activeElement;
    if (!active) return;
    if (
      active === dom.drawerSearch ||
      (container && typeof container.contains === "function" && container.contains(active))
    ) {
      if (typeof active.blur === "function") active.blur();
    }
  }

  function isHistoryDrawerOpen() {
    return Boolean(dom.historyDrawer && dom.historyDrawer.classList.contains("is-open"));
  }

  function isModalOpen() {
    return Boolean(
      (dom.vaultModal && !dom.vaultModal.hidden) ||
      (dom.keysModal && !dom.keysModal.hidden)
    );
  }

  function setDrawerTab(tab) {
    state.drawerTab = tab === "schedules" ? "schedules" : "history";
    drawerSelectedIdx = 0;
    if (dom.drawerTabHistory) {
      dom.drawerTabHistory.classList.toggle("is-active", state.drawerTab === "history");
    }
    if (dom.drawerTabSchedules) {
      dom.drawerTabSchedules.classList.toggle("is-active", state.drawerTab === "schedules");
    }
    if (dom.drawerSectionTitle) {
      dom.drawerSectionTitle.textContent =
        state.drawerTab === "schedules" ? "Scheduled Actions" : "Recent Chats";
    }
    if (dom.drawerSearch) {
      dom.drawerSearch.placeholder =
        state.drawerTab === "schedules"
          ? "Filter schedules… (/ to filter)"
          : "Search recall… (/ to filter)";
    }
    if (state.drawerTab === "schedules") {
      renderDrawerSchedules(dom.drawerSearch ? dom.drawerSearch.value.trim() : "");
      hostLoadSchedules();
    } else {
      const q = dom.drawerSearch ? dom.drawerSearch.value.trim() : "";
      if (!hostSearchHistory(q)) {
        renderHistoryResults(state.history);
      }
    }
  }

  function toggleHistoryDrawer(forceOpen, opts) {
    if (!dom.historyDrawer) return;
    const options = opts || {};
    const shouldOpen =
      typeof forceOpen === "boolean"
        ? forceOpen
        : !dom.historyDrawer.classList.contains("is-open");

    if (!shouldOpen) {
      blurActiveInsideOverlay(dom.historyDrawer);
    }

    dom.historyDrawer.classList.toggle("is-open", shouldOpen);
    dom.historyDrawer.inert = !shouldOpen;
    dom.historyDrawer.setAttribute("aria-hidden", shouldOpen ? "false" : "true");
    dom.drawerBackdrop.hidden = !shouldOpen;
    dom.historyBtn.classList.toggle("is-active", shouldOpen);

    if (shouldOpen) {
      drawerSelectedIdx = 0;
      if (options.tab) {
        setDrawerTab(options.tab);
      } else if (state.drawerTab === "schedules") {
        renderDrawerSchedules(dom.drawerSearch ? dom.drawerSearch.value.trim() : "");
        hostLoadSchedules();
      } else {
        const q =
          typeof options.query === "string"
            ? options.query
            : (dom.drawerSearch ? dom.drawerSearch.value.trim() : "");
        if (typeof options.query === "string" && dom.drawerSearch) {
          dom.drawerSearch.value = options.query;
        }
        if (!hostSearchHistory(q)) {
          renderHistoryResults(state.history);
        }
      }
      if (options.focusSearch && dom.drawerSearch) {
        requestAnimationFrame(function () {
          dom.drawerSearch.focus();
        });
      }
    }
  }

  function getDrawerItemNodes() {
    if (!dom.drawerHistoryList) return [];
    return Array.prototype.slice.call(
      dom.drawerHistoryList.querySelectorAll(".drawer-item")
    );
  }

  function syncDrawerSelection() {
    const items = getDrawerItemNodes();
    if (items.length === 0) {
      drawerSelectedIdx = 0;
      return;
    }
    if (drawerSelectedIdx < 0) drawerSelectedIdx = 0;
    if (drawerSelectedIdx >= items.length) drawerSelectedIdx = items.length - 1;
    items.forEach(function (node, idx) {
      const sel = idx === drawerSelectedIdx;
      node.classList.toggle("is-selected", sel);
      if (sel) {
        node.scrollIntoView({ block: "nearest" });
      }
    });
  }

  function moveDrawerSelection(delta) {
    const items = getDrawerItemNodes();
    if (items.length === 0) return;
    drawerSelectedIdx = (drawerSelectedIdx + delta + items.length) % items.length;
    syncDrawerSelection();
  }

  function activateSelectedDrawerItem() {
    const items = getDrawerItemNodes();
    if (items.length === 0) return;
    const node = items[drawerSelectedIdx] || items[0];
    if (node && typeof node.click === "function") {
      node.click();
    }
  }

  function renderHistoryResults(items) {
    const list = Array.isArray(items) ? items : [];
    state.history = list;
    if (!dom.drawerHistoryList || state.drawerTab !== "history") return;
    dom.drawerHistoryList.innerHTML = "";
    drawerSelectedIdx = 0;

    if (list.length === 0) {
      dom.drawerHistoryList.appendChild(
        el("div", "w-muted", "No matching turns in history.db.")
      );
      return;
    }

    list.forEach(function (entry, idx) {
      const entryTurnId = parsePositiveTurnId(entry.turnId || entry.turn_id || entry.id);
      const isCurrent =
        state.viewMode === "thread" &&
        entryTurnId !== null &&
        state.thread &&
        state.thread.turns.some(function (t) {
          return t.turnId === entryTurnId;
        });

      const btn = el("button", "drawer-item" + (isCurrent ? " is-active" : ""));
      btn.type = "button";
      const top = el("div", "drawer-item-top");
      top.appendChild(
        el("span", "", entry.title || entry.prompt || ("Turn #" + (entryTurnId || entry.id)))
      );
      const badgeText = entry.profile || entry.badge || entry.lane || "default";
      const badge = el("span", "profile-badge", badgeText);
      badge.dataset.badge = String(badgeText).toLowerCase();
      top.appendChild(badge);
      btn.appendChild(top);

      const snippet =
        entry.snippet || entry.answerSnippet || entry.createdAt || "";
      if (snippet) {
        btn.appendChild(el("div", "drawer-item-snippet", snippet));
      }
      if (entry.createdAt) {
        btn.appendChild(el("div", "w-muted", entry.createdAt));
      }

      btn.addEventListener("mouseenter", function () {
        drawerSelectedIdx = idx;
        syncDrawerSelection();
      });

      btn.addEventListener("click", function () {
        if (entryTurnId !== null) {
          pushJumpPoint();
          stopTtsIfSpeaking();
          hostOpenHistoryTurn(entryTurnId);
        }
        toggleHistoryDrawer(false);
      });

      dom.drawerHistoryList.appendChild(btn);
    });
    syncDrawerSelection();
  }

  function renderDrawerSchedules(filterQuery) {
    if (!dom.drawerHistoryList || state.drawerTab !== "schedules") return;
    dom.drawerHistoryList.innerHTML = "";
    drawerSelectedIdx = 0;

    const q = String(filterQuery || "").trim().toLowerCase();
    const list = (Array.isArray(state.schedules) ? state.schedules : []).filter(function (s) {
      if (!q) return true;
      const hay = [s.title, s.name, s.humanSchedule, s.scheduleSpec, s.status, s.mode]
        .join(" ")
        .toLowerCase();
      return hay.indexOf(q) !== -1;
    });

    if (list.length === 0) {
      dom.drawerHistoryList.appendChild(
        el("div", "w-muted", q ? "No matching schedules." : "No scheduled actions configured.")
      );
      return;
    }

    list.forEach(function (s, idx) {
      const target = s.name || String(s.id);
      const isCurrent =
        state.viewMode === "schedule_inspector" &&
        state.activeScheduleTarget === target;

      const btn = el("button", "drawer-item" + (isCurrent ? " is-active" : ""));
      btn.type = "button";
      const top = el("div", "drawer-item-top");
      top.appendChild(el("span", "", s.title || s.name));

      const pill = el("span", "schedule-status-pill", s.status || (s.enabled ? "active" : "paused"));
      pill.dataset.status = String(s.status || (s.enabled ? "active" : "paused")).toLowerCase();
      top.appendChild(pill);
      btn.appendChild(top);

      btn.appendChild(
        el("div", "drawer-item-snippet", (s.humanSchedule || s.scheduleSpec || "") + " · " + (s.mode || "agent"))
      );
      btn.appendChild(
        el(
          "div",
          "w-muted",
          s.enabled
            ? ("Next: " + (s.nextRunLocal || "—"))
            : "Paused"
        )
      );

      btn.addEventListener("mouseenter", function () {
        drawerSelectedIdx = idx;
        syncDrawerSelection();
      });

      btn.addEventListener("click", function () {
        openScheduleInspectorView(target);
        toggleHistoryDrawer(false);
      });

      dom.drawerHistoryList.appendChild(btn);
    });

    // Bottom shortcut row to open the full overview
    const allRow = el("button", "drawer-item");
    allRow.type = "button";
    const allTop = el("div", "drawer-item-top");
    allTop.appendChild(el("span", "", "⟳ Open All Schedules Overview"));
    allTop.appendChild(el("span", "kbd-hint", "S"));
    allRow.appendChild(allTop);
    allRow.addEventListener("click", function () {
      openSchedulesOverviewView();
      toggleHistoryDrawer(false);
    });
    dom.drawerHistoryList.appendChild(allRow);

    syncDrawerSelection();
  }

  /* ------------------------------------------------------------------------
     Modals: Vault / Skill Preview Sheet & Keyboard Shortcuts Cheatsheet
     ------------------------------------------------------------------------ */
  function closeModals() {
    blurActiveInsideOverlay(dom.vaultModal);
    blurActiveInsideOverlay(dom.keysModal);
    if (dom.vaultModal) dom.vaultModal.hidden = true;
    if (dom.keysModal) dom.keysModal.hidden = true;
    if (dom.modalBackdrop) dom.modalBackdrop.hidden = true;
  }

  function closeAllOverlays() {
    closeModals();
    toggleHistoryDrawer(false);
  }

  function openPreviewSheet(opts) {
    dom.vaultTitle.textContent = opts.title || "Preview";
    dom.vaultNote.textContent = opts.note || "";
    dom.vaultPre.textContent = opts.preview || "";

    dom.keysModal.hidden = true;
    dom.modalBackdrop.hidden = false;
    dom.vaultModal.hidden = false;
  }

  function openVaultSheet(ev) {
    if (ev.quiet && !ev.previewOnly) {
      return;
    }
    const path = ev.path || ev.targetPath || state.todayJournalPath;
    const saved = Boolean(ev.saved || ev.written);
    const preview =
      ev.preview || ev.markdown || ev.content || collectThreadMarkdown();

    openPreviewSheet({
      title: (saved ? "Saved to Vault · " : "Vault Preview · ") + path,
      note: saved
        ? "Written to target vault daily note."
        : (ev.message ||
            "Safe preview mode — set instructions.vault in config.toml to write directly to disk."),
      preview: preview
    });
  }

  function handleSkillTemplateSaved(ev) {
    const skillName = ev.skillName || ev.skill_name || "";
    const btns = Array.prototype.slice.call(
      document.querySelectorAll("[data-save-skill-btn]")
    );
    btns.forEach(function (btn) {
      if (!skillName || btn.dataset.saveSkillBtn === skillName) {
        btn.textContent = "Saved ✓";
      }
    });

    const path =
      ev.path ||
      ev.skillDir ||
      ("$XDG_CONFIG_HOME/hey-computer/skills/" + (skillName || "widget-custom") + "/SKILL.md");
    const saved = Boolean(ev.saved || ev.written);

    if (ev.preview || ev.skillMarkdown || ev.previewOnly) {
      openPreviewSheet({
        title: (saved ? "Saved Skill Template · " : "Skill Template Preview · ") + path,
        note:
          ev.message ||
          (saved
            ? "Skill template written (0700 dir, 0600 files) and ready for show_widget reuse."
            : "Safe preview of SKILL.md + template.html."),
        preview:
          ev.preview ||
          ((ev.skillMarkdown || "") +
            (ev.templateHtml ? "\n\n<!-- template.html -->\n" + ev.templateHtml : ""))
      });
    } else {
      showToast(ev.message || ("Saved skill template: " + (skillName || path)), "ok");
    }
  }

  function toggleKeysModal(forceOpen) {
    const shouldOpen =
      typeof forceOpen === "boolean" ? forceOpen : dom.keysModal.hidden;
    if (!shouldOpen) {
      blurActiveInsideOverlay(dom.keysModal);
    }
    dom.vaultModal.hidden = true;
    dom.keysModal.hidden = !shouldOpen;
    dom.modalBackdrop.hidden = !shouldOpen;
  }

  function saveCurrentToVault() {
    const title =
      state.viewMode === "schedule_inspector" && state.scheduleDetail && state.scheduleDetail.summary
        ? (state.scheduleDetail.summary.title || state.scheduleDetail.summary.name || "Schedule")
        : ((state.thread && state.thread.title) || "Answer");
    hostSaveVault(collectThreadMarkdown(), "append", title);
  }

  /* ------------------------------------------------------------------------
     Host Event Dispatcher (window.__hcDispatch)
     ------------------------------------------------------------------------ */
  function resolveCodeBlockForEvent(ev) {
    if (ev.blockId && codeBlocksById.has(String(ev.blockId))) {
      return codeBlocksById.get(String(ev.blockId));
    }
    if (ev.codeIndex !== undefined && codeBlocksById.has("idx:" + String(ev.codeIndex))) {
      return codeBlocksById.get("idx:" + String(ev.codeIndex));
    }
    if (ev.id !== undefined && codeBlocksById.has(String(ev.id))) {
      return codeBlocksById.get(String(ev.id));
    }
    return lastRunCodeBlock;
  }

  window.__hcDispatch = function (ev) {
    if (!ev || typeof ev !== "object") return;
    if (prevDispatch && prevDispatch !== window.__hcDispatch) {
      try {
        prevDispatch(ev);
      } catch (_) {}
    }

    switch (ev.type) {
      case "toast":
        showToast(ev.message || ev.text || "Done", ev.kind || ev.level || "ok");
        break;

      case "theme_changed": {
        const sysDark =
          typeof ev.systemDark === "boolean"
            ? ev.systemDark
            : (typeof ev.dark === "boolean" ? ev.dark : undefined);
        applyTheme(ev.forceTheme || ev.theme || state.theme, sysDark);
        break;
      }

      case "palette_changed":
        applyPalette(ev.palette && typeof ev.palette === "object" ? ev.palette : ev);
        break;

      case "focus_composer":
        if (dom.composerInput) {
          dom.composerInput.focus();
        }
        break;

      case "find_status":
        state.findFound = Boolean(ev.found);
        state.findCount = typeof ev.count === "number" ? ev.count : null;
        if (state.findQuery) {
          state.findActive = true;
        }
        updateFindCountUi();
        break;

      case "tts_state":
        setSpeakingState(Boolean(ev.speaking));
        break;

      case "schedules_loaded":
        state.schedules = Array.isArray(ev.schedules) ? ev.schedules : [];
        if (state.viewMode === "schedules_overview") {
          renderSchedulesOverview(true);
        }
        if (isHistoryDrawerOpen() && state.drawerTab === "schedules") {
          renderDrawerSchedules(dom.drawerSearch ? dom.drawerSearch.value.trim() : "");
        }
        break;

      case "schedule_loaded": {
        const detail = normalizeScheduleDetail(ev.detail, dropCapEnabled);
        if (detail) {
          renderScheduleInspector(detail, false);
        }
        break;
      }

      case "open_history":
        toggleHistoryDrawer(true, {
          tab: "history",
          query: typeof ev.query === "string" ? ev.query : undefined,
          focusSearch: true
        });
        break;

      case "code_run_started": {
        const block = resolveCodeBlockForEvent(ev);
        markCodeRunning(block, ev.lang || "snippet");
        break;
      }

      case "code_run_finished": {
        const block = resolveCodeBlockForEvent(ev);
        renderCodeFinished(block, ev);
        break;
      }

      case "stream_start":
        handleStreamStart(ev);
        break;

      case "stream_progress":
        handleStreamProgress(ev);
        break;

      case "stream_chunk":
        handleStreamChunk(ev);
        break;

      case "stream_widget":
        handleStreamWidget(ev);
        break;

      case "stream_reset":
        handleStreamReset();
        break;

      case "stream_done":
        handleStreamDone(ev);
        break;

      case "stream_error":
        handleStreamError(ev);
        break;

      case "history_results": {
        const results = ev.results || ev.items || ev.turns || [];
        renderHistoryResults(results);
        break;
      }

      case "thread_loaded": {
        activeStreamRef = null;
        syncComposerBusy(false);
        if (Array.isArray(ev.activatedDayPlanDates || ev.activated_day_plan_dates)) {
          state.activatedDayPlanDates = (
            ev.activatedDayPlanDates || ev.activated_day_plan_dates
          ).slice();
          window.__HC_ACTIVATED_DAY_PLAN_DATES__ = state.activatedDayPlanDates.slice();
        }
        state.thread = normalizeThread(ev.thread, dropCapEnabled);
        const latest = parsePositiveTurnId(ev.latestTurnId);
        if (latest !== null) {
          state.latestTurnId = latest;
        }
        renderActiveThread(ev.turnId);
        break;
      }

      case "widget_ack": {
        if (Array.isArray(ev.activatedDayPlanDates || ev.activated_day_plan_dates)) {
          updateActivatedDayPlanDates(
            ev.activatedDayPlanDates || ev.activated_day_plan_dates
          );
        }
        break;
      }

      case "vault_saved":
        openVaultSheet(ev);
        break;

      case "skill_template_saved":
        handleSkillTemplateSaved(ev);
        break;

      case "widget_error":
        window.dispatchEvent(new CustomEvent("hc:widget-error", { detail: ev }));
        showToast("Widget JS error caught by viewer boundary", "error");
        break;

      default:
        break;
    }
  };

  if (!rawHost) {
    window.addEventListener("error", function (errEv) {
      const detail = {
        message: errEv.message || "Uncaught ReferenceError",
        source: errEv.filename || "widget",
        line: errEv.lineno || 0,
        column: errEv.colno || 0,
        stack: errEv.error && errEv.error.stack ? errEv.error.stack : ""
      };
      window.dispatchEvent(new CustomEvent("hc:widget-error", { detail: detail }));
      showToast("Caught widget JS error: " + detail.message, "error");
    });
  }

  /* ------------------------------------------------------------------------
     Build Complete Application Shell inside <body>
     ------------------------------------------------------------------------ */
  function mountApp() {
    document.body.innerHTML = "";

    /* 1. Single-Row Top Editorial Bar */
    const topbar = el("header", "topbar");
    const topbarInner = el("div", "topbar-inner");
    const topbarLeft = el("div", "topbar-left");
    const topbarRight = el("div", "topbar-right");

    const historyBtn = el("button", "nav-btn");
    historyBtn.type = "button";
    historyBtn.title = "History & FTS5 Recall (h)";
    historyBtn.appendChild(el("span", "", "☰ History"));
    historyBtn.appendChild(el("span", "kbd-hint", "h"));
    historyBtn.addEventListener("click", function () {
      toggleHistoryDrawer();
    });
    dom.historyBtn = historyBtn;
    topbarLeft.appendChild(historyBtn);

    const schedulesBtn = el("button", "nav-btn");
    schedulesBtn.type = "button";
    schedulesBtn.title = "Scheduled Actions overview (Shift+S)";
    schedulesBtn.appendChild(el("span", "", "⟳ Schedules"));
    schedulesBtn.appendChild(el("span", "kbd-hint", "S"));
    schedulesBtn.addEventListener("click", function () {
      toggleSchedulesView();
    });
    dom.schedulesBtn = schedulesBtn;
    topbarLeft.appendChild(schedulesBtn);

    const topbarTitle = el("span", "topbar-title", "");
    dom.topbarTitle = topbarTitle;
    topbarLeft.appendChild(topbarTitle);

    function makeActionBtn(label, keyHint, title, onClick) {
      const b = el("button", "nav-btn");
      b.type = "button";
      b.title = title;
      b.appendChild(el("span", "", label));
      if (keyHint) b.appendChild(el("span", "kbd-hint", keyHint));
      b.addEventListener("click", onClick);
      return b;
    }

    const speakBtn = makeActionBtn(
      "Speak",
      "s",
      "Read selection or current turn aloud (s)",
      function () {
        toggleSpeakCurrent();
      }
    );
    dom.speakBtn = speakBtn;

    const copyBtn = makeActionBtn(
      "Copy",
      "y",
      "Copy selection or full Markdown answer (y / Y)",
      function () {
        copySelectionOrMarkdown(false);
      }
    );

    const vaultBtn = makeActionBtn(
      "Vault",
      "v",
      "Append answer to daily Vault note (v)",
      function () {
        saveCurrentToVault();
      }
    );

    const infoBtn = makeActionBtn(
      "Info",
      "I",
      "Toggle turn metadata, duration, and wallpaper palette (Shift+I)",
      function () {
        toggleTurnInfo();
      }
    );
    dom.infoBtn = infoBtn;

    const keysBtn = makeActionBtn("?", null, "Keyboard shortcuts (?)", function () {
      toggleKeysModal();
    });

    topbarRight.appendChild(speakBtn);
    topbarRight.appendChild(copyBtn);
    topbarRight.appendChild(vaultBtn);
    topbarRight.appendChild(infoBtn);
    topbarRight.appendChild(keysBtn);

    topbarInner.appendChild(topbarLeft);
    topbarInner.appendChild(topbarRight);
    topbar.appendChild(topbarInner);
    document.body.appendChild(topbar);

    /* 1b. Floating Flexoki In-Page Search Pill (`/` or `Ctrl-f`) */
    const findBar = el("div", "find-bar");
    findBar.hidden = true;
    findBar.setAttribute("role", "search");
    findBar.appendChild(el("span", "find-prefix", "/"));

    const findInput = el("input", "find-input");
    findInput.type = "text";
    findInput.placeholder = "Find in page… (smartcase)";
    findInput.setAttribute("aria-label", "Find in page");
    findInput.addEventListener("input", function () {
      const q = findInput.value;
      state.findQuery = q;
      if (!q) {
        state.findActive = false;
        state.findCount = null;
        state.findFound = null;
        updateFindCountUi();
        hostFindClose();
      } else {
        state.findActive = true;
        hostFindSearch(q);
      }
    });
    findInput.addEventListener("keydown", function (ev) {
      if (ev.key === "Escape") {
        ev.preventDefault();
        ev.stopPropagation();
        closeFindBar(true);
      } else if (ev.key === "Enter") {
        ev.preventDefault();
        ev.stopPropagation();
        if (!state.findQuery) {
          closeFindBar(true);
          return;
        }
        if (ev.shiftKey) {
          hostFindPrev();
        } else {
          if (
            findOriginScrollY !== null &&
            Math.abs(window.scrollY - findOriginScrollY) >= 24
          ) {
            pushJumpPoint(findOriginScrollY);
          }
          findOriginScrollY = null;
          findInput.blur();
        }
      }
    });

    const findCount = el("span", "find-count", "");
    const findPrevBtn = el("button", "find-btn", "‹");
    findPrevBtn.type = "button";
    findPrevBtn.title = "Previous match (N)";
    findPrevBtn.addEventListener("click", function () {
      if (state.findQuery) hostFindPrev();
    });

    const findNextBtn = el("button", "find-btn", "›");
    findNextBtn.type = "button";
    findNextBtn.title = "Next match (n)";
    findNextBtn.addEventListener("click", function () {
      if (state.findQuery) hostFindNext();
    });

    const findCloseBtn = el("button", "find-btn", "×");
    findCloseBtn.type = "button";
    findCloseBtn.title = "Clear highlights & close (Esc)";
    findCloseBtn.addEventListener("click", function () {
      closeFindBar(true);
    });

    findBar.appendChild(findInput);
    findBar.appendChild(findCount);
    findBar.appendChild(findPrevBtn);
    findBar.appendChild(findNextBtn);
    findBar.appendChild(findCloseBtn);
    document.body.appendChild(findBar);
    dom.findBar = findBar;
    dom.findInput = findInput;
    dom.findCount = findCount;

    /* 2. Main Reading Column */
    const readingShell = el("main", "reading-shell");
    const readingColumn = el("div", "reading-column");
    readingColumn.id = "reading-column";
    readingShell.appendChild(readingColumn);
    document.body.appendChild(readingShell);
    dom.readingColumn = readingColumn;

    /* 3. Clean Sticky Bottom Follow-Up Composer */
    const composerDock = el("footer", "composer-dock");
    const composerCard = el("form", "composer-card");

    const textarea = el("textarea", "composer-textarea");
    textarea.rows = 1;
    textarea.placeholder = DEFAULT_COMPOSER_PLACEHOLDER;
    textarea.addEventListener("blur", function () {
      if (!textarea.value.trim()) {
        textarea.placeholder = DEFAULT_COMPOSER_PLACEHOLDER;
      }
    });

    function autoResizeComposer() {
      textarea.style.height = "auto";
      textarea.style.height = Math.min(150, textarea.scrollHeight) + "px";
    }
    textarea.addEventListener("input", autoResizeComposer);
    textarea.addEventListener("keydown", function (ev) {
      if ((ev.metaKey || ev.ctrlKey) && ev.key === "Enter") {
        ev.preventDefault();
        composerCard.requestSubmit();
      } else if (ev.key === "Escape") {
        ev.preventDefault();
        ev.stopPropagation();
        textarea.blur();
      }
    });

    const sendBtn = el("button", "composer-send-btn", "Send ↵");
    sendBtn.type = "submit";
    dom.sendBtn = sendBtn;

    composerCard.addEventListener("submit", function (ev) {
      ev.preventDefault();
      if (activeStreamRef) return;
      const val = textarea.value.trim();
      if (!val) return;
      textarea.value = "";
      autoResizeComposer();
      window.sendPrompt(val);
    });

    composerCard.appendChild(textarea);
    composerCard.appendChild(sendBtn);
    composerDock.appendChild(composerCard);
    document.body.appendChild(composerDock);
    dom.composerInput = textarea;

    /* 4. Slide-Over History & Schedules Drawer (inert when closed) */
    const drawerBackdrop = el("div", "drawer-backdrop");
    drawerBackdrop.hidden = true;
    drawerBackdrop.addEventListener("click", function () {
      toggleHistoryDrawer(false);
    });
    dom.drawerBackdrop = drawerBackdrop;

    const historyDrawer = el("aside", "history-drawer");
    historyDrawer.inert = true;
    historyDrawer.setAttribute("aria-hidden", "true");
    historyDrawer.setAttribute("aria-label", "History and Scheduled Actions");

    const drawerHeader = el("div", "drawer-header");
    drawerHeader.appendChild(el("h2", "drawer-title", "History & Schedules"));
    const drawerClose = el("button", "w-btn", "Esc ×");
    drawerClose.type = "button";
    drawerClose.addEventListener("click", function () {
      toggleHistoryDrawer(false);
    });
    drawerHeader.appendChild(drawerClose);

    const drawerTabs = el("div", "drawer-tabs");
    const tabHistory = el("button", "drawer-tab is-active", "Recent Chats");
    tabHistory.type = "button";
    tabHistory.addEventListener("click", function () {
      setDrawerTab("history");
    });
    const tabSchedules = el("button", "drawer-tab", "⟳ Schedules");
    tabSchedules.type = "button";
    tabSchedules.addEventListener("click", function () {
      setDrawerTab("schedules");
    });
    drawerTabs.appendChild(tabHistory);
    drawerTabs.appendChild(tabSchedules);
    dom.drawerTabHistory = tabHistory;
    dom.drawerTabSchedules = tabSchedules;

    const searchWrap = el("div", "drawer-search-wrap");
    const searchInput = el("input", "drawer-search-input");
    searchInput.type = "search";
    searchInput.placeholder = "Search recall… (/ to filter)";
    let searchTimer = null;
    searchInput.addEventListener("input", function () {
      clearTimeout(searchTimer);
      const q = searchInput.value.trim();
      if (state.drawerTab === "schedules") {
        renderDrawerSchedules(q);
        return;
      }
      searchTimer = setTimeout(function () {
        hostSearchHistory(q);
      }, 130);
    });
    searchInput.addEventListener("keydown", function (ev) {
      if (ev.key === "Escape") {
        ev.preventDefault();
        ev.stopPropagation();
        searchInput.blur();
        toggleHistoryDrawer(false);
      } else if (ev.key === "ArrowDown" || (ev.ctrlKey && ev.key.toLowerCase() === "n")) {
        ev.preventDefault();
        ev.stopPropagation();
        moveDrawerSelection(1);
      } else if (ev.key === "ArrowUp" || (ev.ctrlKey && ev.key.toLowerCase() === "p")) {
        ev.preventDefault();
        ev.stopPropagation();
        moveDrawerSelection(-1);
      } else if (ev.key === "Enter") {
        ev.preventDefault();
        ev.stopPropagation();
        activateSelectedDrawerItem();
      }
    });
    searchWrap.appendChild(searchInput);
    dom.drawerSearch = searchInput;

    const drawerContent = el("div", "drawer-content");
    const recSec = el("div", "");
    const secTitle = el("div", "drawer-section-title", "Recent Chats");
    dom.drawerSectionTitle = secTitle;
    recSec.appendChild(secTitle);
    const recList = el("div", "drawer-list");
    recSec.appendChild(recList);
    dom.drawerHistoryList = recList;

    drawerContent.appendChild(recSec);
    historyDrawer.appendChild(drawerHeader);
    historyDrawer.appendChild(drawerTabs);
    historyDrawer.appendChild(searchWrap);
    historyDrawer.appendChild(drawerContent);

    document.body.appendChild(drawerBackdrop);
    document.body.appendChild(historyDrawer);
    dom.historyDrawer = historyDrawer;

    /* 5. Shared Modal Backdrop + Vault/Skill Preview Sheet + Shortcuts Cheatsheet */
    const modalBackdrop = el("div", "modal-backdrop");
    modalBackdrop.hidden = true;
    modalBackdrop.addEventListener("click", closeAllOverlays);
    dom.modalBackdrop = modalBackdrop;
    document.body.appendChild(modalBackdrop);

    const vaultModal = el("section", "modal-sheet");
    vaultModal.hidden = true;
    const vHead = el("div", "modal-header");
    const vTitle = el("h2", "modal-title", "Vault Preview");
    const vClose = el("button", "w-btn", "Esc ×");
    vClose.type = "button";
    vClose.addEventListener("click", closeAllOverlays);
    vHead.appendChild(vTitle);
    vHead.appendChild(vClose);

    const vBody = el("div", "modal-body");
    const vNote = el("div", "w-muted", "");
    const vPre = el("pre", "vault-preview-pre", "");
    vBody.appendChild(vNote);
    vBody.appendChild(vPre);

    const vFoot = el("div", "modal-footer");
    const vCopyBtn = el("button", "w-btn w-btn-primary", "Copy");
    vCopyBtn.type = "button";
    vCopyBtn.addEventListener("click", function () {
      hostCopy(vPre.textContent || "", "Copied preview");
    });
    vFoot.appendChild(vCopyBtn);

    vaultModal.appendChild(vHead);
    vaultModal.appendChild(vBody);
    vaultModal.appendChild(vFoot);
    document.body.appendChild(vaultModal);
    dom.vaultModal = vaultModal;
    dom.vaultTitle = vTitle;
    dom.vaultNote = vNote;
    dom.vaultPre = vPre;

    const keysModal = el("section", "modal-sheet");
    keysModal.hidden = true;
    const kHead = el("div", "modal-header");
    kHead.appendChild(el("h2", "modal-title", "Ask Computer — VIM & Viewer Shortcuts"));
    const kClose = el("button", "w-btn", "Esc ×");
    kClose.type = "button";
    kClose.addEventListener("click", closeAllOverlays);
    kHead.appendChild(kClose);

    const kBody = el("div", "modal-body");
    const kGrid = el("div", "keys-grid");
    const shortcuts = [
      ["j / k", "Scroll down / up"],
      ["Ctrl+e / Ctrl+y", "1-line viewport nudge"],
      ["Ctrl+d / Ctrl+u", "Half-page down / up"],
      ["g g / G", "Jump to top / bottom"],
      ["Ctrl+o / Ctrl+i", "Jump list back / forward"],
      ["/  or  Ctrl+f", "In-page search (smartcase)"],
      ["n / N", "Next / previous match"],
      ["i / c", "Focus follow-up composer"],
      ["Ctrl+Enter", "Send follow-up prompt"],
      ["s", "Speak / stop TTS"],
      ["S", "Scheduled Actions view"],
      ["h", "History & Schedules drawer"],
      ["y / Y", "Copy selection / full MD"],
      ["v", "Save to Vault daily note"],
      ["I", "Toggle Turn Info & palette"],
      ["D", "Toggle editorial Drop Cap"],
      ["?", "Toggle shortcuts help"],
      ["Esc / q", "Unwind mode or close window"]
    ];
    shortcuts.forEach(function (pair) {
      const row = el("div", "key-row");
      row.appendChild(el("span", "", pair[1]));
      const combo = el("span", "key-combo");
      combo.appendChild(el("kbd", "", pair[0]));
      row.appendChild(combo);
      kGrid.appendChild(row);
    });
    kBody.appendChild(kGrid);
    keysModal.appendChild(kHead);
    keysModal.appendChild(kBody);
    document.body.appendChild(keysModal);
    dom.keysModal = keysModal;

    /* 6. Toast Stack */
    const toastStack = el("div", "toast-stack");
    document.body.appendChild(toastStack);
    dom.toastStack = toastStack;

    /* Initialize Theme, Wallpaper Palette, Drop Cap, Info, and Initial View */
    applyTheme(state.theme, state.systemDark);
    if (state.palette) {
      applyPalette(state.palette);
    }
    syncDropCap(dropCapEnabled);
    syncTurnInfo(false);
    setSpeakingState(false);

    if (state.initialScrollY > 0) {
      pendingRestoreScrollY = state.initialScrollY;
    }

    if (state.viewMode === "schedule_inspector" && state.scheduleDetail) {
      renderScheduleInspector(state.scheduleDetail, false);
    } else if (state.viewMode === "schedules_overview") {
      renderSchedulesOverview(false);
    } else {
      renderActiveThread();
    }
    renderHistoryResults(state.history);

    if (state.initialScrollY > 0) {
      const targetY = state.initialScrollY;
      const markUserScrolled = function () {
        userScrolledSinceBoot = true;
      };
      window.addEventListener("wheel", markUserScrolled, { once: true, passive: true });
      window.addEventListener("touchstart", markUserScrolled, { once: true, passive: true });
      window.addEventListener("keydown", markUserScrolled, { once: true });

      requestAnimationFrame(function () {
        if (!userScrolledSinceBoot) {
          window.scrollTo({ top: targetY, behavior: "instant" });
        }
        bootRestoreComplete = true;
        scheduleSaveViewState(false);
      });

      if (document.fonts && document.fonts.ready && typeof document.fonts.ready.then === "function") {
        document.fonts.ready.then(function () {
          if (!userScrolledSinceBoot) {
            window.scrollTo({ top: targetY, behavior: "instant" });
            scheduleSaveViewState(false);
          }
        });
      }
    } else {
      bootRestoreComplete = true;
    }

    window.addEventListener(
      "scroll",
      function () {
        scheduleSaveViewState(false);
      },
      { passive: true }
    );
    window.addEventListener("pagehide", function () {
      scheduleSaveViewState(true);
    });
    window.addEventListener("beforeunload", function () {
      scheduleSaveViewState(true);
    });

    if (state.openHistory || state.recallQuery) {
      toggleHistoryDrawer(true, {
        tab: "history",
        query: state.recallQuery || "",
        focusSearch: true
      });
    } else if (state.focusComposer && dom.composerInput) {
      requestAnimationFrame(function () {
        dom.composerInput.focus();
      });
    }

    if (state.initialFindQuery) {
      requestAnimationFrame(function () {
        openFindBar(state.initialFindQuery);
      });
    }
  }

  /* ------------------------------------------------------------------------
     Global Keyboard Navigation (5-step Escape + VIM Motions + inert-hardened)
     ------------------------------------------------------------------------ */
  function resolveEventTarget(evOrTarget) {
    if (!evOrTarget) return null;
    if (typeof evOrTarget.composedPath === "function") {
      const path = evOrTarget.composedPath();
      if (path && path.length > 0) return path[0];
      return evOrTarget.target || null;
    }
    if (evOrTarget.target && !evOrTarget.tagName) {
      return evOrTarget.target;
    }
    return evOrTarget;
  }

  function isTextEditable(evOrTarget) {
    const target = resolveEventTarget(evOrTarget);
    if (!target || !target.tagName) return false;
    if (typeof target.closest === "function") {
      if (target.closest("[inert], [hidden], .history-drawer:not(.is-open)")) {
        return false;
      }
    }
    const tag = target.tagName.toLowerCase();
    if (tag === "textarea" || tag === "select") {
      return true;
    }
    if (tag === "input") {
      const inputType = String(target.type || "text").toLowerCase();
      return !NON_TEXT_INPUT_TYPES[inputType];
    }
    return Boolean(target.isContentEditable);
  }

  window.addEventListener("keydown", function (ev) {
    /* Exact 5-step Escape precedence chain */
    if (ev.key === "Escape") {
      // Step 1: Focused text-editable input
      const eventEditable = isTextEditable(ev) ? resolveEventTarget(ev) : null;
      const activeEditable =
        eventEditable || (isTextEditable(document.activeElement) ? document.activeElement : null);
      if (activeEditable) {
        ev.preventDefault();
        ev.stopPropagation();
        if (activeEditable === dom.findInput) {
          closeFindBar(true);
        } else if (activeEditable === dom.drawerSearch) {
          activeEditable.blur();
          toggleHistoryDrawer(false);
        } else {
          if (typeof activeEditable.blur === "function") activeEditable.blur();
        }
        return;
      }

      // Step 2: Open modal (`vaultModal` / `keysModal`)
      if (isModalOpen()) {
        ev.preventDefault();
        ev.stopPropagation();
        closeModals();
        return;
      }

      // Step 3: Open slide-over drawer (`historyDrawer`)
      if (isHistoryDrawerOpen()) {
        ev.preventDefault();
        ev.stopPropagation();
        toggleHistoryDrawer(false);
        return;
      }

      // Step 4: Active `/` search bar or active FindController highlights
      if (isFindBarOpen() || state.findActive) {
        ev.preventDefault();
        ev.stopPropagation();
        closeFindBar(true);
        return;
      }

      // Step 5: Normal reading state -> close window
      ev.preventDefault();
      ev.stopPropagation();
      hostCloseWindow();
      return;
    }

    const lowerKey = typeof ev.key === "string" ? ev.key.toLowerCase() : "";

    /* Ctrl-* bindings MUST run BEFORE the modifier early-return */
    if (ev.ctrlKey && !ev.altKey && !ev.metaKey) {
      if (lowerKey === "q" || lowerKey === "w") {
        ev.preventDefault();
        hostCloseWindow();
        return;
      }
      if (lowerKey === "f" && !isModalOpen()) {
        ev.preventDefault();
        if (isHistoryDrawerOpen() && dom.drawerSearch) {
          dom.drawerSearch.focus();
          dom.drawerSearch.select();
        } else {
          openFindBar();
        }
        return;
      }

      if (isHistoryDrawerOpen() && !isModalOpen()) {
        if (lowerKey === "n") {
          ev.preventDefault();
          moveDrawerSelection(1);
          return;
        }
        if (lowerKey === "p") {
          ev.preventDefault();
          moveDrawerSelection(-1);
          return;
        }
      }

      if (!isTextEditable(ev) && !isModalOpen() && !isHistoryDrawerOpen()) {
        if (lowerKey === "e") {
          ev.preventDefault();
          cancelScrollAnim();
          window.scrollBy({ top: 36, behavior: "instant" });
          return;
        }
        if (lowerKey === "y") {
          ev.preventDefault();
          cancelScrollAnim();
          window.scrollBy({ top: -36, behavior: "instant" });
          return;
        }
        if (lowerKey === "d") {
          ev.preventDefault();
          vimScrollBy(halfPageDelta(), ev.repeat);
          return;
        }
        if (lowerKey === "u") {
          ev.preventDefault();
          vimScrollBy(-halfPageDelta(), ev.repeat);
          return;
        }
        if (lowerKey === "o") {
          ev.preventDefault();
          jumpListBack();
          return;
        }
        if (lowerKey === "i") {
          ev.preventDefault();
          jumpListForward();
          return;
        }
      }
    }

    if (isTextEditable(ev) || ev.metaKey || ev.ctrlKey || ev.altKey) {
      return;
    }

    /* Modal-open guard */
    if (isModalOpen()) {
      if (ev.key === "?" && dom.keysModal && !dom.keysModal.hidden) {
        ev.preventDefault();
        toggleKeysModal(false);
      } else if (ev.key === "q") {
        ev.preventDefault();
        closeModals();
      }
      return;
    }

    /* Drawer-open keyboard navigation */
    if (isHistoryDrawerOpen()) {
      switch (ev.key) {
        case "/":
          ev.preventDefault();
          if (dom.drawerSearch) {
            dom.drawerSearch.focus();
            dom.drawerSearch.select();
          }
          return;
        case "ArrowDown":
        case "j":
          ev.preventDefault();
          moveDrawerSelection(1);
          return;
        case "ArrowUp":
        case "k":
          ev.preventDefault();
          moveDrawerSelection(-1);
          return;
        case "Enter":
          ev.preventDefault();
          activateSelectedDrawerItem();
          return;
        case "h":
        case "q":
          ev.preventDefault();
          toggleHistoryDrawer(false);
          return;
        default:
          return;
      }
    }

    /* Normal-mode VIM & Viewer bindings */
    switch (ev.key) {
      case "j":
        ev.preventDefault();
        vimScrollBy(64, ev.repeat);
        break;
      case "k":
        ev.preventDefault();
        vimScrollBy(-64, ev.repeat);
        break;
      case "G":
        ev.preventDefault();
        pushJumpPoint();
        cancelScrollAnim();
        window.scrollTo({ top: maxScrollY(), behavior: "instant" });
        break;
      case "g": {
        const now = Date.now();
        if (now - lastGKeyTime < 380) {
          ev.preventDefault();
          pushJumpPoint();
          cancelScrollAnim();
          window.scrollTo({ top: 0, behavior: "instant" });
          lastGKeyTime = 0;
        } else {
          lastGKeyTime = now;
        }
        break;
      }
      case "/":
        ev.preventDefault();
        openFindBar();
        break;
      case "n":
        if (state.findQuery) {
          ev.preventDefault();
          hostFindNext();
        }
        break;
      case "N":
        if (state.findQuery) {
          ev.preventDefault();
          hostFindPrev();
        }
        break;
      case "i":
      case "c":
        ev.preventDefault();
        if (dom.composerInput) {
          dom.composerInput.focus();
        }
        break;
      case "I":
        ev.preventDefault();
        toggleTurnInfo();
        break;
      case "D":
        ev.preventDefault();
        toggleDropCap();
        break;
      case "s":
        ev.preventDefault();
        toggleSpeakCurrent();
        break;
      case "S":
        ev.preventDefault();
        toggleSchedulesView();
        break;
      case "h":
        ev.preventDefault();
        toggleHistoryDrawer();
        break;
      case "y":
        ev.preventDefault();
        copySelectionOrMarkdown(false);
        break;
      case "Y":
        ev.preventDefault();
        copySelectionOrMarkdown(true);
        break;
      case "v":
        ev.preventDefault();
        saveCurrentToVault();
        break;
      case "?":
        ev.preventDefault();
        toggleKeysModal();
        break;
      case "q":
        ev.preventDefault();
        hostCloseWindow();
        break;
      default:
        break;
    }
  });

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", mountApp);
  } else {
    mountApp();
  }
})();
