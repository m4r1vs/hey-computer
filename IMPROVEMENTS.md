# Improvements and workflows

Findings from a review of hey-computer, how a Home Manager configuration wires
it up (voice prefix rule in npu-dictate, Hyprland window rule, the
`programs.hey-computer` options), and patterns reported by users of other personal
AI harnesses (Hermes Agent, LifeOS / PAI, OpenClaw, and hardened offshoots
NanoClaw / ZeroClaw). Items are ordered for implementation from highest
immediate payoff and lowest risk through core harness primitives to multi-mode
and reactive messaging. Implemented items are collected under [Done](#done) with
their commit.

## From voice Q&A to a personal delegation harness

Originally hey-computer handled one request at a time, a new `ask` superseded
the running one, and the result lived only as a notification or a file in
`$XDG_RUNTIME_DIR`. Phases 1–5 have closed that core gap and output UX:
persistent SQLite WAL + FTS5 history (`history.db`), multi-lane background
tasks, local recall, MCP passthrough with lazy schema loading, ambient
CWD/`tmux` context, an agent-authored hybrid scheduler, 3-tier smart routing,
turn-budget awareness, host skills/MCP integrations, a native GTK4 + WebKitGTK
6.0 answer viewer with `mcp-ui` widgets and template skills, periodic
memory/skill distillation, and modality-matched DE/EN streaming spoken answers
are all in place. Phase 6 builds multi-mode daemon isolation and reactive
notification/messenger bridges on top.

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

## Phase 2: Small upstream hardening & everyday latency/UX fixes — Implemented

All Phase 2 items (#8–#14) are implemented and listed under [Done](#done).

## Phase 3: Core delegation primitives (protocol, sessions, lanes) — Implemented

All Phase 3 items (#15–#19) are implemented and listed under [Done](#done).

## Phase 4: Integrations, context, hardening & smart routing — Implemented

All Phase 4 items (#20–#28) are implemented and listed under [Done](#done).

## Phase 5: Better output UX (viewer, distillation & spoken answers) — Implemented

All Phase 5 items (#29–#32) are implemented and listed under [Done](#done).

## Phase 6: Multi-mode isolation & reactive messaging

Highest operational surface; best built once the single-mode daemon and its
tooling are mature.

33. `[Lower priority]` `[Upstream + Config]` **Native keyring wait
    (`agent.wait_for_keyring`).** Move the consumer's GNOME Keyring wait into
    the daemon before warm start so downstream Home Manager configs do not need
    to `mkForce` `ExecStart` (which otherwise silently masks future module
    changes and complicates the systemd template unit in #34). Wait for
    `org.freedesktop.secrets` to be unlocked before warm start (avoiding a
    hidden gcr-prompter behind the lock screen timing out).

    *Implementation note:* Bind `hey-computer.sock` **first** and watch
    `org.freedesktop.secrets` `Locked == false` via a D-Bus `PropertiesChanged`
    subscription inside `Conductor::run`'s `select!` loop (rather than a `0.5 s`
    `dbus-send` shell poll), exposing `agent: "locked"` (or `"starting"`) via
    `status --json` so Waybar and Rofi reflect the locked state immediately.
34. `[Important]` **Work and personal modes.** A mode is a named profile of
    everything the agent can reach: MCP servers, skills, instructions, vault,
    accounts and model.
    - **Work:** the employer's Linear (official MCP with read/write access,
      already wired via `mcp_oauth_tokens_file`), GitLab, Slack (including
      read-only forwarded work email) and Google Calendar / Drive.
    - **Personal:** personal Gmail, Google Calendar, Contacts, Drive and Tasks,
      private GitHub projects (`gh`), WhatsApp, Signal.

    *(Interim single-daemon mitigations already in place in the consuming
    configuration: `open-browser work|personal`, multi-account
    `google-workspace` OAuth with `google-workspace-auth <email> [work|personal]`,
    and prompt-level separation routing work mail via Slack DM while keeping
    Gmail and Google Tasks on the personal account.)*

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

    *Implementation notes from codebase review:*
    - Namespace `$XDG_RUNTIME_DIR/hey-computer/` viewer/answer Markdown files per
      instance (`daemon.rs`, `present.rs`, `main.rs`) and pass instance-aware
      `--socket` / `--state-dir` to `hey-computer-viewer` so `work` and
      `personal` windows, follow-ups, and answer files never collide or prune
      each other.
    - Export `HEY_COMPUTER_SOCKET` and `HEY_COMPUTER_CONFIG` alongside
      `HEY_COMPUTER_STATE_DIR` in `acp.rs` and `scheduler.rs` so
      `hey-computer schedule|recall|tasks|distill|speak|view` invoked by skills
      or jobs inside `hey-computer@work` vs `hey-computer@personal` hit the
      matching daemon socket and config.
    - Splitting stdio MCP servers across `work` (`gitlab`, `linear`, `slack`,
      work Calendar/Drive) and `personal` (`gmail`, personal
      Calendar/Contacts/Drive/Tasks, `gh`, `openhue`) will also roughly halve
      per-daemon idle/peak RSS and swap usage.
35. `[Important]` **Acting on incoming notifications (v1: notification text
    only).** Watch notifications (a D-Bus monitor on
    `org.freedesktop.Notifications`, or a hook in the notification daemon) for
    an allowlist of apps (WhatsApp, Signal, mail). For each one, start a
    background task (#17) that, for example, drafts a reply from the
    notification's sender and text preview, with sending pasting into the
    focused chat window. Show the draft as a notification with Send / Edit /
    Dismiss actions. Rules:
    - **Never send automatically.** Sending is always a click.
    - **Stateless checkpoint + single-use resume token.** Instead of holding an
      idle process or ACP session open waiting for a click, persist each draft
      checkpoint (draft text, target app/chat, single-use resume token, TTL) in
      SQLite and let the drafting turn exit immediately; clicking Send / Edit
      resumes idempotently by token and survives daemon restarts (reused by the
      messenger bridge in #36).
    - **A restricted lane (decided: light, no OS sandbox).** These tasks run in
      their own ACP session with their own permission profile: the daemon
      refuses every `execute` and `edit` permission request and passes no MCP
      servers. No extra confinement (systemd hardening, bus proxy, separate
      account) on purpose. *(Implementation note: because `agy` 1.5.13 executes
      `read_file` without sending `session/request_permission`,
      `LanePolicy::restricted()` in `src/policy.rs` must also explicitly deny
      `"fetch"` and `"search"` tool kinds—in addition to
      `["execute", "edit", "delete", "move"]` + `mcp`—so untrusted incoming
      notification or message text cannot combine `read_file` with web fetch to
      exfiltrate local files; `LaneFilter::Restricted` in `src/main.rs` is
      already wired as of `69fd4ea`.)*
    - **Cost and noise.** A cheap filter decides first whether a message
      deserves a model at all (sender allowlist, group chats off, quiet hours),
      with `HEARTBEAT_OK`-style suppression (#17) and a rate limit.
    - **Only for the active mode** (#34), so work messages never get drafts from
      the personal profile and the other way round.
36. `[Important]` **Separate messenger bridge (WhatsApp / Signal) & phone
    self-chat.** Worth adding once v1 drafts (#35) prove useful and need the
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
    and drafts *your* replies (#35). That rules out a dedicated number: the
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

- `[Important]` `[npu-dictate]` **Hotword detection & TTS barge-in.** Always-on,
  local detection of "Hey Computer", then recording until silence (VAD) and
  transcribing with the existing npu-dictate backend. Candidates for the
  detector are small wake-word models like openWakeWord (custom phrase training)
  running on the CPU or NPU. This probably belongs in npu-dictate rather than
  hey-computer, has no daemon dependencies, and can be tackled anytime after
  direct push-to-talk (#7) covers everyday voice ergonomics. Requirements: the
  audio stays local until the hotword fires; a visible mic indicator (Waybar); a
  mute toggle; pausing while another app has the mic (calls) or while media is
  playing, to avoid false triggers; and **barge-in** interrupting active TTS
  playback (#32) immediately once speaking/wake-word fires by calling
  `hey-computer cancel` / `quiet-speech` (post-turn TTS cancellation and
  push-to-talk key-down silence are already wired in `e5decb7` and `NixConfig
  4a170cd`; hotword barge-in should be gated via PipeWire `module-echo-cancel` /
  headphones or wake-word-only detection while TTS plays to avoid speaker
  self-triggers). Measure the false-accept rate on real speech in both languages
  before trusting it.

## Done

- `[Important]` `[Upstream + Config]` `[Done: e5decb7, 69fd4ea, 0cb1a36, aeb64fc, 3761c65, NixConfig b8b4d27, 4a170cd]`
  **GTK4 + WebKitGTK 6.0 answer viewer (`hey-computer-viewer`), `mcp-ui` widgets
  & template skills, `hey-computer distill`, & voice-matched DE/EN streaming TTS
  (#29, #30, #31, #32).** Promoted the `poc/viewer` prototype into the
  production `hey-computer-viewer` binary (`application_id =
  "hey-computer.answer"`, `NON_UNIQUE`, `hc://` disarmed asset scheme, Flexoki
  dual-theme `syntect`/`two-face` code blocks with inline `▶ Run`,
  `pulldown-latex` MathML Core, Morning Brief auto drop-caps, `Info` (`i`)
  toggle, `inert`-hardened SQLite FTS5 Recall drawer (`h`), local-date Vault
  Journal save (`v`), and live `~/.theme/{mode,palette.json}` `inotify` sync)
  and removed Marker and `poc/viewer/`. Bumped `history.db` to schema v4
  (`widgets_json`, `origin`, `schedule_id`), added `reply_to` live/DB session
  resumption and `stream: true` socket streaming, added internal stdio
  `hey-computer mcp-ui` (`show_widget` and `propose_widget_template` with
  user-skill linking + idle reload), added `hey-computer distill` with two-tier
  read-only + mutable `memory_files` budgeting and scheduled-turn exclusion
  (plus a weekly `weekly-distill` scheduler job in `NixConfig`), added
  short-answer `Copy`/`Reply`/`Open` notifications with `--replace-id`, LRU
  waiter eviction, and `present.reply_command` (`rofi-ask --reply-to {id}`), and
  added `[present.speak]` / `hey-computer speak` (`programs.hey-computer.speak`
  with pinned `en_US-lessac-medium` + `de_DE-thorsten-high` Piper voices, Waybar
  middle-click / Rofi / voice toggles, and `SUPER + ALT + v` talk-key TTS
  silence) with voice-only modality matching, widget restraint on voice queries,
  sentence-streamed DE/EN TTS, media ducking, earcons, Tier 2 escalation flush,
  and post-turn `cancel`.
- `[Important]` `[Upstream + Config]` `[Done: 8dc667d, NixConfig c7f1428, f8cb9b8]`
  **Post-Phase-4 review hardening, declarative scheduler jobs, multi-file memory
  auto-reload, & UX polish (#8, #9, #17, #23, #26, #27, #28).** Added
  declarative `[scheduler.jobs.<name>]` seeding (`managed = 1` in SQLite schema
  v3) and migrated `morning-digest` and `pre-daily` off separate systemd user
  timers into the native hybrid scheduler; added `instructions.memory_files`
  (`memory.extraFiles`) with idle background `AGENTS.md` + warm-spare
  auto-reloading when memory files change; emitted instant `"Thinking…"`
  progress before Tier 3 triage (replaced in place on background promotion),
  suppressed routine `(fast)` badges, prevented auto-backgrounding from
  superseding active foreground turns, and exempted internal MCP schema
  `read_file` lookups from `max_tool_calls`; isolated single-PID Ghostty
  multi-window `tmux`/CWD context and scoped unattended background ambient
  context to time-only; and added a `rofi-hey-computer-tasks` Waybar menu plus
  ACP/SQLite/policy/scheduler bug fixes.
- `[Important]` `[Upstream + Config]` `[Done: 21b5e80, NixConfig 6a1c4c3]`
  **System & per-turn budget awareness + raised `deep` budget (#9, #13, #15,
  #25).** Added `## Turn budgets` to generated `AGENTS.md` (documenting default
  and per-profile tool-call and wall-clock caps and instructing the model to
  batch lookups and wrap up cleanly before hitting hard cutoffs), prepended an
  authoritative `<budget>` block (`profile`, `max_tool_calls`,
  `time_limit_secs`) per turn, and raised `profiles.deep` in the consuming
  configuration to `max_tool_calls = 200` and `max_turn_secs = 900` to
  accommodate lazy MCP schema lookups alongside multi-service tool runs.
- `[Important]` `[Upstream + Config]` `[Done: c03bb60, NixConfig 89ccf19, 9b2fec0, dbe9d5f, 44752fb, 6a1c4c3, 84c5631]`
  **Google Workspace MCP (Gmail, Calendar, Contacts, Drive, Tasks), stdio OAuth
  hardening, & Slack work-mail routing (#20, #21).** Packaged
  `taylorwilsdon/google_workspace_mcp` (`v1.30.1`, MIT) in `nixpkgs.nix` and
  wired `programs.configured.hey-computer.googleWorkspace` exclusively into
  `hey-computer.nix` (`--transport stdio --single-user --tool-tier complete
  --tools gmail calendar contacts drive tasks`, plus `disabledTools` and
  `permissions` options, keeping OAuth credentials in a `0700` state dir outside
  `/nix/store`, model context, and the global coding-CLI `mcp.nix` set). Added a
  standalone `google-workspace-auth` helper (`--status` and profile-aware OAuth
  scope upgrades), isolated stdio MCP OAuth browser launches from JSON-RPC
  stdout via `<agent_dir>/mcp-browser` (`BROWSER` wrapper in `c03bb60`) and
  `_open_browser_stdio_safe` (`NixConfig 44752fb`), routed dated/timed reminders
  to personal Google Tasks (`NixConfig 6deffe2, 84c5631`), and routed forwarded
  work email via read-only Slack DM attachments rather than empty Workspace
  Gmail (`NixConfig 6a1c4c3`).
- `[Important]` `[Config]` `[Done: NixConfig 89ccf19]` **Context7 MCP (`deep`),
  `yt-dlp` / `gh` CLI skills, & built-in `agy` `/browser` (#22).** Wired
  `pkgs.context7-mcp` (`programs.configured.mcp.context7`, excluded from
  `profiles.fast.mcp_servers` alongside `nixos` while inherited by `deep`),
  added `yt-dlp`, `gh`, and `nodejs` (`node`/`npx` for `agy` `/browser`) to
  `extraPackages`, and added `yt-dlp/SKILL.md` and `gh/SKILL.md` offloading
  bulky transcripts, diffs, and CI logs to `$HEY_COMPUTER_ARTIFACTS_DIR/`.
- `[Important]` `[Upstream + Config]` `[Done: cfd2f5e, NixConfig 89ccf19]`
  **Focused-window CWD & Ghostty/`tmux` ambient context + `tmux` skill (#23).**
  Extended `src/context.rs` to parse active window PID, walk `/proc`
  descendants (bounded, zero subprocesses), match Ghostty/terminal `tmux`
  clients (`tmux list-clients` / `list-panes`), resolve `/proc/<pid>/cwd` for
  terminals/editors, and emit sanitized `active_window_cwd` and `tmux_*`
  fields; added bundled `skills/tmux/SKILL.md` and `programs.hey-computer.tmux`.
- `[Important]` `[Upstream + Config]` `[Done: cfd2f5e, NixConfig 89ccf19]`
  **Artifacts directory & large-output offloading (#24).** Created private
  `0700` `<state_dir>/artifacts/` with `history_days` age pruning (via
  `symlink_metadata`, without following symlinks), exported
  `HEY_COMPUTER_ARTIFACTS_DIR`, displayed it in `debug prepare`, and documented
  offloading bulky outputs (> ~16 KB) in `BUILT_IN`.
- `[Important]` `[Upstream]` `[Done: cfd2f5e]` **Daemon hardening (#25).**
  Added `ToolTracker` consecutive identical tool-call loop detection
  (`agent.max_repeat_tool_calls = 3`, canonical sorted JSON arguments with
  `tool_call` -> `request_permission` arg upgrade), live
  `MEMORY: <used> / <max> chars (<pct>%)` capacity header in `AGENTS.md`, and
  automatic SQLite/FTS5 maintenance (`turns_fts('optimize')`, `PRAGMA optimize`,
  `PRAGMA incremental_vacuum(256)`) on start, every 100 turns, and shutdown.
- `[Important]` `[Config]` `[Done: NixConfig 89ccf19, 6a1c4c3]` **Shell-gated
  proactive timers & split failure routing (#26).** Added
  `hey-computer-proactive` (`location`, `morning-digest`, `pre-daily`) and gated
  systemd user timers (`programs.configured.hey-computer.proactive.enable`),
  keeping clean no-ops silent via `--suppress-token HEARTBEAT_OK` while routing
  hard errors to `$XDG_STATE_HOME/hey-computer/proactive/failures.log` + a
  4-hour rate-limited alert notification.
- `[Important]` `[Upstream]` `[Done: cfd2f5e]` **Agent-authored hybrid
  scheduler (#27).** Bumped `history.db` to schema v2 (`schedules` table +
  in-place `v1 -> v2` migration), added `[scheduler]` and `src/scheduler.rs`
  (human specs + 5-field local-time cron, `--script` pre-collectors with
  `Policy::denied_word` validation and 0-token empty skips, `--no-agent` timed
  reminders, split `--failure-deliver` with exponential backoff), added
  `hey-computer schedule [add|list|show|edit|pause|resume|remove|run]`, and
  bundled `skills/schedule/SKILL.md` (`programs.hey-computer.schedule.enable`).
- `[Important]` `[Upstream + Config]` `[Done: cfd2f5e, NixConfig 89ccf19]`
  **Automatic model, effort, and lane routing (#28).** Added `InputSource`
  (`--voice` / `--source`) and 3-tier `[routing]` in `src/routing.rs` +
  `src/conductor.rs` (zero-latency heuristics with follow-up correction
  escalation and non-waiting auto-background delegation, Tier 2 one-time retry
  escalation to `deep_profile` on retryable fast failure, and Tier 3 bounded
  tool-disabled LLM triage strictly for non-voice `Unsure` turns; never invoked
  on voice), plus `--voice` in `npu-dictate.nix` and `--source typed` in
  `rofi-ask.nix`.
- `[Lower priority]` `[Upstream]` `[Done: 3c7390b]` **Timed-out turns & generic
  deny-list message (#10, #11).** Marked `TurnAbort::Quiet` as `broken: true`
  so timed-out sessions are closed instead of extending `follow_up_until`,
  replaced the hardcoded *"which would become root"* message with a generic
  deny-list refusal, scoped `acp::tool_command()` to shell execution calls, and
  added outbound trace redaction for attachment/MCP payloads.
- `[Important]` `[Upstream + Config]` `[Done: f359999, 4e1daa6, NixConfig 4ece432, fc0fdab]`
  **Model and effort profiles (`--profile`) (#15).** Added `[profiles.<name>]`
  (`model`, `thought_level`, `inactivity_timeout_secs`, `max_turn_secs`,
  `max_tool_calls`, `max_repeat_tool_calls`, `thinking_label`, `mcp_servers`)
  and `ask -p/--profile`, follow-up profile inheritance, in-place
  mid-conversation `session/set_config_option` switching (verified live on `agy`
  1.5.13), warm spare preservation for matching profiles, and profile-tagged
  progress (including custom `thinking_label` such as `"Pondering Hard…"`) and
  answer notifications.
- `[Important]` `[Upstream]` `[Done: 22cc456]` **Attachments (`ask --file`,
  `ask --image`) (#16).** Added `src/attachments.rs` (zero extra deps: RFC 4648
  base64, magic-byte MIME detection for PNG/JPEG/WebP/GIF, UTF-8 text checks,
  `O_NONBLOCK | O_NOCTTY` regular-file validation, and size caps) plus
  `-f/--file` and `-i/--image` on `ask` and `debug prompt`, embedded as ACP
  `resource` (`TextResourceContents`) and `image` blocks.
- `[Important]` `[Upstream]` `[Done: 521caf5, 47ceada]` **Unified SQLite WAL
  store, multi-lane background tasks, and scheduled-check support (#17).**
  Replaced `last.json` with a private `0600` SQLite WAL + FTS5 database
  (`<state_dir>/history.db`, `PRAGMA secure_delete = ON`, one-time `last.json`
  migration, `[history]` retention pruning), refactored `src/acp.rs` for
  per-`sessionId` multiplexing on the shared warm `agy acp` process (verified
  live on `agy` 1.5.13), added a bounded FIFO background lane (`[tasks]`,
  `ask -b/--background`, `--suppress-token`, `cancel --id`, `tasks
  [list|show|open|copy|cancel]`, `last --lane`), completion notifications with
  non-blocking **Open** actions, and `LanePolicy` hooks for #35.
- `[Important]` `[Upstream]` `[Done: 4b718d0]` **Local recall index (SQLite
  FTS5) (#18).** Added `hey-computer recall` (safe FTS5 query sanitization,
  BM25 + recency ranking, `--since`/`--until` relative/calendar filters,
  `<untrusted_recall>` framing) and the bundled `skills/recall/SKILL.md`
  enabled by default via `programs.hey-computer.recall.enable` in the Home
  Manager module.
- `[Important]` `[Upstream + Config]` `[Done: d0b7732, 2296f7b, 4e1daa6, 815dbfb, c03bb60, NixConfig 4ece432, fc0fdab, 71459c4]`
  **MCP passthrough (#19).** Added `src/mcp.rs`, `[mcp.<name>]` (`env_files` /
  `header_files` loaded at prepare time so secrets stay out of `config.toml` and
  `/nix/store`), `agent.mcp_oauth_tokens_file`, `<agent_dir>/mcp-browser`
  `BROWSER` isolation for stdio MCP servers, per-profile `mcp_servers`
  allowlists enforced per turn by `LanePolicy`, and
  `programs.hey-computer.mcp.servers` in `nix/home-manager.nix` (wiring
  `nixos`, `linear`, `gitlab`, `slack`, `context7`, and `google-workspace` in
  the consuming configuration). Writes enabled servers to
  `<agent_dir>/config/mcp_config.json` (mode `0600`) and passes `mcpServers: []`
  on `session/new` so `agy acp` 1.5.13 enables lazy `call_mcp_tool` loading
  without eagerly truncating large tool sets, while preserving conversations on
  mid-session profile switches across different `mcp_servers` allowlists.
- `[Important]` `[Upstream + Config]` `[Done: 3e3b6e1, NixConfig 37637a5, 905d20c]`
  **Bounded, frozen-snapshot memory and fail-loudly prompt rule (#8).** Added
  `instructions.memory_file` / `instructions.memory_max_chars` (default `4000`,
  Home Manager `memory.file` defaulting to `~/.config/hey-computer/memory.md`
  and wired to an `agenix`-encrypted secret path in `NixConfig` when defined,
  rejected inside the vault or via symlinks into the vault), loaded into
  `AGENTS.md` on agent start and kept frozen for the life of the agent process,
  plus the explicit fail-loudly rule in `BUILT_IN`.
- `[Important]` `[Upstream]` `[Done: acedf5b]` **Hard step and wall-clock
  budgets (#9).** Added `agent.max_tool_calls` (default `20`, deduplicated by
  `toolCallId`, wire reason `limit_reached`) and `agent.max_turn_secs` (default
  `300`, `0` disables either cap), closing broken sessions and clearing the
  follow-up window when tripped.
- `[Important]` `[Upstream]` `[Done: 9533fc9]` **Live progress updates in the
  `"Thinking…"` notification (#12).** Forwarded sanitized `toolSummary`/title
  summaries (never raw shell arguments) from `conductor.rs` through
  `Presentation::ProgressUpdate` with 400 ms trailing-edge throttling, burst
  coalescing, and `--replace-id` replacement in `Desktop`.
- `[Important]` `[Upstream + Config]` `[Done: bcf844e, NixConfig 11eddf5]`
  **Ambient desktop and time context injection (#13).** Added opt-in `[context]`
  (`time`, `desktop`), prepending a sanitized `<untrusted_desktop_context>`
  header with local date/time/offset/weekday (`localtime_r`) and active Hyprland
  workspace/window (with stale-signature and empty-workspace fallbacks) per user
  turn without busting `AGENTS.md` caching, plus `debug context`.
- `[Important]` `[Upstream]` `[Done: 199b2a9]` **Quick retrieval (`hey-computer
  last`) (#14).** Persisted the last successful answer atomically (originally to
  `<state_dir>/last.json`, unified into `history.db` in `521caf5`), added
  `hey-computer last [--copy | --open]` with HTML-disarmed viewer launching and
  `wl-copy`-safe `present.clipboard` handling (defaulting to
  `${pkgs.wl-clipboard}/bin/wl-copy` in the Home Manager module).
- `[Important]` `[Config]` `[Done: NixConfig 8329b59, 4a0f1aa]` **Interactive
  agy sandbox disabled & `toolPermission = "always-proceed"` (intended).** Set
  `enableTerminalSandbox = false` (broken on NixOS with agy 1.5.13) and
  intentionally set `toolPermission = "always-proceed"` in
  `antigravity-cli.nix` so commands, web requests, and MCP tools run without
  prompts unless matched by explicit `permissions.ask` or `permissions.deny`
  rules.
- `[Important]` `[Config]` `[Done: NixConfig 8329b59, 9e4d25e, 6deffe2]`
  **`instructions.extra` and rebuild deny list.** Added tone/style and stack
  preferences, metric/24h locale conventions, `Journal/YYYY-MM-DD.md` daily-note
  and top-of-list task insertion rules, default browser guidance,
  speech-recognition proper-noun glossary, and off-limits rules (plus
  `nixos-rebuild`/`rebuild` in `permissions.deny`).
- `[Important]` `[Upstream + Config]` `[Done: NixConfig 8329b59, 80e2cce, 37637a5, 905d20c]`
  **Generated `desktop-scripts`, Philips Hue (`openhue` + `home-lights`), &
  public `transit` skills.** Exposed `voice-command` (including `slack`,
  `signal`, `spotify`), `open-browser` (work/personal profiles in transient
  scopes), and host desktop scripts (`nightlight`, `powermode`, `caffeinate`,
  `toggle-zen`, `netbird-vpn`, `darkman`, `spotify-like`,
  `random-album-of-the-day`, `switch-kb-layout`, `open-linear-ticket`,
  `connect-galaxy-buds`, `GalaxyBudsClient`, `gcalcli`) via `extraPackages` and
  `desktop-scripts/SKILL.md`; added bundled `openhue` + host `hue-lights` /
  `home-lights` skills for Philips Hue smart scenes and room aliases; and added
  a live HVV / Deutsche Bahn `transit` CLI + `transit/SKILL.md`.
- `[Important]` `[Config]` `[Done: NixConfig 8329b59, f92c640, f2940c2, 4ece432, fc0fdab]`
  **Typed front end & cancel keybind.** Added minimalist `rofi-ask` `-modi`
  prompt (`SUPER + a`, matching `rofi-calculator`'s single-bar layout with
  dynamic follow-up/background-count placeholder and composable `! ` / `/new `,
  `/deep ` / `/d `, `/fast `, and `/bg ` / `/task ` prefixes) and cancel bind
  (`SUPER + CTRL + a`), standardized on `"Ask Computer"` UI labels.
- `[Important]` `[Config]` `[Done: NixConfig 8329b59, f92c640]` **Unified
  desktop context (`desktop-context`).** Replaced the separate "Ask about"
  shortcut/menu with a `desktop-context` CLI & skill (`selection`, `clipboard`
  text/image, frozen region/window/screen `capture` piped into `agy` via
  `read_file`, `copy`, and `paste`) usable directly from `SUPER + v`,
  `SUPER + ALT + v`, and `SUPER + a`.
- `[Important]` `[Config]` `[Done: NixConfig 8329b59, 4ece432]` **Waybar status
  indicator.** Added `custom/hey-computer` (`waybar-hey-computer watch` with
  `SIGUSR1` wakeups, replacing `custom/os-logo` on `hey-computer` hosts): shows
  the NixOS logo at idle (preserving left-click search and right-click power
  menu), an animated robot spinner while thinking (with active profile in the
  tooltip; click cancels), a robot-chat icon during the follow-up window (click
  opens `rofi-ask`), and a steady robot icon with task counts while background
  tasks are running or queued.
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
