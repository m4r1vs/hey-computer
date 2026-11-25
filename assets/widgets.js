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
    renderWidget: renderWidget
  };
})();
