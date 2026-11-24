# Improvements and workflows

Findings from a review of hey-computer, how a Home Manager configuration wires
it up (voice prefix rule in npu-dictate, Hyprland window rule, the
`programs.hey-computer` options), and patterns reported by users of other personal
AI harnesses (Hermes Agent, LifeOS / PAI, OpenClaw, and hardened offshoots
NanoClaw / ZeroClaw). Items are ordered for implementation from highest
immediate payoff and lowest risk through core harness primitives to multi-mode
and reactive messaging. Implemented items are collected under [Done](#done) with
their commit.

## The gap: it is a voice Q&A tool, not yet a delegation tool

Today hey-computer handles one request at a time, a new `ask` supersedes the
running one, and the result exists only as a notification or a file in
`$XDG_RUNTIME_DIR`. That is right for "Hey Computer, …" but limits handing it
longer tasks. Phase 1–2 items make everyday use smoother; Phase 3 turns it into
a delegation harness; Phases 4–6 build integrations, richer output UX,
multi-mode isolation, and reactive messaging on top.

**Triage & trust policy:** Items below are tagged `[Important]` or
`[Lower priority]`, along with where the change lives (`[Config]` for the
consuming Home Manager configuration, `[Upstream]` for `hey-computer`,
`[npu-dictate]` for the dictation frontend). Disregarded items (vault Git
auto-backup, `permissions.ask` confirmation tier, `hey-computer log`, and
env-var filtering) have been removed. **No permission prompts at all** — trust
the AI to do the right thing directly; permission prompts can be added later
only if needed.

**UI naming:** In user-facing prompts, launchers, keybind descriptions, and
future UI labels (viewer, notification actions, etc.), use **"Ask Computer"**
(not "Ask Hey Computer").

## Phase 1: Zero-Rust quick wins (consuming configuration & npu-dictate) — Implemented

All Phase 1 items (#1–#7) are implemented and listed under [Done](#done).

## Phase 2: Small upstream hardening & everyday latency/UX fixes

Surgical changes while `conductor.rs` is still single-lane. Landing budgets and
bug fixes before background tasks prevents unattended runaway loops later.

8. `[Important]` `[Upstream]` **Bounded, frozen-snapshot memory and fail-loudly
   prompt rule.** Support an optional, strictly size-capped user-owned memory
   file (e.g. `~/.config/hey-computer/memory.md`, kept outside the untrusted
   vault) loaded into `AGENTS.md` at agent start and frozen for the life of the
   session so prompt caching is never busted mid-session. Also add an explicit
   "fail loudly" rule to `BUILT_IN` (`src/environment.rs:165-186`) prohibiting
   the model from claiming success or guessing when a tool errors or returns
   empty output.
9. `[Important]` `[Upstream]` **Hard step and wall-clock budgets.**
   `inactivity_timeout_secs` resets on every `session/update`
   (`src/conductor.rs:1045`), so an agent stuck retrying failing commands can
   loop indefinitely and burn quota. Add `agent.max_tool_calls` (e.g. 15–20) and
   a generous `agent.max_turn_secs` (e.g. 300 s).
10. `[Lower priority]` `[Upstream]` **Timed-out turns still extend the follow-up
    window.** On `FailReason::Timeout`, `turn()` returns `broken: false`
    (`src/conductor.rs:1101-1106`), so `run_request()` increments
    `conversation.turns` and extends `follow_up_until`
    (`src/conductor.rs:690-696`). Marking timed-out turns `broken: true` avoids
    reusing a half-completed or wedged session on the next request.
11. `[Lower priority]` `[Upstream]` **Hardcoded "would become root" denial
    message.** In `src/conductor.rs:1161-1167`, a denied command always reports
    *"which would become root"*, even though `nix/home-manager.nix:115` suggests
    adding non-root commands like `"shutdown"` to `permissions.deny`. Make the
    refusal message generic (naming the refused command and deny list).
12. `[Important]` `[Upstream]` **Live progress updates in the `"Thinking…"`
    notification.** `src/conductor.rs:1049-1059` already receives `tool_call`
    updates (`kind`, `title`, `toolSummary`). Forwarding throttled summaries to
    `Desktop` using the existing `--replace-id` lets the user see mid-turn what
    the agent is doing and hit `cancel` (#4) early if speech recognition
    misheard the request.
13. `[Important]` `[Upstream]` **Ambient desktop and time context injection.**
    Every shell call costs 5–10 s (`README.md`). Prepending a tiny, opt-in
    ephemeral header to the **user turn** (not `AGENTS.md`, keeping the
    system-prompt prefix cacheable) with local date/time/weekday and active
    Hyprland window class/title/workspace wrapped in
    `<untrusted_desktop_context>` delimiters lets contextual requests ("put that
    in today's note", "move this to workspace 3") finish in fewer tool
    round-trips.
14. `[Important]` `[Upstream]` **Quick retrieval (`hey-computer last`).** Add
    `hey-computer last [--copy | --open]` so short answers whose notifications
    faded—or long answers needed on the clipboard—are immediately retrievable.
    (The SQLite FTS5 recall index follows in #18 so it shares the persistent
    `$XDG_STATE_HOME` store with background tasks.)

## Phase 3: Core delegation primitives (protocol, sessions, lanes)

The architectural foundations that the remaining roadmap builds on. Finalize the
`$XDG_STATE_HOME` answer/task record format once in #17 so `last` (#14), FTS5
recall (#18), `tasks` (#17), and the future viewer (#23) all read the same
schema.

15. `[Important]` `[Upstream]` **Model and effort profiles (`--profile` /
    `--deep`).** `model` and `thought_level` are fixed globally. Supporting
    named profiles in `config.toml` and `ask --profile deep` (or voice prefix
    routing) keeps routine commands fast/cheap (`low`) while allowing on-demand
    escalation for derivations, debugging, or research without restarting the
    daemon, and lets background tasks (#17) pick a profile per job. *Spike while
    here:* check whether `agy acp` accepts `session/set_config_option`
    mid-conversation (needed for follow-up escalation in #22).
16. `[Important]` `[Upstream]` **Attachments (`ask --file`, `ask --image`).**
    The prompt is text-only (`src/conductor.rs:1005`). Probing `agy acp` 1.5.13
    confirmed `promptCapabilities: { "image": true, "audio": false,
    "embeddedContext": true }`, so both image input and embedded context blocks
    are supported. Interactive screen capture already works via the
    `desktop-context` skill + `read_file`, while `--file`/`--image` lets CLI
    and scripted callers attach files or images directly to `ask`.
17. `[Important]` `[Upstream]` **Background tasks and scheduled-check support**
    (the core delegation feature). An `ask --background` flag or a `task`
    request type that is not superseded by a newer ask, runs in its own ACP
    session (serialized cleanly per lane, with per-lane permission profile hooks
    for #28), persists its result under `$XDG_STATE_HOME`, notifies on
    completion with an "Open" action, and is listed by `hey-computer tasks`.
    This also makes scheduled workflows safe: a systemd timer running
    `ask --print --new "…"` (e.g. a morning job writing weather and calendar
    into today's note) currently cancels whatever was asked by voice at that
    moment. Pair this with an optional `--suppress-token HEARTBEAT_OK` flag so
    systemd timers can run OpenClaw-style periodic checks silently unless
    something actually requires attention.
18. `[Important]` `[Upstream]` **Local recall index (SQLite FTS5).** A
    lightweight local SQLite FTS5 index of recent `(timestamp, request, answer)`
    turns built on the `$XDG_STATE_HOME` store from #17, exposed via a small
    `SKILL.md` so *"what was that command you gave me yesterday?"* works without
    vector-DB overhead.
19. `[Important]` `[Upstream]` **MCP passthrough.** `mcpServers` is hardcoded to
    `[]` (`src/conductor.rs:781`). Expose configured MCP servers (e.g.
    `mcp-nixos`, calendar, work tools) without extra permission prompts. *Spike
    first:* `agy acp` 1.5.13 omits `mcpCapabilities` from `initialize`, so
    verify stdio `mcpServers` handling directly in `session/new` before
    committing Phase 4 integrations to MCP versus local CLIs.

## Phase 4: Integrations & smart routing

Built on background tasks (#17), profiles (#15), and the MCP probe (#19). Per
the trust policy above, no daemon-level permission prompts are used — actions
execute directly, and permission prompts can be added later if ever needed.

20. `[Important]` **Google Calendar.** Reading ("what's on tomorrow", "when is
    my next meeting") and creating, moving, or deleting events. Either through a
    calendar MCP server (needs #19) or a small CLI wrapped in a `SKILL.md`
    ("code before prompts"). The CLI should hold the OAuth token itself, so the
    token never enters the model's context.
21. `[Important]` **Gmail.** Search, summarise ("anything important since this
    morning?"), draft, and send replies without permission prompts.
22. `[Important]` **Automatic model selection by difficulty.** Builds on model
    and effort profiles (#15), attachments (#16), and background tasks (#17).
    Options, cheapest first:
    - **Heuristics:** length of the request, keywords ("explain", "debug",
      "research", "derive"), attachments, background tasks → deep; short desktop
      commands → fast.
    - **Escalation on failure:** start fast; if a tool fails, the answer is
      empty, or the follow-up starts with "no, …", retry the follow-up on the
      deeper profile.
    - **A triage turn:** the fast model labels the request with a profile before
      the real turn. This costs one extra round-trip, so use it only when the
      heuristics are unsure.

    Model and thought level are set per session (`session/set_config_option`),
    so routing a new session is cheap. Rely on mid-conversation escalation only
    if the probe in #15 confirms agy accepts it. Show which profile answered in
    the notification, so wrong routing is visible.

## Phase 5: Better output UX (viewer & spoken answers)

Built after the `$XDG_STATE_HOME` history store (#17–#18) and the typed front
end (#4) are in place.

23. `[Important]` **A much better answer view, with actions.** Marker has known
    limits (README: an editor at heart with Open/Save buttons, umlauts break
    syntax highlighting, last release in 2023). A dedicated viewer (or a
    different existing one) should offer:
    - copy the whole answer or a single code block;
    - run a code block directly on click (no permission prompt);
    - open links (still disarmed until clicked);
    - a follow-up input field that continues the same conversation;
    - "save to vault" (append to today's note) and "open in editor";
    - a history of recent answers (ties in with `hey-computer last` #14 and the
      local recall index #18).

    Short answers should get notification actions too: Copy, Reply (opens the
    typed front end #4 in follow-up mode) and Open in viewer. Keep the HTML and
    remote-image disarming for whatever renders the answer.
24. `[Important]` `[Upstream]` **Optional audio feedback hook (`present.speak` /
    earcons).** For hands-free voice ergonomics, add an optional `present.speak`
    argv hook (e.g. Piper or Kokoro for short answers) or subtle earcons, with
    `cancel` stopping playback immediately.
25. `[Important]` **Optional spoken answers (TTS).** Expands the `present.speak`
    hook (#24). Use a local TTS engine (Piper or Kokoro) with a German and an
    English voice, chosen by the answer's language. Speak short answers whole;
    for long answers speak only the first sentence and open the viewer. `cancel`
    or pressing the talk key stops speech at once. Lower media volume (or pause
    via playerctl) while speaking. Make it a toggle that is easy to reach (voice
    command, Waybar click), because it's wanted at the desk only sometimes.

## Phase 6: Multi-mode isolation & reactive messaging

Highest operational surface; best built once the single-mode daemon and its
tooling are mature.

26. `[Lower priority]` `[Upstream + Config]` **Native keyring wait
    (`agent.wait_for_keyring`).** Move the consumer's GNOME Keyring wait into
    the daemon before warm start so downstream Home Manager configs do not need
    to `mkForce` `ExecStart` (which otherwise silently masks future module
    changes and complicates the systemd template unit in #27). Wait for
    `org.freedesktop.secrets` to be unlocked before warm start (avoiding a
    hidden gcr-prompter behind the lock screen timing out).
27. `[Important]` **Work and personal modes.** A mode is a named profile of
    everything the agent can reach: MCP servers, skills, instructions, vault,
    accounts and model.
    - **Work:** the employer's Linear, GitLab, Slack and Google Calendar.
    - **Personal:** personal Gmail and Google Calendar, WhatsApp, Signal.

    The simplest clean design is one daemon instance per mode (a systemd
    template unit, `hey-computer@work` / `hey-computer@personal`), each with its
    own socket, `state_dir` and agy `--gemini-dir`. Then conversation history,
    memory and agy's transcripts never mix between modes. The front ends talk to
    whichever socket is active; switch with a voice command, a keybind or a
    Waybar click, and show the active mode in Waybar. Points settled:
    - **Not a security boundary.** Both instances run as the same user, so each
      can read the other's tokens on disk. Separate Unix users would be needed
      for a real boundary.
    - **Work data policy.** Settled: the employer allows sending work data
      (Linear, GitLab, Slack, calendar) to the model service.
    - **GitLab write access.** Allowed directly without permission prompts.
28. `[Important]` **Acting on incoming notifications (v1: notification text
    only).** Watch notifications (a D-Bus monitor on
    `org.freedesktop.Notifications`, or a hook in the notification daemon) for
    an allowlist of apps (WhatsApp, Signal, mail). For each one, start a
    background task (#17) that, for example, drafts a reply from the
    notification's sender and text preview, with sending pasting into the
    focused chat window. Show the draft as a notification with Send / Edit /
    Dismiss actions. Rules:
    - **Never send automatically.** Sending is always a click.
    - **A restricted lane (decided: light, no OS sandbox).** These tasks run in
      their own ACP session with their own permission profile: the daemon
      refuses every `execute` and `edit` permission request and passes no MCP
      servers. No extra confinement (systemd hardening, bus proxy, separate
      account) on purpose.
    - **Cost and noise.** A cheap filter decides first whether a message
      deserves a model at all (sender allowlist, group chats off, quiet hours),
      with `HEARTBEAT_OK`-style suppression (#17) and a rate limit.
    - **Only for the active mode** (#27), so work messages never get drafts from
      the personal profile and the other way round.
29. `[Important]` **Separate messenger bridge (WhatsApp / Signal) & phone
    self-chat.** Worth adding once v1 drafts (#28) prove useful and need the
    previous messages of a chat, or when you want to talk to hey-computer from
    your phone.

    **How OpenClaw and Hermes connect messengers** (from their channel docs,
    November 2026):

    - **WhatsApp.** Both use **Baileys**, a library that emulates a WhatsApp Web
      session as a *linked device*. It is paired once by QR code (Settings →
      Linked Devices) and the session is kept on disk (OpenClaw
      `~/.openclaw/credentials/whatsapp/<account>/creds.json`, Hermes
      `~/.hermes/platforms/whatsapp/session`; Hermes runs it as a Node.js bridge
      process that its gateway starts). Neither uses the official Business API
      by default. Hermes offers it as a second path; it needs a Meta Business
      account and a public webhook. OpenClaw's voice-call feature uses a
      separate whatsmeow session.
    - **Number model.** Both recommend a **dedicated number** for the bot and
      treat the personal number as a supported second choice in **self-chat**
      mode: you message yourself and the agent answers there (OpenClaw
      `selfChatMode`, Hermes `WHATSAPP_MODE=self-chat`). Both model the agent as
      a contact *you talk to*, not as a reader of your other chats.
    - **Risk.** Hermes says plainly that WhatsApp does not support third-party
      bots outside the Business API, so a bridge "carries a small risk of
      account restrictions". Its mitigations: a dedicated number, no bulk
      messages, and no automated messages to people who haven't written first.
      It also warns that protocol updates can break the bridge and that WhatsApp
      unlinks devices after long inactivity. OpenClaw's page doesn't mention
      bans at all.
    - **Access control** is deny-by-default in both: DM allowlists by phone
      number, pairing codes for unknown senders, and groups off unless
      allowlisted, often with replies only on mention. Hermes refuses all
      incoming messages until an allowlist is set. Hermes also has
      `/approve` / `/deny` for dangerous commands raised from a chat.
    - **Session = the whole account.** Hermes: the session directory "grants
      full access to the WhatsApp account"; keep it `0700` and never commit it.
    - **Signal.** Both use **signal-cli** as a linked device, running as a
      daemon in HTTP mode: events come in over SSE (`/api/v1/events`) and sends
      go out over JSON-RPC (`/api/v1/rpc`) (`signal-cli daemon --http
      127.0.0.1:8080`, linked with `signal-cli link -n NAME`). OpenClaw can also
      use the `signal-cli-rest-api` container in `MODE=json-rpc`. Hermes
      supports talking to the agent through Signal's "Note to Self" on your own
      number. OpenClaw notes that on a personal account the bot ignores your
      own messages (loop protection) and recommends a separate number. Signal
      rate-limits attachment uploads.

    **What this means for hey-computer.** The goal here differs from theirs: not
    a bot people message, but an assistant that reads *your* incoming messages
    and drafts *your* replies (#28). That rules out a dedicated number: the
    bridge has to be a linked device on the personal account, which sees every
    chat. Suggested design:

    - **A separate messenger bridge service**, not part of the daemon. It holds
      the Baileys (or whatsmeow) and signal-cli sessions and offers a small
      local socket API: a stream of incoming messages (with the last few
      messages of that chat as context) and `send` to one chat. hey-computer
      consumes the stream and never touches the session files itself.
    - **Sends stay human.** Only replies to people who wrote first, each one a
      click, never bulk. That is exactly the usage Hermes calls low-risk.
    - **Phone as a front end, for free.** Once the bridge exists, self-chat
      (WhatsApp "message yourself", Signal "Note to Self") becomes a way to ask
      hey-computer from the phone. Accept only messages from your own number,
      and run them in the same lane as voice requests (no permission prompts).

## Independent track (slot in anytime)

- `[Important]` `[npu-dictate]` **Hotword detection.** Always-on, local
  detection of "Hey Computer", then recording until silence (VAD) and
  transcribing with the existing npu-dictate backend. Candidates for the
  detector are small wake-word models like openWakeWord (custom phrase training)
  running on the CPU or NPU. This probably belongs in npu-dictate rather than
  hey-computer, has no daemon dependencies, and can be tackled anytime after
  direct push-to-talk (#7) covers everyday voice ergonomics. Requirements: the
  audio stays local until the hotword fires; a visible mic indicator (Waybar); a
  mute toggle; and pausing while another app has the mic (calls) or while media
  is playing, to avoid false triggers. Measure the false-accept rate on real
  speech in both languages before trusting it.

## Done

- `[Important]` `[Config]` `[Done: NixConfig 8329b59, 4a0f1aa]` **Interactive
  agy sandbox disabled & `toolPermission = "always-proceed"` (intended).** Set
  `enableTerminalSandbox = false` (broken on NixOS with agy 1.5.13) and
  intentionally set `toolPermission = "always-proceed"` in
  `antigravity-cli.nix` so commands, web requests, and MCP tools run without
  prompts unless matched by explicit `permissions.ask` or `permissions.deny`
  rules.
- `[Important]` `[Config]` `[Done: NixConfig 8329b59]` **`instructions.extra`
  and rebuild deny list.** Added metric/24h locale conventions,
  `Journal/YYYY-MM-DD.md` daily-note format, default browser guidance,
  speech-recognition proper-noun glossary, and off-limits rules (plus
  `nixos-rebuild`/`rebuild` in `permissions.deny`).
- `[Important]` `[Config]` `[Done: NixConfig 8329b59]` **Generated
  `desktop-scripts` skill.** Exposed `voice-command` (including `slack`,
  `signal`, `spotify`) and host desktop scripts (`nightlight`, `powermode`,
  `caffeinate`, `toggle-zen`, `netbird-vpn`, `darkman`, `spotify-like`,
  `random-album-of-the-day`, `switch-kb-layout`, `open-linear-ticket`) via
  `extraPackages` and a generated `desktop-scripts/SKILL.md` in `skills.extra`.
- `[Important]` `[Config]` `[Done: NixConfig 8329b59, f92c640]` **Typed front
  end & cancel keybind.** Added minimalist `rofi-ask` `-modi` prompt
  (`SUPER + a`, matching `rofi-calculator`'s single-bar layout with dynamic
  follow-up placeholder and `! ` / `/new ` prefix for `--new`) and cancel bind
  (`SUPER + CTRL + a`).
- `[Important]` `[Config]` `[Done: NixConfig 8329b59, f92c640]` **Unified
  desktop context (`desktop-context`).** Replaced the separate "Ask about"
  shortcut/menu with a `desktop-context` CLI & skill (`selection`, `clipboard`
  text/image, frozen region/window/screen `capture` piped into `agy` via
  `read_file`, `copy`, and `paste`) usable directly from `SUPER + v`,
  `SUPER + ALT + v`, and `SUPER + a`.
- `[Important]` `[Config]` `[Done: NixConfig 8329b59]` **Waybar status
  indicator.** Added `custom/hey-computer` (`waybar-hey-computer`, 1 s poll +
  `SIGRTMIN+12`), hidden at idle and showing thinking (`running_secs`),
  follow-up countdown, starting, and down states; click cancels when thinking
  or opens `rofi-ask` during follow-up.
- `[Important]` `[npu-dictate + Config]` `[Done: NixConfig 8329b59, npu-dictate]`
  **Direct push-to-talk key.** Added `start --to RULE` / `toggle --to RULE` and
  `kind = "direct"` in `npu-dictate` (bypassing phrase matching and live
  preedit, joining cut phrases on stdin, dropping silent holds quietly), wired
  to `SUPER + ALT + v` in `NixConfig` (reusing `binddrint "v"`). *(Raw audio
  ACP blocks remain unsupported by agy 1.5.13: `promptCapabilities.audio:
  false`.)*
- `[Important]` `[Done: 83e3fc9]` **1.5 s UI lag on long answers.**
  `open_viewer()` awaits `VIEWER_GRACE` (1.5 s) inline inside the sequential
  `Desktop` presenter worker (`src/present.rs:606-616`, `820-844`). Because
  Marker stays running, every long answer leaves `"Thinking…"` on screen for an
  extra 1.5 s after the window is already launching. Send `"Answer opened"` once
  `spawn()` succeeds and watch for early exit in a spawned task that notifies
  only on failure.
- `[Important]` `[Done: b55fbef]` **`policy.rs` shell-parsing blind spots.**
  `SHELLS` `-c` detection checks `w == "-c"` (`src/policy.rs:161-166`), missing
  combined short flags like `bash -lc 'sudo …'` or `sh -ec '…'`. Also
  `substitutions()` (`src/policy.rs:283-308`) ignores single quotes, causing
  false positives on literal strings like `echo '$(sudo id)'`.
- `[Important]` `[Done: bef9ece]` **Inline math without a backslash misses the
  viewer.** `has_latex_inline_math()` (`src/present.rs:113-124`) requires `\`
  inside `$…$`, so `$x^2 + y^2 = z^2$` or `$a_i < b_i$` goes raw to a desktop
  notification. Checking for `\` or `^`/`_` (when `$` is not adjacent to
  whitespace) catches common math without tripping on currency like `$5 and
  $10`.
- `[Important]` `[Done: 89ea71f]` **HTML/image disarming edge cases.**
  `is_remote_image()` (`src/present.rs:328-346`) disarms `http(s)://`, `//`,
  `ftp://`, and `![alt][ref]`, but misses `data:` URIs (e.g.
  `data:image/svg+xml`) and CommonMark shortcut reference images (`![ref]`
  paired with `[ref]: https://…`).

## Lessons from other personal AI harnesses (Hermes, LifeOS, OpenClaw)

Synthesis of community reports, architecture docs, and security postmortems
across [Hermes Agent](https://github.com/NousResearch/hermes-agent) (Nous
Research), [LifeOS / PAI](https://github.com/danielmiessler/LifeOS) (Daniel
Miessler, [ourlifeos.ai](https://ourlifeos.ai)),
[OpenClaw](https://github.com/openclaw/openclaw), and hardened offshoots
[NanoClaw](https://github.com/nanocoai/nanoclaw) and
[ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw). The recurring takeaway:
**harness discipline (memory lifecycle, context hygiene, deterministic
scaffolding, and OS-level boundaries) matters far more than swapping frontier
models.**

### Patterns reported to work well in practice

- **Strict file-role separation + two-tier memory:** Separate `SOUL.md` (stable
  persona/boundaries), `USER.md` (preferences/stack), `AGENTS.md` (operating
  rules), and a **strictly token-capped `MEMORY.md`** (~500–800 tokens in
  Hermes). Write raw daily logs to `memory/YYYY-MM-DD.md` (auto-loading only
  today + yesterday), promote durable lessons periodically, and back historical
  lookup with **SQLite FTS5** rather than heavy vector DBs.
- **Frozen-snapshot loading for prompt cache stability (Hermes):** Persist
  memory writes to disk immediately, but keep the injected system-prompt memory
  block frozen for the active session (`stable → context → volatile`) and
  refresh only on the next session.
- **Deterministic scaffolding first ("code before prompts"):** Put repeatable
  workflows into small CLI scripts. For scheduled checks, run cheap shell
  pre-checks *before* waking an LLM, route routine checks to cheap/fast models,
  and suppress no-op runs with a sentinel (`HEARTBEAT_OK`) + quiet hours.
- **Progressive skill disclosure (`SKILL.md` over opaque plugins):** Prefer
  concise, audited Markdown skills invoking built-in primitives (`exec`, files,
  search) loaded on demand over stuffing 30+ tool schemas into turn 1.
- **Pre-compaction memory flush and output offloading (OpenClaw):** Intercept
  bulky tool outputs, save them to disk, and pass only a summary + path into
  the loop. Before compacting long sessions, trigger a silent flush turn to
  persist open loops/decisions (`NO_FLUSH` if none).
- **Single orchestrator + isolated workers:** Keep one parent session owning
  user channels and shared state; delegate heavy research or coding to
  subagents with **isolated context by default** on cheaper models returning
  compact artifacts.
- **Git-backed vanilla Markdown/Obsidian vault:** Power users point multiple
  harnesses (e.g. OpenClaw for mobile/Telegram routing + Hermes or Claude Code
  for deep work) at one plain-Markdown Obsidian vault with automated Git
  commits for rollback.

### Standout differentiators by harness

- **Hermes Agent:** Procedural memory loop (distilling multi-step runs into
  reusable `SKILL.md` files), bounded FTS5 + Honcho dialectic user modeling,
  pluggable execution sandboxes (Docker, SSH, Modal, Daytona), and an immutable
  global dangerous-command blocklist even in YOLO mode.
- **LifeOS / PAI:** TELOS intent engineering (explicit mission, goals, and
  *Ideal State Artifacts* rather than ad-hoc prompting), 5-tier functional
  memory (Work, Knowledge, Learning, Relationship, Observability), and
  event-driven lifecycle hooks (`SessionStart`, `KnowledgeWriteGuard`,
  `MemoryHealthGate`) enforcing invariants structurally rather than in prompts.
- **OpenClaw (+ NanoClaw / ZeroClaw):** Universal messaging gateway (Telegram
  widely preferred over WhatsApp for streaming, inline approval buttons, and
  Whisper voice notes), `HEARTBEAT.md` / `HEARTBEAT_OK`, and hardening patterns
  from NanoClaw/ZeroClaw (ephemeral per-session containers with
  `--cap-drop=ALL`, Landlock/Firejail, command allowlists, action rate limits,
  and outbound credential/entropy leak redaction).

### Common failure modes and practical fixes

| Failure mode | How it shows up | Practical fixes |
|---|---|---|
| **Runaway loops & cost blowups** | Ambiguous goals or retry loops burn tokens on frontier models. | Hard `max_steps` / wall-clock caps, deterministic pre-checks before LLM wake, model tiering. |
| **Prompt cache busting** | Mutating memory files or tool lists mid-session invalidates cached prefixes. | Frozen-snapshot memory loading, stable tool/system prompt ordering, lazy skill loading. |
| **Context rot & compaction cliffs** | Bloated `AGENTS.md`/`MEMORY.md` or raw tool dumps degrade instruction following; cached token counts can mask live usage. | Strict token caps on memory files, offload large tool outputs to disk, track local cumulative size. |
| **Lossy memory & "skill poisoning"** | Background self-eval (e.g. Hermes `_spawn_background_review`) can mark failed runs as successes and overwrite good `SKILL.md` files. | Git-version skills and memory, pin/lock core workflows, review auto-generated skills like PRs. |
| **Tool sprawl & "rule blindness"** | 30+ exposed tools or overlapping skills cause malformed calls and ignored prompt rules. | Prune redundant skills, expose core primitives + lazy loading, enforce hard rules in harness hooks. |
| **Supply-chain & prompt-injection attacks ("ClawHavoc")** | Malicious community skills shipped infostealers; audited skills leaked API keys into prompts/logs; untrusted web/note text hijacked shells. | Audit every skill (prefer pure `SKILL.md`), wrap external text in `<untrusted_content>` tags, keep secrets out of LLM context, enforce boundaries outside the LLM. |
| **Silent hallucination on tool failure** | Agent fabricates a plausible success response when a tool fails or returns empty. | Explicit fail-loudly prompt rules + harness-level error propagation and verification gates. |
| **Over-engineering ("Second Brain as a Full-Time Job")** | Building complex multi-agent org charts before establishing basic daily habits. | Start with one agent, vanilla Markdown, and automate real recurring friction first. |
