/* ==========================================================================
   Ask Computer — Calm, Low-Clutter Structured MCP & show_widget Renderers (v2)
   Exports: window.HCWidgets = { renderWidget(widget) }
   ========================================================================== */

(function () {
  "use strict";

  function el(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  }

  function triggerPrompt(text, opts) {
    if (typeof window.sendPrompt === "function") {
      window.sendPrompt(text, opts || {});
      return;
    }
    if (window.heyComputer && typeof window.heyComputer.sendPrompt === "function") {
      window.heyComputer.sendPrompt(text, opts || {});
    }
  }

  function hostPost(msg) {
    const hc = window.heyComputer;
    if (hc && typeof hc.post === "function") {
      hc.post(msg);
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

  function emitWidgetAction(widgetKind, action, payload) {
    const hc = window.heyComputer;
    if (hc && typeof hc.widgetAction === "function") {
      hc.widgetAction({
        widgetKind: widgetKind,
        action: action,
        payload: payload || {}
      });
      return;
    }
    hostPost({
      type: "widget_action",
      widgetKind: widgetKind,
      action: action,
      payload: payload || {}
    });
  }

  function saveJournalTask(markdownLine, mode, journalPath) {
    const hc = window.heyComputer;
    if (hc && typeof hc.saveToVault === "function") {
      hc.saveToVault({
        markdown: markdownLine,
        mode: mode || "prepend_task",
        title: journalPath || "Journal Task"
      });
      return;
    }
    hostPost({
      type: "save_vault",
      markdown: markdownLine,
      mode: mode || "prepend_task",
      title: journalPath || "Journal Task"
    });
  }

  /* Single-line low-clutter widget shell */
  function buildWidgetShell(widget, rightMetaText) {
    const kind = widget.kind || widget.type || "unknown";
    const card = el("section", "hc-widget hc-widget-" + kind);
    if (widget.id) card.dataset.widgetId = String(widget.id);

    const header = el("header", "widget-header");
    header.appendChild(el("h3", "widget-title", widget.title || "Widget"));

    const headerRight = el("div", "widget-header-right");
    if (rightMetaText) {
      headerRight.appendChild(el("span", "", rightMetaText));
    }
    header.appendChild(headerRight);

    const body = el("div", "widget-body");
    card.appendChild(header);
    card.appendChild(body);

    return { card: card, header: header, headerRight: headerRight, body: body };
  }

  /* ------------------------------------------------------------------------
     1. calendar_agenda — Flat Hairline Timeline + Hover Actions
     ------------------------------------------------------------------------ */
  function renderCalendarAgenda(widget, data) {
    const events = Array.isArray(data.events) ? data.events : [];
    const shell = buildWidgetShell(widget, data.timezone || "");

    const conflictNote = data.conflictNote || data.conflict_note;
    if (conflictNote) {
      const banner = el("div", "agenda-conflict-banner");
      banner.appendChild(el("span", "", "⚠ " + conflictNote));
      const resolveBtn = el("button", "w-btn", "Resolve");
      resolveBtn.type = "button";
      resolveBtn.addEventListener("click", function () {
        triggerPrompt(
          "Propose a schedule adjustment to resolve this calendar conflict: " + conflictNote
        );
      });
      banner.appendChild(resolveBtn);
      shell.body.appendChild(banner);
    }

    const list = el("div", "agenda-list");

    events.forEach(function (ev) {
      const cal = ev.calendar === "personal" ? "personal" : "work";
      const status = ev.status || "confirmed";
      const row = el("div", "w-row agenda-item");
      row.dataset.calendar = cal;
      row.dataset.status = status;

      const bar = el("div", "agenda-bar");

      const timeCol = el("div", "agenda-time-col");
      timeCol.appendChild(
        el("span", "", (ev.start || "—") + (ev.end ? "–" + ev.end : ""))
      );
      timeCol.appendChild(
        el("span", "agenda-cal-label", status !== "confirmed" ? cal + " · " + status : cal)
      );

      const main = el("div", "agenda-main");
      const titleRow = el("div", "agenda-title-row");
      const titleLeft = el("div", "agenda-title-left");
      titleLeft.appendChild(el("span", "agenda-event-title", ev.title || "Untitled event"));

      const meetUrl = ev.meetUrl || ev.meet_url;
      if (meetUrl) {
        const meetLink = el("a", "meet-chip", "Join Meet ↗");
        meetLink.href = meetUrl;
        meetLink.target = "_blank";
        meetLink.rel = "noopener noreferrer";
        titleLeft.appendChild(meetLink);
      }

      const actions = el("div", "row-hover-actions");
      const prepBtn = el("button", "w-btn", "Prep Brief");
      prepBtn.type = "button";
      prepBtn.addEventListener("click", function () {
        triggerPrompt(
          "Prepare a concise briefing and talking points for \"" +
            (ev.title || "meeting") +
            "\" (" +
            (ev.start || "") +
            "–" +
            (ev.end || "") +
            ")."
        );
      });

      const bufferBtn = el("button", "w-btn", "+15m Buffer");
      bufferBtn.type = "button";
      bufferBtn.addEventListener("click", function () {
        triggerPrompt(
          "Add a 15-minute transition buffer after \"" +
            (ev.title || "meeting") +
            "\" at " +
            (ev.end || ev.start || "its end time") +
            " and check for conflicts."
        );
      });

      actions.appendChild(prepBtn);
      actions.appendChild(bufferBtn);
      titleRow.appendChild(titleLeft);
      titleRow.appendChild(actions);
      main.appendChild(titleRow);

      const subParts = [];
      if (ev.location) subParts.push(ev.location);
      const attendees = Array.isArray(ev.attendees) ? ev.attendees : [];
      const attNames = attendees
        .map(function (a) { return typeof a === "string" ? a : (a.name || a.email || ""); })
        .filter(Boolean);
      if (attNames.length > 0) subParts.push(attNames.join(", "));
      if (subParts.length > 0) {
        main.appendChild(el("div", "agenda-subline", subParts.join(" · ")));
      }

      const travelNote = ev.travelNote || ev.travel_note;
      if (travelNote) {
        main.appendChild(el("div", "agenda-travel-note", "↳ " + travelNote));
      }

      row.appendChild(bar);
      row.appendChild(timeCol);
      row.appendChild(main);
      list.appendChild(row);
    });

    shell.body.appendChild(list);
    return shell.card;
  }

  /* ------------------------------------------------------------------------
     2. transit_board — Flat Station Board + Hover Remind
     ------------------------------------------------------------------------ */
  function detectLineCategory(route) {
    const raw = String(route.category || "").toUpperCase();
    if (raw === "U" || raw === "S" || raw === "ICE" || raw === "REGIO") return raw;
    const line = String(route.line || "").toUpperCase();
    if (line.startsWith("ICE") || line.startsWith("IC")) return "ICE";
    if (line.startsWith("U")) return "U";
    if (line.startsWith("S")) return "S";
    if (line.startsWith("RE") || line.startsWith("RB")) return "REGIO";
    return "OTHER";
  }

  function formatPlatform(platform) {
    if (!platform) return "";
    const s = String(platform).trim();
    if (/^(pl\.|gl\.|platform|gleis|track)/i.test(s)) return s;
    return "Pl. " + s;
  }

  function renderTransitBoard(widget, data) {
    const origin = data.origin || "Origin";
    const destination = data.destination || "Destination";
    const routes = Array.isArray(data.routes) ? data.routes : [];

    const shell = buildWidgetShell(widget, "");
    const updatedSpan = el(
      "span",
      "",
      "Updated " + (data.updatedAt || data.updated_at || "live")
    );
    const refreshBtn = el("button", "w-btn w-btn-ghost", "↻ Refresh");
    refreshBtn.type = "button";
    refreshBtn.addEventListener("click", function () {
      const now = new Date();
      const hh = String(now.getHours()).padStart(2, "0");
      const mm = String(now.getMinutes()).padStart(2, "0");
      const ss = String(now.getSeconds()).padStart(2, "0");
      updatedSpan.textContent = "Updated " + hh + ":" + mm + ":" + ss;
      emitWidgetAction("transit_board", "refresh", {
        origin: origin,
        destination: destination
      });
    });

    shell.headerRight.appendChild(updatedSpan);
    shell.headerRight.appendChild(refreshBtn);

    const list = el("div", "");

    routes.forEach(function (route) {
      const row = el("div", "w-row transit-row");
      const main = el("div", "transit-row-main");

      const left = el("div", "transit-left");
      const badge = el("span", "line-badge", route.line || "—");
      badge.dataset.cat = detectLineCategory(route);

      const timesText =
        (route.departure || "—") +
        (route.arrival ? " → " + route.arrival : "") +
        (route.duration ? " (" + route.duration + ")" : "");
      left.appendChild(badge);
      left.appendChild(el("span", "transit-times", timesText));
      if (route.direction) {
        left.appendChild(el("span", "w-muted", "→ " + route.direction));
      }

      const right = el("div", "transit-right");
      const hoverActions = el("div", "row-hover-actions");
      const remindBtn = el("button", "w-btn", "Remind 10m Before");
      remindBtn.type = "button";
      remindBtn.addEventListener("click", function () {
        triggerPrompt(
          "Schedule a timed reminder 10 minutes before the " +
            (route.departure || "") +
            " " +
            (route.line || "train") +
            " departure from " +
            origin +
            " to " +
            destination +
            "."
        );
      });
      hoverActions.appendChild(remindBtn);
      right.appendChild(hoverActions);

      const plat = formatPlatform(route.platform);
      if (plat) right.appendChild(el("span", "w-muted", plat));

      const delay = Number(
        route.delayMins !== undefined ? route.delayMins : (route.delay_mins || 0)
      );
      let delayLabel = "On time";
      let delayClass = "delay-pill is-ontime";
      if (delay < 0) {
        delayLabel = delay + " min";
      } else if (delay >= 4) {
        delayLabel = "+" + delay + " min";
        delayClass = "delay-pill is-delayed";
      } else if (delay > 0) {
        delayLabel = "+" + delay + " min";
        delayClass = "delay-pill is-minor";
      }
      right.appendChild(el("span", delayClass, delayLabel));

      main.appendChild(left);
      main.appendChild(right);
      row.appendChild(main);

      const stops = Array.isArray(route.stops) ? route.stops : [];
      const statusNote = route.statusNote || route.status_note || "";
      let stopsDrawer = null;

      if (statusNote || stops.length > 0) {
        const subrow = el("div", "transit-subrow");
        subrow.appendChild(el("span", "", statusNote));

        if (stops.length > 0) {
          stopsDrawer = el("div", "transit-stops");
          stopsDrawer.hidden = true;
          stops.forEach(function (stop) {
            const stopText =
              typeof stop === "string"
                ? stop
                : (stop.time ? stop.time + " " : "") + (stop.name || "");
            stopsDrawer.appendChild(el("span", "transit-stop-chip", stopText));
          });

          const stopsBtn = el("button", "w-btn w-btn-ghost", "▾ " + stops.length + " stops");
          stopsBtn.type = "button";
          stopsBtn.addEventListener("click", function () {
            const nextHidden = !stopsDrawer.hidden;
            stopsDrawer.hidden = nextHidden;
            stopsBtn.textContent = (nextHidden ? "▾ " : "▴ ") + stops.length + " stops";
          });
          subrow.appendChild(stopsBtn);
        }

        row.appendChild(subrow);
        if (stopsDrawer) row.appendChild(stopsDrawer);
      }

      list.appendChild(row);
    });

    shell.body.appendChild(list);
    return shell.card;
  }

  /* ------------------------------------------------------------------------
     3. tasks_journal — Hairline Two-Column Vault Note + Google Tasks
     ------------------------------------------------------------------------ */
  function defaultTodayJournalPath() {
    if (
      window.__HC_BOOT__ &&
      typeof window.__HC_BOOT__.todayJournalPath === "string" &&
      window.__HC_BOOT__.todayJournalPath
    ) {
      return window.__HC_BOOT__.todayJournalPath;
    }
    const now = new Date();
    const y = String(now.getFullYear());
    const m = String(now.getMonth() + 1).padStart(2, "0");
    const d = String(now.getDate()).padStart(2, "0");
    return "Journal/" + y + "-" + m + "-" + d + ".md";
  }

  function renderTasksJournal(widget, data) {
    const journalPath = data.journalPath || data.journal_path || defaultTodayJournalPath();
    const journalTasks = (Array.isArray(data.journalTasks) ? data.journalTasks : []).map(
      function (t, i) {
        return {
          id: t.id || "jt-" + i,
          text: t.text || "",
          done: Boolean(t.done),
          tag: t.tag || ""
        };
      }
    );
    const datedTasks = (Array.isArray(data.datedTasks) ? data.datedTasks : []).map(
      function (t, i) {
        return {
          id: t.id || "dt-" + i,
          text: t.text || "",
          due: t.due || "",
          done: Boolean(t.done),
          list: t.list || ""
        };
      }
    );

    const shell = buildWidgetShell(widget, journalPath);
    const grid = el("div", "tasks-grid");

    /* Left Column: Vault Daily Note */
    const vaultPane = el("div", "tasks-pane");
    const vaultHeader = el("div", "tasks-pane-header");
    vaultHeader.appendChild(el("span", "tasks-pane-title", "Vault Daily Note"));
    const vaultCount = el("span", "", "");
    vaultHeader.appendChild(vaultCount);
    vaultPane.appendChild(vaultHeader);

    const vaultList = el("div", "task-items");

    function updateVaultCount() {
      const done = journalTasks.filter(function (t) { return t.done; }).length;
      vaultCount.textContent = done + "/" + journalTasks.length;
    }

    function createJournalRow(item) {
      const row = el("div", "task-row" + (item.done ? " is-done" : ""));
      const label = el("label", "task-label");
      const cb = document.createElement("input");
      cb.type = "checkbox";
      cb.checked = item.done;
      cb.addEventListener("change", function () {
        item.done = cb.checked;
        row.classList.toggle("is-done", item.done);
        updateVaultCount();
        emitWidgetAction("tasks_journal", "toggle_journal_task", {
          id: item.id,
          text: item.text,
          done: item.done,
          journalPath: journalPath
        });
      });

      label.appendChild(cb);
      label.appendChild(el("span", "task-text", item.text));
      row.appendChild(label);

      if (item.tag) {
        const tagText = String(item.tag).startsWith("#") ? item.tag : "#" + item.tag;
        row.appendChild(el("span", "task-tag", tagText));
      }
      return row;
    }

    journalTasks.forEach(function (item) {
      vaultList.appendChild(createJournalRow(item));
    });
    updateVaultCount();
    vaultPane.appendChild(vaultList);

    const quickForm = el("form", "journal-quick-add");
    const quickInput = el("input", "journal-input");
    quickInput.type = "text";
    quickInput.placeholder = "+ Quick Add to Journal…";
    quickInput.addEventListener("keydown", function (ev) {
      if (ev.key === "Escape") {
        ev.preventDefault();
        ev.stopPropagation();
        quickInput.blur();
      }
    });
    const quickBtn = el("button", "w-btn w-btn-primary", "Add");
    quickBtn.type = "submit";

    quickForm.addEventListener("submit", function (ev) {
      ev.preventDefault();
      const text = quickInput.value.trim();
      if (!text) return;
      const newItem = {
        id: "jt-new-" + Date.now(),
        text: text,
        done: false,
        tag: ""
      };
      journalTasks.unshift(newItem);
      vaultList.insertBefore(createJournalRow(newItem), vaultList.firstChild);
      updateVaultCount();
      quickInput.value = "";
      saveJournalTask("- [ ] " + text, "prepend_task", journalPath);
    });

    quickForm.appendChild(quickInput);
    quickForm.appendChild(quickBtn);
    vaultPane.appendChild(quickForm);

    /* Right Column: Dated Google Tasks */
    const gtasksPane = el("div", "tasks-pane");
    const gtasksHeader = el("div", "tasks-pane-header");
    gtasksHeader.appendChild(el("span", "tasks-pane-title", "Google Tasks"));
    const gtasksCount = el("span", "", "");
    gtasksHeader.appendChild(gtasksCount);
    gtasksPane.appendChild(gtasksHeader);

    const gtasksList = el("div", "task-items");

    function updateGtasksCount() {
      const open = datedTasks.filter(function (t) { return !t.done; }).length;
      gtasksCount.textContent = open + " open";
    }

    datedTasks.forEach(function (item) {
      const row = el("div", "task-row" + (item.done ? " is-done" : ""));
      const label = el("label", "task-label");
      const cb = document.createElement("input");
      cb.type = "checkbox";
      cb.checked = item.done;
      cb.addEventListener("change", function () {
        item.done = cb.checked;
        row.classList.toggle("is-done", item.done);
        updateGtasksCount();
        emitWidgetAction("tasks_journal", "toggle_dated_task", {
          id: item.id,
          text: item.text,
          done: item.done
        });
      });

      label.appendChild(cb);
      label.appendChild(el("span", "task-text", item.text));
      row.appendChild(label);

      if (item.due) {
        row.appendChild(el("span", "due-chip", item.due));
      }
      gtasksList.appendChild(row);
    });
    updateGtasksCount();
    gtasksPane.appendChild(gtasksList);

    grid.appendChild(vaultPane);
    grid.appendChild(gtasksPane);
    shell.body.appendChild(grid);
    return shell.card;
  }

  /* ------------------------------------------------------------------------
     4. dev_triage — Flat Linear Issues & GitLab Merge Requests
     ------------------------------------------------------------------------ */
  function normalizeStageStatus(raw) {
    const s = String(raw || "").toLowerCase();
    if (s === "passed" || s === "success" || s === "ok") return { key: "passed", sym: "✓" };
    if (s === "failed" || s === "error") return { key: "failed", sym: "✗" };
    if (s === "running" || s === "pending") return { key: "running", sym: "◌" };
    return { key: "other", sym: "–" };
  }

  function formatMrIid(iid) {
    const s = String(iid || "").trim();
    if (!s) return "!—";
    return s.startsWith("!") ? s : "!" + s;
  }

  function renderDevTriage(widget, data) {
    const issues = Array.isArray(data.issues) ? data.issues : [];
    const mergeRequests = Array.isArray(data.mergeRequests || data.merge_requests)
      ? (data.mergeRequests || data.merge_requests)
      : [];

    const shell = buildWidgetShell(
      widget,
      issues.length + " issues · " + mergeRequests.length + " MRs"
    );

    const issuesSec = el("div", "triage-section");
    issuesSec.appendChild(el("div", "triage-section-label", "Linear Issues"));

    issues.forEach(function (issue) {
      const row = el("div", "w-row issue-row");
      const top = el("div", "issue-top");
      const idGroup = el("div", "issue-id-group");

      const prioDot = el("span", "prio-dot");
      prioDot.dataset.prio = String(issue.priority || "medium").toLowerCase();
      prioDot.title = "Priority: " + (issue.priority || "medium");
      idGroup.appendChild(prioDot);

      const ident = issue.url
        ? el("a", "issue-ident", issue.identifier || issue.id)
        : el("span", "issue-ident", issue.identifier || issue.id);
      if (issue.url) {
        ident.href = issue.url;
        ident.target = "_blank";
        ident.rel = "noopener noreferrer";
      }
      idGroup.appendChild(ident);
      idGroup.appendChild(el("span", "issue-title", issue.title || ""));

      const rightGroup = el("div", "transit-right");
      const hoverActions = el("div", "row-hover-actions");
      const statusLabel = el("span", "w-pill", issue.status || "Todo");

      const sumBtn = el("button", "w-btn", "Summarize");
      sumBtn.type = "button";
      sumBtn.addEventListener("click", function () {
        triggerPrompt(
          "Summarize Linear issue " +
            (issue.identifier || issue.id) +
            " (\"" +
            (issue.title || "") +
            "\") including recent comments and acceptance criteria."
        );
      });

      const moveBtn = el("button", "w-btn", "Move to Review");
      moveBtn.type = "button";
      moveBtn.addEventListener("click", function () {
        statusLabel.textContent = "In Review";
        emitWidgetAction("dev_triage", "move_issue_review", {
          identifier: issue.identifier || issue.id,
          title: issue.title || ""
        });
      });

      hoverActions.appendChild(sumBtn);
      hoverActions.appendChild(moveBtn);
      rightGroup.appendChild(hoverActions);
      rightGroup.appendChild(statusLabel);

      top.appendChild(idGroup);
      top.appendChild(rightGroup);
      row.appendChild(top);

      const metaParts = [];
      if (issue.project) metaParts.push(issue.project);
      if (issue.assignee) {
        metaParts.push(
          String(issue.assignee).indexOf("@") !== -1
            ? String(issue.assignee)
            : "@" + issue.assignee
        );
      }
      if (metaParts.length > 0) {
        row.appendChild(el("div", "issue-bottom", metaParts.join(" · ")));
      }

      issuesSec.appendChild(row);
    });

    const mrSec = el("div", "triage-section");
    mrSec.appendChild(el("div", "triage-section-label", "GitLab Merge Requests"));

    mergeRequests.forEach(function (mr) {
      const row = el("div", "w-row mr-row");
      const top = el("div", "mr-top");
      const idGroup = el("div", "issue-id-group");
      const iidLabel = formatMrIid(mr.iid || mr.id);

      const iidNode = mr.url
        ? el("a", "mr-iid", iidLabel)
        : el("span", "mr-iid", iidLabel);
      if (mr.url) {
        iidNode.href = mr.url;
        iidNode.target = "_blank";
        iidNode.rel = "noopener noreferrer";
      }
      if (mr.repo) idGroup.appendChild(el("span", "w-muted", mr.repo));
      idGroup.appendChild(iidNode);
      idGroup.appendChild(el("span", "mr-title", mr.title || ""));
      top.appendChild(idGroup);

      if (mr.approvals !== undefined && mr.approvals !== null) {
        const appLabel =
          typeof mr.approvals === "number"
            ? "✓ " + mr.approvals
            : "✓ " + String(mr.approvals);
        top.appendChild(el("span", "w-muted", appLabel));
      }
      row.appendChild(top);

      const bottom = el("div", "mr-bottom");
      const srcBranch = mr.sourceBranch || mr.source_branch || "feature";
      const tgtBranch = mr.targetBranch || mr.target_branch || "main";
      bottom.appendChild(el("span", "", srcBranch + " → " + tgtBranch));

      const pipeline = mr.pipeline || {};
      const stages = Array.isArray(pipeline.stages) ? pipeline.stages : [];
      const stagesWrap = el("div", "pipeline-stages");

      stages.forEach(function (st) {
        const norm = normalizeStageStatus(st.status);
        const sp = el("span", "stage-pill", (st.name || "stage") + " " + norm.sym);
        sp.dataset.status = norm.key;
        stagesWrap.appendChild(sp);
      });

      const failedJob = pipeline.failedJob || pipeline.failed_job;
      let failureDrawer = null;

      if (failedJob) {
        const toggleLogBtn = el(
          "button",
          "w-btn w-btn-danger",
          "▴ " + (failedJob.name || "Failed job")
        );
        toggleLogBtn.type = "button";

        failureDrawer = el("div", "ci-failure-drawer");
        failureDrawer.appendChild(el("pre", "ci-log-snippet", failedJob.snippet || ""));

        const diagBtn = el("button", "w-btn w-btn-primary", "Diagnose CI Failure");
        diagBtn.type = "button";
        diagBtn.addEventListener("click", function () {
          triggerPrompt(
            "Diagnose the failing GitLab CI job \"" +
              (failedJob.name || "test") +
              "\" on MR " +
              iidLabel +
              " (" +
              srcBranch +
              " → " +
              tgtBranch +
              ") and propose a fix:\n\n```\n" +
              (failedJob.snippet || "") +
              "\n```"
          );
        });
        failureDrawer.appendChild(diagBtn);

        toggleLogBtn.addEventListener("click", function () {
          const nextHidden = !failureDrawer.hidden;
          failureDrawer.hidden = nextHidden;
          toggleLogBtn.textContent =
            (nextHidden ? "▾ " : "▴ ") + (failedJob.name || "Failed job");
        });
        stagesWrap.appendChild(toggleLogBtn);
      }

      bottom.appendChild(stagesWrap);
      row.appendChild(bottom);
      if (failureDrawer) row.appendChild(failureDrawer);
      mrSec.appendChild(row);
    });

    shell.body.appendChild(issuesSec);
    shell.body.appendChild(mrSec);
    return shell.card;
  }

  /* ------------------------------------------------------------------------
     5. hue_rooms — Philips Hue Room Control
     ------------------------------------------------------------------------ */
  function mirekLabel(mirek) {
    const m = Number(mirek) || 310;
    if (m <= 265) return { label: "Cool", mirek: m, hex: "#8ECAE6" };
    if (m <= 345) return { label: "Neutral", mirek: m, hex: "#E9C46A" };
    return { label: "Warm", mirek: m, hex: "#F4A261" };
  }

  function nextMirekStop(mirek) {
    const m = Number(mirek) || 310;
    if (m <= 265) return 320;
    if (m <= 345) return 410;
    return 235;
  }

  function applyScenePreset(sceneName, rooms) {
    const name = String(sceneName || "").toLowerCase();
    rooms.forEach(function (r, idx) {
      if (name === "all off") {
        r.on = false;
        r.brightness = 0;
      } else if (name === "focus") {
        r.on = true;
        r.brightness = idx === 0 ? 90 : idx === 1 ? 62 : 45;
        r.tempMirek = idx === 0 ? 235 : 275;
      } else if (name === "warm paper") {
        r.on = true;
        r.brightness = idx === 0 ? 76 : idx === 1 ? 65 : 38;
        r.tempMirek = 355;
      } else if (name === "evening relax") {
        r.on = idx !== 2;
        r.brightness = r.on ? (idx === 1 ? 52 : 34) : 0;
        r.tempMirek = 415;
      } else if (name === "nightlight") {
        r.on = idx === 1 || idx === 3;
        r.brightness = r.on ? 12 : 0;
        r.tempMirek = 450;
      } else {
        r.on = true;
        r.brightness = Math.max(30, r.brightness || 60);
      }
    });
  }

  function renderHueRooms(widget, data) {
    const scenes = Array.isArray(data.scenes) && data.scenes.length
      ? data.scenes
      : ["Focus", "Warm Paper", "Evening Relax", "Nightlight", "All Off"];
    let activeScene = data.activeScene || data.active_scene || scenes[0];

    const rooms = (Array.isArray(data.rooms) ? data.rooms : []).map(function (r, idx) {
      return {
        id: r.id || "room-" + idx,
        name: r.name || "Room " + (idx + 1),
        on: Boolean(r.on),
        brightness: Number(r.brightness || 0),
        tempMirek: Number(r.tempMirek || r.temp_mirek || 320),
        accentHex: r.accentHex || r.accent_hex || "#D0A215"
      };
    });

    const shell = buildWidgetShell(widget, "");
    const summarySpan = el("span", "", "");
    shell.headerRight.appendChild(summarySpan);

    const scenesBar = el("div", "hue-scenes-bar");
    const roomDomRefs = [];

    function refreshSummary() {
      const active = rooms.filter(function (r) { return r.on && r.brightness > 0; });
      const avg = active.length
        ? Math.round(
            active.reduce(function (acc, r) { return acc + r.brightness; }, 0) / active.length
          )
        : 0;
      summarySpan.textContent =
        active.length + "/" + rooms.length + " on" + (active.length ? " · " + avg + "%" : "");
    }

    function syncRoomCard(ref) {
      const r = ref.room;
      const isLit = r.on && r.brightness > 0;
      const warmth = mirekLabel(r.tempMirek);
      ref.card.classList.toggle("is-off", !isLit);
      ref.card.style.setProperty("--room-accent", r.accentHex || warmth.hex);
      ref.switchBtn.setAttribute("aria-checked", isLit ? "true" : "false");
      ref.slider.value = String(isLit ? r.brightness : 0);
      ref.pct.textContent = isLit ? r.brightness + "%" : "Off";
      ref.tempBtn.textContent = warmth.label + " · " + r.tempMirek + "M";
      refreshSummary();
    }

    scenes.forEach(function (scene) {
      const pill = el(
        "button",
        "hue-scene-pill" + (scene === activeScene ? " is-active" : ""),
        scene
      );
      pill.type = "button";
      pill.addEventListener("click", function () {
        activeScene = scene;
        Array.prototype.forEach.call(scenesBar.children, function (c) {
          c.classList.remove("is-active");
        });
        pill.classList.add("is-active");
        applyScenePreset(scene, rooms);
        roomDomRefs.forEach(syncRoomCard);
        emitWidgetAction("hue_rooms", "activate_scene", {
          scene: scene,
          room: data.room || null,
          rooms: rooms.map(function (r) { return r.name; })
        });
      });
      scenesBar.appendChild(pill);
    });

    const grid = el("div", "hue-rooms-grid");

    rooms.forEach(function (room) {
      const card = el("div", "hue-room-card");
      const top = el("div", "hue-room-top");
      const nameWrap = el("div", "hue-room-name-wrap");
      nameWrap.appendChild(el("span", "hue-swatch"));
      nameWrap.appendChild(el("span", "hue-room-name", room.name));

      const switchBtn = el("button", "hue-switch");
      switchBtn.type = "button";
      switchBtn.setAttribute("role", "switch");
      switchBtn.title = "Toggle " + room.name;

      top.appendChild(nameWrap);
      top.appendChild(switchBtn);

      const sliderRow = el("div", "hue-slider-row");
      const slider = el("input", "hue-slider");
      slider.type = "range";
      slider.min = "0";
      slider.max = "100";
      const pct = el("span", "hue-pct", "");
      sliderRow.appendChild(slider);
      sliderRow.appendChild(pct);

      const bottom = el("div", "hue-room-bottom");
      bottom.appendChild(el("span", "", "Warmth"));
      const tempBtn = el("button", "temp-pill", "");
      tempBtn.type = "button";
      bottom.appendChild(tempBtn);

      card.appendChild(top);
      card.appendChild(sliderRow);
      card.appendChild(bottom);
      grid.appendChild(card);

      const ref = {
        room: room,
        card: card,
        switchBtn: switchBtn,
        slider: slider,
        pct: pct,
        tempBtn: tempBtn
      };
      roomDomRefs.push(ref);
      syncRoomCard(ref);

      switchBtn.addEventListener("click", function () {
        room.on = !(room.on && room.brightness > 0);
        if (room.on && room.brightness === 0) room.brightness = 65;
        syncRoomCard(ref);
        emitWidgetAction("hue_rooms", "toggle_room", {
          room: room.name,
          roomName: room.name,
          roomId: room.id,
          on: room.on,
          brightness: room.brightness
        });
      });

      slider.addEventListener("input", function () {
        const val = Number(slider.value);
        room.brightness = val;
        room.on = val > 0;
        syncRoomCard(ref);
      });

      slider.addEventListener("change", function () {
        emitWidgetAction("hue_rooms", "set_brightness", {
          room: room.name,
          roomName: room.name,
          roomId: room.id,
          on: room.on,
          brightness: room.brightness
        });
      });

      tempBtn.addEventListener("click", function () {
        room.tempMirek = nextMirekStop(room.tempMirek);
        syncRoomCard(ref);
        emitWidgetAction("hue_rooms", "set_temp", {
          room: room.name,
          roomName: room.name,
          roomId: room.id,
          tempMirek: room.tempMirek
        });
      });
    });

    shell.body.appendChild(scenesBar);
    shell.body.appendChild(grid);
    return shell.card;
  }

  /* ------------------------------------------------------------------------
     6. distill_diff — Periodic Memory & Skill Distillation Review (#30)
     ------------------------------------------------------------------------ */
  function renderDistillDiff(widget, data) {
    const usedChars = Number(data.memoryUsedChars || data.memory_used_chars || 2410);
    const maxChars = Number(data.memoryMaxChars || data.memory_max_chars || 4000);
    const targetFile =
      data.targetFile || data.target_file || "$XDG_STATE_HOME/hey-computer/memory.md";
    const skippedTurns = Number(
      data.skippedScheduledTurns !== undefined
        ? data.skippedScheduledTurns
        : (data.skipped_scheduled_turns || 0)
    );
    const hunks = (Array.isArray(data.memoryHunks || data.memory_hunks)
      ? (data.memoryHunks || data.memory_hunks)
      : []
    ).map(function (h, idx) {
      return {
        id: h.id || "hunk-" + idx,
        kind: String(h.kind || "add").toLowerCase(),
        section: h.section || "Memory",
        before: h.before || "",
        after: h.after || "",
        reason: h.reason || "",
        accepted: true
      };
    });

    const skillDraft = data.skillDraft || data.skill_draft || null;
    let includeSkill = Boolean(skillDraft);

    const shell = buildWidgetShell(
      widget,
      skippedTurns > 0 ? skippedTurns + " scheduled turns skipped" : targetFile
    );

    const meterBox = el("div", "distill-meter-box");
    const meterTop = el("div", "distill-meter-top");
    const meterLabel = el("span", "", "");
    const meterProjected = el("span", "w-muted", "");
    meterTop.appendChild(meterLabel);
    meterTop.appendChild(meterProjected);

    const barTrack = el("div", "distill-bar-track");
    const barFill = el("div", "distill-bar-fill");
    barTrack.appendChild(barFill);
    meterBox.appendChild(meterTop);
    meterBox.appendChild(barTrack);
    shell.body.appendChild(meterBox);

    const hunksContainer = el("div", "");
    const footerSummary = el("span", "", "");

    function updateDistillMetrics() {
      let delta = 0;
      let acceptedCount = 0;
      hunks.forEach(function (h) {
        if (h.accepted) {
          acceptedCount += 1;
          delta += (h.after ? h.after.length : 0) - (h.before ? h.before.length : 0);
        }
      });
      const projected = Math.max(0, usedChars + delta);
      const basePct = Math.min(100, Math.round((usedChars / maxChars) * 100));
      const projPct = Math.min(100, Math.round((projected / maxChars) * 100));

      meterLabel.textContent =
        "MEMORY: " +
        usedChars.toLocaleString() +
        " / " +
        maxChars.toLocaleString() +
        " chars · " +
        basePct +
        "%";
      const sign = delta >= 0 ? "+" : "";
      meterProjected.textContent =
        "→ " + projected.toLocaleString() + " chars (" + sign + delta + ")";
      barFill.style.width = projPct + "%";

      footerSummary.textContent =
        acceptedCount +
        "/" +
        hunks.length +
        " hunks" +
        (skillDraft ? (includeSkill ? " + 1 skill draft" : "") : "") +
        " selected";
    }

    hunks.forEach(function (hunk) {
      const row = el("div", "w-row distill-hunk");
      const head = el("div", "distill-hunk-head");
      const left = el("div", "transit-right");

      const badge = el("span", "hunk-kind-badge", hunk.kind);
      badge.dataset.kind = hunk.kind;
      left.appendChild(badge);
      left.appendChild(el("strong", "", hunk.section));

      const toggleBtn = el("button", "w-btn w-btn-primary", "✓ Accept");
      toggleBtn.type = "button";
      toggleBtn.addEventListener("click", function () {
        hunk.accepted = !hunk.accepted;
        row.classList.toggle("is-skipped", !hunk.accepted);
        toggleBtn.className = hunk.accepted ? "w-btn w-btn-primary" : "w-btn";
        toggleBtn.textContent = hunk.accepted ? "✓ Accept" : "Skipped";
        updateDistillMetrics();
      });

      head.appendChild(left);
      head.appendChild(toggleBtn);
      row.appendChild(head);

      const diffLines = el("div", "distill-diff-lines");
      if (hunk.before) {
        String(hunk.before)
          .split("\n")
          .forEach(function (ln) {
            diffLines.appendChild(el("div", "diff-line is-del", "- " + ln));
          });
      }
      if (hunk.after) {
        String(hunk.after)
          .split("\n")
          .forEach(function (ln) {
            diffLines.appendChild(el("div", "diff-line is-add", "+ " + ln));
          });
      }
      row.appendChild(diffLines);

      if (hunk.reason) {
        row.appendChild(el("div", "distill-reason", hunk.reason));
      }

      hunksContainer.appendChild(row);
    });
    shell.body.appendChild(hunksContainer);

    if (skillDraft) {
      const skillBox = el("div", "skill-draft-box");
      const skillTop = el("div", "issue-top");
      const skillInfo = el("div", "issue-id-group");
      skillInfo.appendChild(
        el("strong", "", skillDraft.path || ("skills/" + (skillDraft.name || "draft") + "/SKILL.md"))
      );
      skillInfo.appendChild(
        el("span", "w-pill", (skillDraft.occurrences || 3) + "× repeated")
      );

      const skillControls = el("div", "transit-right");
      const previewPre = el(
        "pre",
        "skill-draft-preview",
        skillDraft.previewMarkdown || skillDraft.preview_markdown || ""
      );
      previewPre.hidden = true;

      const previewBtn = el("button", "w-btn w-btn-ghost", "▾ Preview SKILL.md");
      previewBtn.type = "button";
      previewBtn.addEventListener("click", function () {
        previewPre.hidden = !previewPre.hidden;
        previewBtn.textContent = previewPre.hidden ? "▾ Preview SKILL.md" : "▴ Hide SKILL.md";
      });

      const includeToggle = el("button", "w-btn w-btn-primary", "✓ Include");
      includeToggle.type = "button";
      includeToggle.addEventListener("click", function () {
        includeSkill = !includeSkill;
        includeToggle.className = includeSkill ? "w-btn w-btn-primary" : "w-btn";
        includeToggle.textContent = includeSkill ? "✓ Include" : "Skipped";
        updateDistillMetrics();
      });

      skillControls.appendChild(previewBtn);
      skillControls.appendChild(includeToggle);
      skillTop.appendChild(skillInfo);
      skillTop.appendChild(skillControls);
      skillBox.appendChild(skillTop);

      if (skillDraft.summary) {
        skillBox.appendChild(el("div", "w-muted", skillDraft.summary));
      }
      skillBox.appendChild(previewPre);
      shell.body.appendChild(skillBox);
    }

    updateDistillMetrics();

    const footer = el("footer", "widget-footer");
    footer.appendChild(footerSummary);
    const approveBtn = el("button", "w-btn w-btn-primary", "Approve Selected Changes");
    approveBtn.type = "button";
    approveBtn.addEventListener("click", function () {
      const acceptedHunks = hunks.filter(function (h) { return h.accepted; });
      const acceptedIds = acceptedHunks.map(function (h) { return h.id; });
      approveBtn.disabled = true;
      approveBtn.textContent = "Applying…";
      emitWidgetAction("distill_diff", "approve_changes", {
        widgetId: widget.id || null,
        targetFile: targetFile,
        acceptedHunks: acceptedIds,
        hunks: acceptedHunks,
        memoryHunks: hunks,
        includeSkill: includeSkill,
        skillDraft: skillDraft,
        skillPath: skillDraft ? skillDraft.path : null
      });
    });
    window.addEventListener("hc:widget_ack", function (ev) {
      const d = ev && ev.detail ? ev.detail : {};
      if (d.widgetKind === "distill_diff" && d.action === "approve_changes") {
        if (d.ok === false) {
          approveBtn.disabled = false;
          approveBtn.textContent = "Approve Selected Changes";
        } else {
          approveBtn.disabled = true;
          approveBtn.textContent = "Approved ✓";
        }
      }
    });
    footer.appendChild(approveBtn);
    shell.card.appendChild(footer);

    return shell.card;
  }

  /* ------------------------------------------------------------------------
     6b. day_plan — Interactive Morning Brief & Day Plan Card
     ------------------------------------------------------------------------ */
  function dayPlanPlural(n, singular, plural) {
    return n + " " + (n === 1 ? singular : plural);
  }

  function formatChannelBadge(rawChannel) {
    const lower = String(rawChannel || "").trim().toLowerCase();
    if (lower === "whatsapp" || lower === "wa") return { key: "whatsapp", label: "WhatsApp" };
    if (lower === "signal") return { key: "signal", label: "Signal" };
    if (lower === "gmail" || lower === "mail" || lower === "email") {
      return { key: "gmail", label: lower === "gmail" ? "Gmail" : "Mail" };
    }
    if (lower === "slack") return { key: "slack", label: "Slack" };
    return { key: "other", label: rawChannel ? String(rawChannel) : "Message" };
  }

  function formatNudgeKind(rawKind) {
    const lower = String(rawKind || "unanswered").trim().toLowerCase();
    if (
      lower === "promised_followup" ||
      lower === "promised-followup" ||
      lower === "promised" ||
      lower === "open_loop"
    ) {
      return { key: "promised_followup", label: "Promised follow-up" };
    }
    return { key: "unanswered", label: "Unanswered" };
  }

  function formatTaskSourceLabel(rawSource) {
    const lower = String(rawSource || "").trim().toLowerCase();
    if (lower === "gitlab") return "GitLab";
    if (lower === "linear") return "Linear";
    if (lower === "vault" || lower === "obsidian") return "Vault";
    if (lower === "gtasks" || lower === "google_tasks" || lower === "google-tasks") return "GTasks";
    if (lower === "personal") return "Personal";
    return rawSource ? String(rawSource) : "";
  }

  function getKnownActivatedDayPlanDates() {
    const set = new Set();
    const list = window.__HC_ACTIVATED_DAY_PLAN_DATES__;
    if (Array.isArray(list)) {
      list.forEach(function (d) {
        if (typeof d === "string" && d.trim()) set.add(d.trim());
      });
    }
    return set;
  }

  function renderDayPlan(widget, data) {
    const widgetId = widget.id ? String(widget.id) : ("day-plan-" + Date.now());
    const planDate = String(data.date || "").trim();
    const rawMode = String(data.mode || "workday").trim().toLowerCase();
    const isDayOff = rawMode === "day_off" || rawMode === "day-off" || rawMode === "dayoff";
    const modeKey = isDayOff ? "day_off" : "workday";
    const modeLabel = isDayOff ? "Day Off" : "Workday";
    const modeDetail = String(data.modeDetail || data.mode_detail || "").trim();
    const initialStatus = String(data.status || "proposed").trim().toLowerCase();
    const roterFaden = String(data.roterFaden || data.roter_faden || "").trim();

    const rawNudges = Array.isArray(data.replyNudges || data.reply_nudges)
      ? (data.replyNudges || data.reply_nudges)
      : [];
    const replyNudges = rawNudges.map(function (n, idx) {
      const sel = n.selected !== false;
      return {
        id: String(n.id || ("r" + (idx + 1))),
        channel: String(n.channel || ""),
        sender: String(n.sender || "Contact"),
        location: String(n.location || ""),
        age: String(n.age || ""),
        kind: String(n.kind || "unanswered"),
        summary: String(n.summary || ""),
        quote: String(n.quote || ""),
        selected: sel,
        initialSelected: sel
      };
    });

    const rawTasks = Array.isArray(data.tasks) ? data.tasks : [];
    const tasks = rawTasks.map(function (t, idx) {
      const sel = t.selected !== false;
      return {
        id: String(t.id || ("t" + (idx + 1))),
        title: String(t.title || ""),
        source: String(t.source || ""),
        ref: String(t.ref || ""),
        priority: String(t.priority || "medium"),
        destination: String(t.destination || ""),
        due: t.due ? String(t.due) : "",
        selected: sel,
        initialSelected: sel,
        subtasks: Array.isArray(t.subtasks)
          ? t.subtasks.map(function (s) { return String(s || "").trim(); }).filter(Boolean)
          : []
      };
    });

    const rawBlocks = Array.isArray(data.calendarBlocks || data.calendar_blocks)
      ? (data.calendarBlocks || data.calendar_blocks)
      : [];
    const calendarBlocks = rawBlocks.map(function (b, idx) {
      const sel = b.selected !== false;
      const cal = String(b.calendar || (isDayOff ? "personal" : "work")).toLowerCase() === "personal"
        ? "personal"
        : "work";
      return {
        id: String(b.id || ("b" + (idx + 1))),
        start: String(b.start || ""),
        end: String(b.end || ""),
        title: String(b.title || "Focus Block"),
        calendar: cal,
        transparency: String(b.transparency || (cal === "work" ? "opaque" : "")),
        taskRef: b.taskRef || b.task_ref || null,
        note: b.note ? String(b.note) : "",
        selected: sel,
        initialSelected: sel
      };
    });

    const rawDailyNote = data.dailyNote || data.daily_note || null;
    const dailyNote = rawDailyNote && typeof rawDailyNote === "object"
      ? {
          enabled: rawDailyNote.enabled !== false,
          initialEnabled: rawDailyNote.enabled !== false,
          path: String(rawDailyNote.path || defaultTodayJournalPath()),
          writeDayPlan:
            (rawDailyNote.writeDayPlan !== undefined
              ? rawDailyNote.writeDayPlan
              : rawDailyNote.write_day_plan) !== false,
          syncTodos:
            (rawDailyNote.syncTodos !== undefined
              ? rawDailyNote.syncTodos
              : rawDailyNote.sync_todos) !== false
        }
      : null;

    const rawPreDaily = data.preDailySync || data.pre_daily_sync || null;
    const preDailySync = rawPreDaily && typeof rawPreDaily === "object" && !isDayOff
      ? {
          enabled: rawPreDaily.enabled !== false,
          initialEnabled: rawPreDaily.enabled !== false,
          label: String(rawPreDaily.label || "Anchor StandUp brief on this Roter Faden")
        }
      : null;

    const rawCheckins = Array.isArray(data.checkins) ? data.checkins : [];
    const checkins = rawCheckins.map(function (c, idx) {
      const sel = c.selected !== false;
      return {
        id: String(c.id || ("c" + (idx + 1))),
        time: String(c.time || ""),
        label: String(c.label || "Check-in"),
        reason: String(c.reason || ""),
        selected: sel,
        initialSelected: sel
      };
    });

    const shell = buildWidgetShell(widget, "");
    shell.card.dataset.widgetId = widgetId;
    if (planDate) shell.card.dataset.dayPlanDate = planDate;

    /* Header left: title + mode pill */
    const titleWrap = el("div", "dp-header-left");
    shell.header.replaceChild(titleWrap, shell.header.firstChild);
    titleWrap.appendChild(el("h3", "widget-title", widget.title || "Day Plan"));

    const modePill = el(
      "span",
      "dp-mode-pill",
      modeDetail ? modeLabel + " · " + modeDetail : modeLabel
    );
    modePill.dataset.mode = modeKey;
    titleWrap.appendChild(modePill);

    /* Header right: live counter + status badge */
    const headerCounter = el("span", "dp-live-counter", "");
    const statusBadge = el("span", "dp-status-pill", "");
    statusBadge.hidden = true;
    shell.headerRight.appendChild(headerCounter);
    shell.headerRight.appendChild(statusBadge);

    const allCheckboxes = [];
    const draftButtons = [];

    /* Roter Faden banner */
    if (roterFaden) {
      const rfBox = el("div", "dp-roter-faden");
      rfBox.appendChild(el("div", "dp-roter-faden-label", "Roter Faden"));
      rfBox.appendChild(el("p", "dp-roter-faden-text", roterFaden));
      shell.body.appendChild(rfBox);
    }

    const sectionCounters = {};

    function computeLiveCountsText() {
      const parts = [];
      if (replyNudges.length > 0) {
        const c = replyNudges.filter(function (n) { return n.selected; }).length;
        parts.push(dayPlanPlural(c, "reply", "replies"));
      }
      if (tasks.length > 0) {
        const c = tasks.filter(function (t) { return t.selected; }).length;
        parts.push(dayPlanPlural(c, "task", "tasks"));
      }
      if (calendarBlocks.length > 0) {
        const c = calendarBlocks.filter(function (b) { return b.selected; }).length;
        parts.push(dayPlanPlural(c, "block", "blocks"));
      }
      if (checkins.length > 0) {
        const c = checkins.filter(function (ci) { return ci.selected; }).length;
        parts.push(dayPlanPlural(c, "check-in", "check-ins"));
      }
      return parts.join(" · ");
    }

    const footerSummary = el("span", "dp-footer-summary", "");

    function refreshCounts() {
      const text = computeLiveCountsText();
      headerCounter.textContent = text;
      footerSummary.textContent = text || "No items selected";
      if (sectionCounters.nudges) {
        const c = replyNudges.filter(function (n) { return n.selected; }).length;
        sectionCounters.nudges.textContent = c + "/" + replyNudges.length;
      }
      if (sectionCounters.tasks) {
        const c = tasks.filter(function (t) { return t.selected; }).length;
        sectionCounters.tasks.textContent = c + "/" + tasks.length;
      }
      if (sectionCounters.blocks) {
        const c = calendarBlocks.filter(function (b) { return b.selected; }).length;
        sectionCounters.blocks.textContent = c + "/" + calendarBlocks.length;
      }
    }

    function makeSection(title, counterKey) {
      const sec = el("div", "dp-section");
      const head = el("div", "dp-section-header");
      head.appendChild(el("span", "dp-section-label", title));
      if (counterKey) {
        const cnt = el("span", "w-muted", "");
        sectionCounters[counterKey] = cnt;
        head.appendChild(cnt);
      }
      sec.appendChild(head);
      return sec;
    }

    /* Section 1: Reply Nudges */
    if (replyNudges.length > 0) {
      const sec = makeSection("Reply Nudges", "nudges");
      replyNudges.forEach(function (nudge) {
        const row = el("div", "w-row dp-row dp-nudge-row" + (nudge.selected ? "" : " is-skipped"));
        const cb = document.createElement("input");
        cb.type = "checkbox";
        cb.className = "dp-checkbox";
        cb.checked = nudge.selected;
        cb.setAttribute("aria-label", "Include reply nudge for " + nudge.sender);
        cb.addEventListener("change", function () {
          nudge.selected = cb.checked;
          row.classList.toggle("is-skipped", !nudge.selected);
          refreshCounts();
        });
        allCheckboxes.push(cb);

        const content = el("div", "dp-row-main");
        const top = el("div", "dp-nudge-top");
        const meta = el("div", "dp-nudge-meta");

        const ch = formatChannelBadge(nudge.channel);
        const chBadge = el("span", "dp-channel-badge", ch.label);
        chBadge.dataset.channel = ch.key;
        meta.appendChild(chBadge);

        meta.appendChild(el("strong", "dp-sender", nudge.sender));
        if (nudge.location) {
          meta.appendChild(el("span", "w-muted", nudge.location));
        }
        if (nudge.age) {
          meta.appendChild(el("span", "w-muted", "· " + nudge.age));
        }

        const kindInfo = formatNudgeKind(nudge.kind);
        const kindPill = el("span", "dp-nudge-kind", kindInfo.label);
        kindPill.dataset.kind = kindInfo.key;
        meta.appendChild(kindPill);

        const draftBtn = el("button", "w-btn dp-draft-btn", "Draft Reply");
        draftBtn.type = "button";
        draftBtn.title = "Draft a read-only reply via fast profile (never auto-sends)";
        draftBtn.addEventListener("click", function () {
          const prevLabel = draftBtn.textContent;
          draftBtn.textContent = "Drafting…";
          emitWidgetAction("day_plan", "draft_reply", {
            widgetId: widgetId,
            date: planDate || null,
            id: nudge.id,
            channel: nudge.channel,
            sender: nudge.sender,
            location: nudge.location || null,
            age: nudge.age || null,
            kind: nudge.kind,
            summary: nudge.summary,
            quote: nudge.quote || null
          });
          setTimeout(function () {
            if (draftBtn.textContent === "Drafting…") {
              draftBtn.textContent = prevLabel;
            }
          }, 2400);
        });
        draftButtons.push(draftBtn);

        top.appendChild(meta);
        top.appendChild(draftBtn);
        content.appendChild(top);

        if (nudge.summary) {
          content.appendChild(el("div", "dp-nudge-summary", nudge.summary));
        }
        if (nudge.quote) {
          content.appendChild(el("div", "dp-nudge-quote", "“" + nudge.quote + "”"));
        }

        row.appendChild(cb);
        row.appendChild(content);
        sec.appendChild(row);
      });
      shell.body.appendChild(sec);
    }

    /* Section 2: Priorities & Sub-TODOs */
    if (tasks.length > 0) {
      const sec = makeSection("Priorities & Tasks", "tasks");
      tasks.forEach(function (task) {
        const row = el("div", "w-row dp-row dp-task-row" + (task.selected ? "" : " is-skipped"));
        const cb = document.createElement("input");
        cb.type = "checkbox";
        cb.className = "dp-checkbox";
        cb.checked = task.selected;
        cb.setAttribute("aria-label", "Include task " + task.title);
        cb.addEventListener("change", function () {
          task.selected = cb.checked;
          row.classList.toggle("is-skipped", !task.selected);
          refreshCounts();
        });
        allCheckboxes.push(cb);

        const content = el("div", "dp-row-main");
        const head = el("div", "dp-task-head");
        const titleLeft = el("div", "dp-task-title-group");

        const dot = el("span", "prio-dot");
        dot.dataset.prio = String(task.priority || "medium").toLowerCase();
        dot.title = "Priority: " + (task.priority || "medium");
        titleLeft.appendChild(dot);
        titleLeft.appendChild(el("span", "dp-task-title", task.title));
        head.appendChild(titleLeft);

        const pills = el("div", "dp-task-pills");
        if (task.ref) {
          pills.appendChild(el("span", "w-pill dp-ref-pill", task.ref));
        }
        const srcLabel = formatTaskSourceLabel(task.source);
        if (srcLabel) {
          pills.appendChild(el("span", "w-pill", srcLabel));
        }
        if (task.due) {
          pills.appendChild(el("span", "due-chip", task.due));
        }
        if (pills.childNodes.length > 0) {
          head.appendChild(pills);
        }
        content.appendChild(head);

        if (task.subtasks.length > 0) {
          const ul = el("ul", "dp-subtasks");
          task.subtasks.forEach(function (st) {
            ul.appendChild(el("li", "", st));
          });
          content.appendChild(ul);
        }

        row.appendChild(cb);
        row.appendChild(content);
        sec.appendChild(row);
      });
      shell.body.appendChild(sec);
    }

    /* Section 3: Proposed Calendar Blocks */
    if (calendarBlocks.length > 0) {
      const sec = makeSection("Proposed Calendar Blocks", "blocks");
      const list = el("div", "agenda-list");
      calendarBlocks.forEach(function (block) {
        const row = el(
          "div",
          "w-row agenda-item dp-block-row" + (block.selected ? "" : " is-skipped")
        );
        row.dataset.calendar = block.calendar;

        const cb = document.createElement("input");
        cb.type = "checkbox";
        cb.className = "dp-checkbox";
        cb.checked = block.selected;
        cb.setAttribute("aria-label", "Include calendar block " + block.title);
        cb.addEventListener("change", function () {
          block.selected = cb.checked;
          row.classList.toggle("is-skipped", !block.selected);
          refreshCounts();
        });
        allCheckboxes.push(cb);

        const bar = el("div", "agenda-bar");

        const timeCol = el("div", "agenda-time-col");
        timeCol.appendChild(
          el("span", "", (block.start || "—") + (block.end ? "–" + block.end : ""))
        );
        const isBusy = String(block.transparency || "").toLowerCase() === "opaque";
        timeCol.appendChild(
          el("span", "agenda-cal-label", isBusy ? block.calendar + " · busy" : block.calendar)
        );

        const main = el("div", "agenda-main");
        const titleRow = el("div", "agenda-title-row");
        const titleLeft = el("div", "agenda-title-left");
        titleLeft.appendChild(el("span", "agenda-event-title", block.title));
        titleRow.appendChild(titleLeft);
        main.appendChild(titleRow);

        if (block.note) {
          main.appendChild(el("div", "agenda-subline", block.note));
        }

        row.appendChild(cb);
        row.appendChild(bar);
        row.appendChild(timeCol);
        row.appendChild(main);
        list.appendChild(row);
      });
      sec.appendChild(list);
      shell.body.appendChild(sec);
    }

    /* Section 4: Daily Setup & Dynamic Check-ins */
    if (dailyNote || preDailySync || checkins.length > 0 || isDayOff) {
      const sec = makeSection("Daily Setup & Check-ins", null);

      if (dailyNote) {
        const row = el("div", "w-row dp-row dp-setup-row" + (dailyNote.enabled ? "" : " is-skipped"));
        const cb = document.createElement("input");
        cb.type = "checkbox";
        cb.className = "dp-checkbox";
        cb.checked = dailyNote.enabled;
        cb.setAttribute("aria-label", "Sync daily note");
        cb.addEventListener("change", function () {
          dailyNote.enabled = cb.checked;
          row.classList.toggle("is-skipped", !dailyNote.enabled);
          refreshCounts();
        });
        allCheckboxes.push(cb);

        const main = el("div", "dp-row-main dp-setup-main");
        const descParts = [];
        if (dailyNote.writeDayPlan) descParts.push("Write ## Day Plan");
        if (dailyNote.syncTodos) descParts.push("sync top-level - [ ] tasks");
        const labelText = descParts.length > 0
          ? descParts.join(" & ")
          : "Update daily note";
        main.appendChild(el("span", "dp-setup-title", labelText));
        if (dailyNote.path) {
          main.appendChild(el("span", "w-pill", dailyNote.path));
        }
        row.appendChild(cb);
        row.appendChild(main);
        sec.appendChild(row);
      }

      if (preDailySync) {
        const row = el(
          "div",
          "w-row dp-row dp-setup-row" + (preDailySync.enabled ? "" : " is-skipped")
        );
        const cb = document.createElement("input");
        cb.type = "checkbox";
        cb.className = "dp-checkbox";
        cb.checked = preDailySync.enabled;
        cb.setAttribute("aria-label", "Sync pre-daily StandUp brief");
        cb.addEventListener("change", function () {
          preDailySync.enabled = cb.checked;
          row.classList.toggle("is-skipped", !preDailySync.enabled);
          refreshCounts();
        });
        allCheckboxes.push(cb);

        const main = el("div", "dp-row-main dp-setup-main");
        main.appendChild(el("span", "dp-setup-title", preDailySync.label));
        main.appendChild(el("span", "w-pill", "pre-daily"));
        row.appendChild(cb);
        row.appendChild(main);
        sec.appendChild(row);
      }

      checkins.forEach(function (ci) {
        const row = el("div", "w-row dp-row dp-setup-row" + (ci.selected ? "" : " is-skipped"));
        const cb = document.createElement("input");
        cb.type = "checkbox";
        cb.className = "dp-checkbox";
        cb.checked = ci.selected;
        cb.setAttribute("aria-label", "Schedule check-in " + ci.label);
        cb.addEventListener("change", function () {
          ci.selected = cb.checked;
          row.classList.toggle("is-skipped", !ci.selected);
          refreshCounts();
        });
        allCheckboxes.push(cb);

        const main = el("div", "dp-row-main dp-checkin-main");
        if (ci.time) {
          main.appendChild(el("span", "dp-checkin-time", ci.time));
        }
        main.appendChild(el("strong", "dp-setup-title", ci.label));
        if (ci.reason) {
          main.appendChild(el("span", "dp-checkin-reason", "— " + ci.reason));
        }
        row.appendChild(cb);
        row.appendChild(main);
        sec.appendChild(row);
      });

      if (isDayOff && checkins.length === 0) {
        sec.appendChild(
          el("div", "dp-empty-note", "No reassessment check-ins scheduled by default on days off.")
        );
      }

      shell.body.appendChild(sec);
    }

    refreshCounts();

    /* Footer */
    const footer = el("footer", "widget-footer dp-footer");
    footer.appendChild(footerSummary);

    const footerActions = el("div", "dp-footer-actions");
    const iterateBtn = el("button", "w-btn w-btn-ghost", "Iterate in Chat");
    iterateBtn.type = "button";
    iterateBtn.addEventListener("click", function () {
      const hint =
        "Adjust blocks, tasks, or check-ins… (your checkbox toggles are sent automatically)";
      if (window.HCViewer && typeof window.HCViewer.focusComposer === "function") {
        window.HCViewer.focusComposer(hint);
      } else {
        const input = document.querySelector(".composer-textarea");
        if (input) {
          input.placeholder = hint;
          input.focus();
        }
      }
    });

    const confirmBtn = el("button", "w-btn w-btn-primary", "Confirm & Activate Plan");
    confirmBtn.type = "button";

    function buildLivePayload() {
      return {
        widgetId: widgetId,
        date: planDate || null,
        mode: modeKey,
        modeDetail: modeDetail || null,
        roterFaden: roterFaden,
        replyNudges: replyNudges.map(function (n) {
          return {
            id: n.id,
            channel: n.channel,
            sender: n.sender,
            location: n.location || null,
            age: n.age || null,
            kind: n.kind,
            summary: n.summary,
            quote: n.quote || null,
            selected: n.selected
          };
        }),
        tasks: tasks.map(function (t) {
          return {
            id: t.id,
            title: t.title,
            source: t.source || null,
            ref: t.ref || null,
            priority: t.priority,
            destination: t.destination || null,
            due: t.due || null,
            selected: t.selected,
            subtasks: t.subtasks.slice()
          };
        }),
        calendarBlocks: calendarBlocks.map(function (b) {
          return {
            id: b.id,
            start: b.start,
            end: b.end,
            title: b.title,
            calendar: b.calendar,
            transparency: b.transparency || null,
            taskRef: b.taskRef,
            note: b.note || null,
            selected: b.selected
          };
        }),
        dailyNote: dailyNote
          ? {
              enabled: dailyNote.enabled,
              path: dailyNote.path,
              writeDayPlan: dailyNote.writeDayPlan,
              syncTodos: dailyNote.syncTodos
            }
          : null,
        preDailySync: preDailySync
          ? {
              enabled: preDailySync.enabled,
              label: preDailySync.label
            }
          : null,
        checkins: checkins.map(function (ci) {
          return {
            id: ci.id,
            time: ci.time,
            label: ci.label,
            reason: ci.reason || null,
            selected: ci.selected
          };
        })
      };
    }

    let cardState = initialStatus === "activated" ? "activated" : "proposed";
    let isActivating = false;

    function applyVisualState(nextState, isMutedOlder) {
      cardState = nextState;
      const isActivated = nextState === "activated";
      const isSuperseded = nextState === "superseded";
      const locked = isActivated || isSuperseded;

      shell.card.classList.toggle("is-activated", isActivated);
      shell.card.classList.toggle("is-superseded", isSuperseded || Boolean(isMutedOlder));
      shell.card.classList.toggle("is-activating", isActivating);

      allCheckboxes.forEach(function (cb) {
        cb.disabled = locked || isActivating;
      });
      draftButtons.forEach(function (btn) {
        btn.disabled = isSuperseded || Boolean(isMutedOlder);
      });

      if (isActivated) {
        headerCounter.hidden = true;
        statusBadge.hidden = false;
        statusBadge.className = "dp-status-pill is-activated";
        statusBadge.textContent = "Plan Activated ✓";
        iterateBtn.hidden = true;
        confirmBtn.disabled = true;
        confirmBtn.textContent = "Plan Activated ✓";
        confirmBtn.className = "w-btn dp-btn-activated";
      } else if (isSuperseded) {
        headerCounter.hidden = true;
        statusBadge.hidden = false;
        statusBadge.className = "dp-status-pill is-superseded";
        statusBadge.textContent = "Superseded by revision";
        iterateBtn.hidden = true;
        confirmBtn.disabled = true;
        confirmBtn.textContent = "Superseded by revision";
        confirmBtn.className = "w-btn";
      } else {
        headerCounter.hidden = false;
        statusBadge.hidden = true;
        iterateBtn.hidden = false;
        iterateBtn.disabled = isActivating;
        confirmBtn.disabled = isActivating;
        confirmBtn.textContent = isActivating ? "Activating…" : "Confirm & Activate Plan";
        confirmBtn.className = "w-btn w-btn-primary";
      }
    }

    confirmBtn.addEventListener("click", function () {
      if (cardState !== "proposed" || isActivating) return;
      isActivating = true;
      applyVisualState("proposed", false);
      emitWidgetAction("day_plan", "confirm_plan", buildLivePayload());
    });

    window.addEventListener("hc:widget_ack", function (ev) {
      if (!shell.card.isConnected) return;
      const d = ev && ev.detail ? ev.detail : {};
      if (d.widgetKind !== "day_plan" || d.action !== "confirm_plan") return;
      const ackId = d.widgetId ? String(d.widgetId) : null;
      if (ackId && ackId !== widgetId) return;
      isActivating = false;
      if (d.ok === false) {
        applyVisualState("proposed", false);
      } else {
        if (planDate) {
          const dates = getKnownActivatedDayPlanDates();
          dates.add(planDate);
          window.__HC_ACTIVATED_DAY_PLAN_DATES__ = Array.from(dates);
        }
        applyVisualState("activated", false);
        syncDayPlanStates(window.__HC_ACTIVATED_DAY_PLAN_DATES__);
      }
    });

    footerActions.appendChild(iterateBtn);
    footerActions.appendChild(confirmBtn);
    footer.appendChild(footerActions);
    shell.card.appendChild(footer);

    function computeDelta() {
      if (cardState !== "proposed" || isActivating) return null;
      const unchecked = [];
      const checked = [];

      replyNudges.forEach(function (n) {
        if (n.selected !== n.initialSelected) {
          const tag = 'reply "' + n.sender + '"';
          (n.selected ? checked : unchecked).push(tag);
        }
      });
      tasks.forEach(function (t) {
        if (t.selected !== t.initialSelected) {
          const tag = 'task "' + (t.ref ? t.ref + " " : "") + t.title + '"';
          (t.selected ? checked : unchecked).push(tag);
        }
      });
      calendarBlocks.forEach(function (b) {
        if (b.selected !== b.initialSelected) {
          const range = b.start && b.end ? b.start + "–" + b.end + " " : "";
          const tag = 'block "' + range + b.title + '"';
          (b.selected ? checked : unchecked).push(tag);
        }
      });
      if (dailyNote && dailyNote.enabled !== dailyNote.initialEnabled) {
        (dailyNote.enabled ? checked : unchecked).push("daily note sync");
      }
      if (preDailySync && preDailySync.enabled !== preDailySync.initialEnabled) {
        (preDailySync.enabled ? checked : unchecked).push("pre-daily sync");
      }
      checkins.forEach(function (ci) {
        if (ci.selected !== ci.initialSelected) {
          const tag = 'check-in "' + (ci.time ? ci.time + " " : "") + ci.label + '"';
          (ci.selected ? checked : unchecked).push(tag);
        }
      });

      if (unchecked.length === 0 && checked.length === 0) return null;
      const parts = [];
      if (unchecked.length > 0) parts.push("unchecked: " + unchecked.join(", "));
      if (checked.length > 0) parts.push("checked: " + checked.join(", "));
      return {
        widgetId: widgetId,
        date: planDate || null,
        unchecked: unchecked,
        checked: checked,
        text: "[Live Day Plan UI selections: " + parts.join("; ") + "]"
      };
    }

    shell.card.__hcDayPlanCtrl = {
      widgetId: widgetId,
      date: planDate,
      initialStatus: initialStatus,
      getState: function () { return cardState; },
      applyVisualState: applyVisualState,
      computeDelta: computeDelta
    };

    // Initial state check against known activated dates
    const knownDates = getKnownActivatedDayPlanDates();
    if (initialStatus === "activated" || (planDate && knownDates.has(planDate))) {
      applyVisualState("activated", false);
    } else {
      applyVisualState("proposed", false);
    }

    return shell.card;
  }

  function syncDayPlanStates(activatedDatesList) {
    const activatedDates = getKnownActivatedDayPlanDates();
    if (Array.isArray(activatedDatesList)) {
      activatedDatesList.forEach(function (d) {
        if (typeof d === "string" && d.trim()) activatedDates.add(d.trim());
      });
      window.__HC_ACTIVATED_DAY_PLAN_DATES__ = Array.from(activatedDates);
    }

    const cards = Array.prototype.slice.call(
      document.querySelectorAll(".hc-widget-day_plan")
    );
    if (cards.length === 0) return;

    // Track whether any newer card has status=activated (globally or per date).
    let hasActivatedCard = false;
    cards.forEach(function (card) {
      const ctrl = card.__hcDayPlanCtrl;
      if (!ctrl) return;
      if (ctrl.initialStatus === "activated" || ctrl.getState() === "activated") {
        hasActivatedCard = true;
        if (ctrl.date) activatedDates.add(ctrl.date);
      }
    });

    cards.forEach(function (card, idx) {
      const ctrl = card.__hcDayPlanCtrl;
      if (!ctrl) return;
      const isLatest = idx === cards.length - 1;
      const dateActivated = Boolean(ctrl.date && activatedDates.has(ctrl.date));
      const selfActivated =
        ctrl.initialStatus === "activated" ||
        ctrl.getState() === "activated" ||
        dateActivated;

      if (selfActivated) {
        ctrl.applyVisualState("activated", !isLatest);
      } else if (!isLatest) {
        if (hasActivatedCard) {
          ctrl.applyVisualState("activated", true);
        } else {
          ctrl.applyVisualState("superseded", true);
        }
      } else {
        ctrl.applyVisualState("proposed", false);
      }
    });
  }

  function getActiveDayPlanDelta() {
    const cards = Array.prototype.slice.call(
      document.querySelectorAll(".hc-widget-day_plan")
    );
    for (let i = cards.length - 1; i >= 0; i -= 1) {
      const ctrl = cards[i].__hcDayPlanCtrl;
      if (ctrl && ctrl.getState() === "proposed") {
        return ctrl.computeDelta();
      }
    }
    return null;
  }

  /* ------------------------------------------------------------------------
     7. custom_html — Agent show_widget + Save as Template… + Error Boundary
     ------------------------------------------------------------------------ */
  function activateEmbeddedScripts(container) {
    const scripts = Array.prototype.slice.call(container.querySelectorAll("script"));
    scripts.forEach(function (oldScript) {
      const fresh = document.createElement("script");
      Array.prototype.forEach.call(oldScript.attributes, function (attr) {
        fresh.setAttribute(attr.name, attr.value);
      });
      fresh.textContent = oldScript.textContent;
      oldScript.parentNode.replaceChild(fresh, oldScript);
    });
  }

  function renderCustomHtml(widget, data) {
    const shell = buildWidgetShell(widget, "");

    const saveTemplateBtn = el("button", "w-btn w-btn-ghost", "Save as Template…");
    saveTemplateBtn.type = "button";
    saveTemplateBtn.title = "Draft a reusable SKILL.md + HTML template from this widget";
    saveTemplateBtn.addEventListener("click", function () {
      triggerPrompt(
        'Save the "' + (widget.title || "Custom Widget") + '" widget as a reusable skill template.'
      );
    });
    shell.headerRight.appendChild(saveTemplateBtn);

    const host = el("div", "show-widget-host");
    host.innerHTML = data.html || "";

    host.addEventListener("click", function (ev) {
      const target = ev.target && ev.target.closest
        ? ev.target.closest("[data-send-prompt], [data-prompt]")
        : null;
      if (target) {
        const promptText =
          target.getAttribute("data-send-prompt") || target.getAttribute("data-prompt");
        if (promptText) triggerPrompt(promptText);
      }
    });

    shell.body.appendChild(host);

    const errBanner = el("div", "widget-error-banner");
    errBanner.hidden = true;
    const errTitleRow = el("div", "widget-error-title");
    const errHeading = el("span", "", "⚠ Uncaught Widget JS Error");
    const errBtnGroup = el("div", "transit-right");
    const reportBtn = el("button", "w-btn w-btn-danger", "Report Error to Session");
    reportBtn.type = "button";
    const dismissBtn = el("button", "w-btn", "Dismiss");
    dismissBtn.type = "button";
    errBtnGroup.appendChild(reportBtn);
    errBtnGroup.appendChild(dismissBtn);
    errTitleRow.appendChild(errHeading);
    errTitleRow.appendChild(errBtnGroup);

    const errStack = el("pre", "widget-error-stack", "");
    errBanner.appendChild(errTitleRow);
    errBanner.appendChild(errStack);
    shell.body.appendChild(errBanner);

    let lastErrorReport = "";

    function surfaceWidgetError(detail) {
      const message = (detail && (detail.message || detail.reason)) || "Uncaught widget error";
      const file = (detail && (detail.source || detail.filename)) || "widget";
      const line = detail && (detail.line || detail.lineno);
      const col = detail && (detail.column || detail.colno);
      const loc = line
        ? " (" + file + ":" + line + (col ? ":" + col : "") + ")"
        : "";
      const stack = detail && detail.stack ? "\n" + detail.stack : "";
      lastErrorReport = message + loc + stack;
      errHeading.textContent = "⚠ Widget JS Error · " + message;
      errStack.textContent = lastErrorReport.trim();
      errBanner.hidden = false;
    }

    reportBtn.addEventListener("click", function () {
      triggerPrompt(
        'A show_widget component ("' +
          (widget.title || "custom_html") +
          '") threw an uncaught JavaScript error. Diagnose and regenerate the widget:\n\n```\n' +
          lastErrorReport +
          "\n```"
      );
    });

    dismissBtn.addEventListener("click", function () {
      errBanner.hidden = true;
    });

    window.addEventListener("hc:widget-error", function (ev) {
      if (shell.card.isConnected) {
        surfaceWidgetError(ev.detail || {});
      }
    });

    try {
      activateEmbeddedScripts(host);
    } catch (err) {
      surfaceWidgetError({
        message: err && err.message ? err.message : String(err),
        stack: err && err.stack ? err.stack : ""
      });
    }

    return shell.card;
  }

  /* ------------------------------------------------------------------------
     8. skill_template_draft — Save Reusable Widget Template as Skill
     ------------------------------------------------------------------------ */
  function renderSkillTemplateDraft(widget, data) {
    const skillName = data.skillName || data.skill_name || "custom-widget";
    const skillDir =
      data.skillDir ||
      data.skill_dir ||
      ("$XDG_CONFIG_HOME/hey-computer/skills/" + skillName);
    const skillPath = skillDir.endsWith("SKILL.md") ? skillDir : (skillDir + "/SKILL.md");
    const description = data.description || "Reusable parameterized show_widget skill template.";
    const slots = Array.isArray(data.slots) ? data.slots : [];
    const skillMarkdown = data.skillMarkdown || data.skill_markdown || "";
    const templateHtml = data.templateHtml || data.template_html || "";

    const draftWidget = {
      id: widget.id,
      kind: "skill_template_draft",
      title: widget.title || ("Skill Template · " + skillName)
    };
    const shell = buildWidgetShell(draftWidget, skillPath);

    shell.body.appendChild(el("div", "", description));

    if (slots.length > 0) {
      const slotsRow = el("div", "template-slots-row");
      slotsRow.appendChild(el("span", "w-muted", "Slots:"));
      slots.forEach(function (slot) {
        const name = typeof slot === "string" ? slot : (slot.name || slot.key || "slot");
        const clean = name.indexOf("{{") !== -1 ? name : "{{" + name + "}}";
        slotsRow.appendChild(el("span", "slot-pill", clean));
      });
      shell.body.appendChild(slotsRow);
    }

    const tabsRow = el("div", "template-tabs");
    const previewPre = el("pre", "template-preview-pre", skillMarkdown);
    previewPre.hidden = true;
    let activeTab = null;

    function selectTab(tabKey, content, btn) {
      if (activeTab === tabKey && !previewPre.hidden) {
        previewPre.hidden = true;
        activeTab = null;
        Array.prototype.forEach.call(tabsRow.children, function (b) {
          b.classList.remove("w-btn-primary");
        });
        return;
      }
      activeTab = tabKey;
      previewPre.textContent = content || "(empty)";
      previewPre.hidden = false;
      Array.prototype.forEach.call(tabsRow.children, function (b) {
        b.classList.toggle("w-btn-primary", b === btn);
      });
    }

    const skillTabBtn = el("button", "w-btn", "▾ SKILL.md");
    skillTabBtn.type = "button";
    skillTabBtn.addEventListener("click", function () {
      selectTab("skill", skillMarkdown, skillTabBtn);
    });

    const htmlTabBtn = el("button", "w-btn", "▾ template.html");
    htmlTabBtn.type = "button";
    htmlTabBtn.addEventListener("click", function () {
      selectTab("html", templateHtml, htmlTabBtn);
    });

    tabsRow.appendChild(skillTabBtn);
    tabsRow.appendChild(htmlTabBtn);
    shell.body.appendChild(tabsRow);
    shell.body.appendChild(previewPre);

    const footer = el("footer", "widget-footer");
    footer.appendChild(el("span", "", "Reviewed manually before writing skill files"));

    const saveBtn = el("button", "w-btn w-btn-primary", "Save Skill Template");
    saveBtn.type = "button";
    saveBtn.dataset.saveSkillBtn = skillName;
    saveBtn.addEventListener("click", function () {
      const posted = hostPost({
        type: "save_skill_template",
        skillName: skillName,
        skillDir: skillDir,
        skillMarkdown: skillMarkdown,
        templateHtml: templateHtml
      });
      if (!posted && typeof window.__hcDispatch === "function") {
        window.__hcDispatch({
          type: "skill_template_saved",
          saved: false,
          previewOnly: true,
          skillName: skillName,
          path: skillPath,
          preview: skillMarkdown + "\n\n<!-- template.html -->\n" + templateHtml
        });
      }
    });

    footer.appendChild(saveBtn);
    shell.card.appendChild(footer);
    return shell.card;
  }

  /* ------------------------------------------------------------------------
     Public Entry Point
     ------------------------------------------------------------------------ */
  function renderWidget(widget) {
    if (!widget || typeof widget !== "object") {
      return el("div", "hc-widget");
    }
    const kind = widget.kind || widget.type || "custom_html";
    const data = widget.data || widget.payload || widget;

    switch (kind) {
      case "calendar_agenda":
        return renderCalendarAgenda(widget, data);
      case "transit_board":
        return renderTransitBoard(widget, data);
      case "tasks_journal":
        return renderTasksJournal(widget, data);
      case "dev_triage":
        return renderDevTriage(widget, data);
      case "hue_rooms":
        return renderHueRooms(widget, data);
      case "distill_diff":
        return renderDistillDiff(widget, data);
      case "day_plan":
        return renderDayPlan(widget, data);
      case "custom_html":
      case "show_widget":
        return renderCustomHtml(widget, data);
      case "skill_template_draft":
        return renderSkillTemplateDraft(widget, data);
      default: {
        const fallback = buildWidgetShell(widget, kind);
        fallback.body.appendChild(el("pre", "vault-preview-pre", JSON.stringify(data, null, 2)));
        return fallback.card;
      }
    }
  }

  window.HCWidgets = {
    renderWidget: renderWidget,
    getActiveDayPlanDelta: getActiveDayPlanDelta,
    syncDayPlanStates: syncDayPlanStates
  };
})();
