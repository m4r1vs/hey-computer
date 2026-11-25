/* ==========================================================================
   Ask Computer — Editorial Answer Viewer Controller
   Prose-first layout, inert-hardened History & FTS5 Recall drawer, live
   wallpaper palette sync, Info (i) metadata toggle, and live socket streaming.
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
    custom_html: ["mcp-ui: show_widget"],
    show_widget: ["mcp-ui: show_widget"],
    skill_template_draft: ["mcp-ui: propose_widget_template"]
  };

  const VALID_THEMES = ["auto", "light", "dark"];

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

  function hostCloseWindow() {
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
      badge: t.badge || t.profile || threadDefaults.badge || "default",
      profile: t.profile || "",
      lane: t.lane || threadDefaults.lane || "interactive",
      origin: t.origin || "cli",
      scheduleId: t.scheduleId !== undefined ? t.scheduleId : (t.schedule_id || null),
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

    return {
      theme: VALID_THEMES.indexOf(bootTheme) !== -1 ? bootTheme : "auto",
      systemDark:
        typeof boot.systemDark === "boolean"
          ? boot.systemDark
          : (typeof boot.system_dark === "boolean" ? boot.system_dark : null),
      palette: normalizePalette(boot.palette),
      dropCap: dropCap,
      focusComposer: Boolean(boot.focusComposer || boot.focus_composer),
      todayJournalPath: boot.todayJournalPath || localTodayJournalPath(),
      thread: thread,
      latestTurnId: latestTurnId,
      history: Array.isArray(boot.history || boot.recentTurns || boot.recent_turns)
        ? (boot.history || boot.recentTurns || boot.recent_turns)
        : []
    };
  }

  /* ------------------------------------------------------------------------
     Application State & DOM References
     ------------------------------------------------------------------------ */
  const state = normalizeBootPayload(window.__HC_BOOT__);
  let dropCapEnabled = state.dropCap;
  let showTurnInfo = false;

  const codeBlocksById = new Map();
  let lastRunCodeBlock = null;
  let activeStreamRef = null;
  let lastGKeyTime = 0;

  const dom = {};

  function collectThreadMarkdown() {
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

  function syncComposerBusy(isBusy) {
    if (dom.sendBtn) {
      dom.sendBtn.disabled = Boolean(isBusy);
      dom.sendBtn.textContent = isBusy ? "Streaming…" : "Send ↵";
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
     Markdown Content Hydration (Code Copy/Run + Disarmed Images)
     ------------------------------------------------------------------------ */
  function hydrateTurnContent(container, turnIdPrefix) {
    const links = Array.prototype.slice.call(container.querySelectorAll("a"));
    links.forEach(function (a) {
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
      if (/^https?:\/\//i.test(a.getAttribute("href") || "")) {
        a.target = "_blank";
        a.rel = "noopener noreferrer";
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
     Turn Info Metadata Strip (`Info i`)
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

    if (turn.prompt) {
      const promptBox = el("div", "turn-prompt-box");
      promptBox.appendChild(el("p", "turn-prompt-text", turn.prompt));
      header.appendChild(promptBox);
    }

    const infoStrip = el("div", "turn-info-strip");
    populateTurnInfoStrip(infoStrip, turn);
    header.appendChild(infoStrip);

    article.appendChild(header);

    const body = el("div", "turn-body");
    body.innerHTML = turn.html || '<div class="markdown-content"></div>';
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
        "Type a question below to begin a conversation, or press h to search past answers."
      )
    );
    dom.readingColumn.appendChild(empty);
  }

  function renderActiveThread(scrollTargetTurnId) {
    if (!dom.readingColumn) return;
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

    if (scrollEl && thread.turns.length > 1) {
      scrollEl.scrollIntoView({ behavior: "smooth", block: "start" });
    } else {
      window.scrollTo({ top: 0, behavior: "smooth" });
    }
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

    const options = opts && typeof opts === "object" ? opts : {};
    const profile = options.profile || "";
    const explicitReplyTo = parsePositiveTurnId(options.replyTo);
    const replyTo =
      explicitReplyTo !== null ? explicitReplyTo : state.latestTurnId;

    clearEmptyPlaceholder();

    const tempTurn = {
      id: "stream-" + Date.now(),
      turnId: null,
      prompt: cleanText,
      badge: profile || "default",
      profile: profile || "",
      lane: "interactive",
      origin: "typed",
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
    refs.article.scrollIntoView({ behavior: "smooth", block: "end" });

    const payload = {
      prompt: cleanText,
      text: cleanText,
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
      clearEmptyPlaceholder();
      const tempTurn = {
        id: "stream-" + Date.now(),
        turnId: null,
        prompt: ev.prompt || "Follow-up",
        badge: ev.profile || "default",
        profile: ev.profile || "",
        lane: "interactive",
        origin: "typed",
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
      refs.article.scrollIntoView({ behavior: "smooth", block: "end" });
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

    clearEmptyPlaceholder();
    const turn = {
      id: String(incomingId),
      turnId: parsedNum,
      prompt: ev.prompt || "Follow-up",
      badge: ev.profile || ev.badge || "default",
      profile: ev.profile || "",
      lane: "interactive",
      origin: "typed",
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
    refs.article.scrollIntoView({ behavior: "smooth", block: "end" });
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

    activeStreamRef.refs.article.scrollIntoView({ behavior: "smooth", block: "end" });
    activeStreamRef = null;
    syncComposerBusy(false);
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
     Inert-Hardened History & FTS5 Recall Drawer
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

  function toggleHistoryDrawer(forceOpen) {
    if (!dom.historyDrawer) return;
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
      const q = dom.drawerSearch ? dom.drawerSearch.value.trim() : "";
      if (!hostSearchHistory(q)) {
        renderHistoryResults(state.history);
      }
    }
  }

  function renderHistoryResults(items) {
    if (!dom.drawerHistoryList) return;
    dom.drawerHistoryList.innerHTML = "";

    const list = Array.isArray(items) ? items : [];
    state.history = list;

    if (list.length === 0) {
      dom.drawerHistoryList.appendChild(
        el("div", "w-muted", "No matching turns in history.db.")
      );
      return;
    }

    list.forEach(function (entry) {
      const entryTurnId = parsePositiveTurnId(entry.turnId || entry.turn_id || entry.id);
      const isCurrent =
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

      btn.addEventListener("click", function () {
        if (entryTurnId !== null) {
          hostOpenHistoryTurn(entryTurnId);
        }
        toggleHistoryDrawer(false);
      });

      dom.drawerHistoryList.appendChild(btn);
    });
  }

  /* ------------------------------------------------------------------------
     Modals: Vault / Skill Preview Sheet & Keyboard Shortcuts Cheatsheet
     ------------------------------------------------------------------------ */
  function closeAllOverlays() {
    blurActiveInsideOverlay(dom.historyDrawer);
    blurActiveInsideOverlay(dom.vaultModal);
    blurActiveInsideOverlay(dom.keysModal);
    toggleHistoryDrawer(false);
    if (dom.vaultModal) dom.vaultModal.hidden = true;
    if (dom.keysModal) dom.keysModal.hidden = true;
    if (dom.modalBackdrop) dom.modalBackdrop.hidden = true;
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
        state.thread = normalizeThread(ev.thread, dropCapEnabled);
        const latest = parsePositiveTurnId(ev.latestTurnId);
        if (latest !== null) {
          state.latestTurnId = latest;
        }
        renderActiveThread(ev.turnId);
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

    const copyBtn = makeActionBtn("Copy", "y", "Copy full Markdown answer (y)", function () {
      hostCopy(collectThreadMarkdown(), "Copied full Markdown answer");
    });

    const vaultBtn = makeActionBtn(
      "Save to Vault",
      "v",
      "Append answer to daily Vault note (v)",
      function () {
        hostSaveVault(
          collectThreadMarkdown(),
          "append",
          (state.thread && state.thread.title) || "Answer"
        );
      }
    );

    const infoBtn = makeActionBtn(
      "Info",
      "i",
      "Toggle turn metadata, duration, and wallpaper palette (i)",
      function () {
        toggleTurnInfo();
      }
    );
    dom.infoBtn = infoBtn;

    const keysBtn = makeActionBtn("?", null, "Keyboard shortcuts (?)", function () {
      toggleKeysModal();
    });

    topbarRight.appendChild(copyBtn);
    topbarRight.appendChild(vaultBtn);
    topbarRight.appendChild(infoBtn);
    topbarRight.appendChild(keysBtn);

    topbarInner.appendChild(topbarLeft);
    topbarInner.appendChild(topbarRight);
    topbar.appendChild(topbarInner);
    document.body.appendChild(topbar);

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
    textarea.placeholder =
      "Ask Computer a follow-up… (/ to focus, Ctrl+Enter to send)";

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

    /* 4. Slide-Over History & FTS5 Recall Drawer (inert when closed) */
    const drawerBackdrop = el("div", "drawer-backdrop");
    drawerBackdrop.hidden = true;
    drawerBackdrop.addEventListener("click", function () {
      toggleHistoryDrawer(false);
    });
    dom.drawerBackdrop = drawerBackdrop;

    const historyDrawer = el("aside", "history-drawer");
    historyDrawer.inert = true;
    historyDrawer.setAttribute("aria-hidden", "true");
    historyDrawer.setAttribute("aria-label", "History and Local Recall");

    const drawerHeader = el("div", "drawer-header");
    drawerHeader.appendChild(el("h2", "drawer-title", "History & Recall"));
    const drawerClose = el("button", "w-btn", "Esc ×");
    drawerClose.type = "button";
    drawerClose.addEventListener("click", function () {
      toggleHistoryDrawer(false);
    });
    drawerHeader.appendChild(drawerClose);

    const searchWrap = el("div", "drawer-search-wrap");
    const searchInput = el("input", "drawer-search-input");
    searchInput.type = "search";
    searchInput.placeholder = "Search recall… (/ to filter)";
    let searchTimer = null;
    searchInput.addEventListener("input", function () {
      clearTimeout(searchTimer);
      const q = searchInput.value.trim();
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
      }
    });
    searchWrap.appendChild(searchInput);
    dom.drawerSearch = searchInput;

    const drawerContent = el("div", "drawer-content");
    const recSec = el("div", "");
    recSec.appendChild(el("div", "drawer-section-title", "Recent Turns"));
    const recList = el("div", "drawer-list");
    recSec.appendChild(recList);
    dom.drawerHistoryList = recList;

    drawerContent.appendChild(recSec);
    historyDrawer.appendChild(drawerHeader);
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
    kHead.appendChild(el("h2", "modal-title", "Ask Computer — Keyboard Shortcuts"));
    const kClose = el("button", "w-btn", "Esc ×");
    kClose.type = "button";
    kClose.addEventListener("click", closeAllOverlays);
    kHead.appendChild(kClose);

    const kBody = el("div", "modal-body");
    const kGrid = el("div", "keys-grid");
    const shortcuts = [
      ["j / k", "Scroll down / up"],
      ["g g / G", "Jump to top / bottom"],
      ["/", "Focus composer (or recall filter)"],
      ["Ctrl+Enter", "Send follow-up prompt"],
      ["h", "Toggle History & Recall"],
      ["y", "Copy full Markdown answer"],
      ["v", "Save to Vault daily note"],
      ["i", "Toggle turn Info & palette"],
      ["d", "Toggle editorial Drop Cap"],
      ["?", "Toggle shortcuts help"],
      ["q / Esc", "Close overlay or window"]
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

    /* Initialize Auto Theme, Wallpaper Palette, Drop Cap, Info, and Thread */
    applyTheme(state.theme, state.systemDark);
    if (state.palette) {
      applyPalette(state.palette);
    }
    syncDropCap(dropCapEnabled);
    syncTurnInfo(false);
    renderActiveThread();
    renderHistoryResults(state.history);

    if (state.focusComposer && dom.composerInput) {
      requestAnimationFrame(function () {
        dom.composerInput.focus();
      });
    }
  }

  /* ------------------------------------------------------------------------
     Global Keyboard Navigation (inert-hardened)
     ------------------------------------------------------------------------ */
  function isEditableTarget(target) {
    if (!target || !target.tagName) return false;
    if (typeof target.closest === "function") {
      if (target.closest("[inert], [hidden], .history-drawer:not(.is-open)")) {
        return false;
      }
    }
    const tag = target.tagName.toLowerCase();
    return (
      tag === "input" ||
      tag === "textarea" ||
      tag === "select" ||
      Boolean(target.isContentEditable)
    );
  }

  window.addEventListener("keydown", function (ev) {
    if (ev.key === "Escape") {
      const anyOpen =
        isHistoryDrawerOpen() ||
        (dom.vaultModal && !dom.vaultModal.hidden) ||
        (dom.keysModal && !dom.keysModal.hidden);
      if (anyOpen) {
        ev.preventDefault();
        closeAllOverlays();
      }
      return;
    }

    if (isEditableTarget(ev.target) || ev.metaKey || ev.ctrlKey || ev.altKey) {
      return;
    }

    switch (ev.key) {
      case "/":
        ev.preventDefault();
        if (isHistoryDrawerOpen() && dom.drawerSearch) {
          dom.drawerSearch.focus();
        } else if (dom.composerInput) {
          dom.composerInput.focus();
        }
        break;
      case "h":
        ev.preventDefault();
        toggleHistoryDrawer();
        break;
      case "y":
        ev.preventDefault();
        hostCopy(collectThreadMarkdown(), "Copied full Markdown answer");
        break;
      case "v":
        ev.preventDefault();
        hostSaveVault(
          collectThreadMarkdown(),
          "append",
          (state.thread && state.thread.title) || "Answer"
        );
        break;
      case "i":
        ev.preventDefault();
        toggleTurnInfo();
        break;
      case "d":
        ev.preventDefault();
        toggleDropCap();
        break;
      case "?":
        ev.preventDefault();
        toggleKeysModal();
        break;
      case "j":
        window.scrollBy({ top: 120, behavior: "smooth" });
        break;
      case "k":
        window.scrollBy({ top: -120, behavior: "smooth" });
        break;
      case "G":
        window.scrollTo({ top: document.body.scrollHeight, behavior: "smooth" });
        break;
      case "g": {
        const now = Date.now();
        if (now - lastGKeyTime < 380) {
          window.scrollTo({ top: 0, behavior: "smooth" });
          lastGKeyTime = 0;
        } else {
          lastGKeyTime = now;
        }
        break;
      }
      case "q": {
        const overlayOpen =
          isHistoryDrawerOpen() ||
          (dom.vaultModal && !dom.vaultModal.hidden) ||
          (dom.keysModal && !dom.keysModal.hidden);
        if (overlayOpen) {
          closeAllOverlays();
        } else {
          hostCloseWindow();
        }
        break;
      }
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
