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

## Phase 2: Small upstream hardening & everyday latency/UX fixes — Implemented

All Phase 2 items (#8–#14) are implemented and listed under [Done](#done).

## Phase 3: Core delegation primitives (protocol, sessions, lanes) — Implemented

All Phase 3 items (#15–#19) are implemented and listed under [Done](#done).

## Phase 4: Integrations, context, hardening & smart routing

Built on background tasks (#17), profiles (#15), and MCP passthrough (#19). Per
the trust policy above, no daemon-level permission prompts are used — actions
execute directly, and permission prompts can be added later if ever needed.

**MCP vs. CLI rule of thumb:** Don't turn stateless one-liner CLIs into MCP
servers. Keep small local CLIs (lights, media, desktop toggles, `yt-dlp`, `gh`)
as `SKILL.md` scripts with zero turn-1 schema tokens; reserve MCP for stateful
OAuth/remote APIs or structured schema introspection, and keep heavy MCPs off
the default `fast` voice profile using per-profile `mcp_servers` (#15). For
NixOS packaging, evaluate
[`natsukium/mcp-servers-nix`](https://github.com/natsukium/mcp-servers-nix) for
pinned `/nix/store` derivations wired straight into
`programs.hey-computer.mcp.servers` instead of shelling out to `npx` at runtime.

20. `[Important]` `[Config]` **Google Calendar.** Reading ("what's on tomorrow",
    "when is my next meeting") and creating, moving, or deleting events. Target
    **`taylorwilsdon/google_workspace_mcp`** (MIT, supports `stdio`/HTTP,
    trimmed via `--tool-tier core` and/or `X-MCP-Toolsets` headers to cap schema
    bloat), keeping OAuth tokens managed by the server outside the model's
    context.
21. `[Important]` `[Config]` **Gmail.** Search, summarise ("anything important
    since this morning?"), draft, and send replies without permission prompts —
    served through **`taylorwilsdon/google_workspace_mcp`** alongside #20
    (unlike Google's hosted Developer Preview Gmail MCP, which focuses on
    read/label/draft rather than full sending).
22. `[Important]` `[Config]` **Context7 MCP (`deep`), `yt-dlp` / `gh` CLI
    skills, & built-in `agy` `/browser`.**
    - **Context7 MCP** mounted on the `deep` profile (2 tools, ~4.9k chars) to
      pull up-to-date library/framework docs into context instead of relying on
      stale model weights.
    - **`yt-dlp` CLI + `SKILL.md`** (no YouTube MCP for now) writing
      subtitles/transcripts to disk under `$HEY_COMPUTER_STATE_DIR/artifacts/`
      (#24) so 60+ minute videos don't hit MCP response size limits and cost
      zero turn-1 schema tokens.
    - **`gh` CLI + `SKILL.md`** (no GitHub MCP for now) for GitHub repos,
      issues, and PRs — GitHub is used frequently for private/personal projects
      (pairs with Personal mode in #34).
    - **Built-in `agy` `/browser` verification & fine-tuning:** Antigravity CLI
      already ships a `/browser` command and isolated `browser` subagent that
      attaches to local Chrome (v144+, `chrome://inspect/#remote-debugging`) via
      Chrome DevTools MCP with zero turn-1 schema cost. Verify and fine-tune how
      it works through `agy acp` (putting `nodejs`/`npx` on `extraPackages`
      `PATH`, Chrome remote debugging, and browser actuation settings) instead
      of adding a separate browser MCP.
23. `[Important]` `[Upstream + Config]` **Focused-window CWD & Ghostty/`tmux`
    ambient context + `tmux` skill.** Extend `[context] desktop` (#13) so that
    when the focused window is a terminal or editor, ambient context resolves
    its working directory, and when the focused window is **Ghostty** (where
    `tmux` auto-starts), includes lightweight `tmux` context as well. Pair this
    with a **`tmux` skill** so the agent can query pane content/metadata
    directly — enabling hands-free prompts like *"Explain this function"* to get
    context from `tmux` and read the entire open file instead of relying on OCR.
24. `[Important]` `[Upstream + Config]` **Artifacts directory & CLI large-output
    offloading (`$HEY_COMPUTER_STATE_DIR/artifacts/`).** Because `hey-computer`
    is an ACP client that does not intercept `agy`'s internal tool execution, it
    cannot rewrite MCP/shell tool outputs directly. Instead, have the daemon
    create a private `0700` `$HEY_COMPUTER_STATE_DIR/artifacts/` directory with
    automatic age pruning on agent start (alongside `history_days`), document it
    in `BUILT_IN`, and follow the convention in bulky CLI skills (`yt-dlp`,
    `tmux`, etc.) of writing outputs above ~16 KB to disk and returning a
    truncated preview + path for `rg`/`read_file`.
25. `[Important]` `[Upstream]` **Daemon hardening: consecutive tool-loop
    detection, live `memory.md` capacity header, & SQLite/FTS5 maintenance.**
    - **Consecutive tool-loop detection:** Add a lightweight repeat guard inside
      `ToolTracker` (`src/conductor.rs`, e.g. `agent.max_repeat_tool_calls = 3`,
      `0` disables) comparing each distinct `toolCallId`'s canonicalized tool
      name/kind and normalized JSON arguments (ignoring key order), aborting
      wedged retry loops via `limit_reached` in 3 steps instead of burning all
      20 (#9).
    - **Live `memory.md` capacity header:** When `environment::prepare()` builds
      `AGENTS.md` on agent start (#8), include a capacity header (e.g.
      `MEMORY: 2,840 / 4,000 chars (71%)`) so the model consolidates or prunes
      stale entries instead of blindly hitting truncation, while preserving
      frozen-snapshot prompt caching.
    - **Automatic SQLite / FTS5 maintenance:** Periodically merge FTS5 segments
      (`INSERT INTO turns_fts(turns_fts) VALUES('optimize')`) and run
      `PRAGMA optimize` / safe `VACUUM` in `src/store.rs` on daemon start or
      maintenance windows so `history.db` stays compact and `recall` stays fast.
26. `[Important]` `[Config]` **Shell-gated proactive timers (wiring up `#17`'s
    `--suppress-token` + split failure routing).** Systemd user timers / session
    hooks that run cheap shell pre-checks first and only wake
    `hey-computer ask -b --suppress-token HEARTBEAT_OK` when state warrants it:
    - **Morning digest:** triggered a couple minutes after first wake in the
      morning (once per day), writing a concise brief of today's calendar, mail,
      and mentions to `Journal/YYYY-MM-DD.md` and/or firing a notification.
    - **Location-aware work-schedule checks:** cheap IP check detecting whether
      the machine is **at home**, **at work**, or **neither**, used to gate
      proactive checks on the work schedule (and later tie into Work/Personal
      modes in #34).
    - **Pre-daily brief:** triggered a couple minutes before the **10:00 daily**
      standup (gated to workdays / work schedule).
    - **Split failure routing:** Clean no-ops stay silent via `HEARTBEAT_OK`,
      while hard failures (expired OAuth token, network down, script crash)
      route separately (dedicated alert/log or rate-limited failure notice)
      instead of vanishing silently or spamming every interval.
27. `[Important]` `[Upstream]` **Agent-authored hybrid scheduler.** Complement
    declarative Nix timers (#26) with a SQLite-backed scheduler inside the
    daemon so the agent itself can create, list, pause, edit, and test-run
    ad-hoc recurring or temporary jobs (e.g. *"check this PR every 15 minutes
    until CI passes"*, *"remind me every Friday at 16:00"*) without editing Nix
    config. Support optional `--script` pre-collectors (wake the LLM only when
    `stdout` is non-empty), zero-token `no_agent` jobs (run a script and notify
    directly with no LLM call), and split failure routing (`--failure-deliver`).
    Keeping jobs inside the daemon's SQLite store also keeps them cleanly
    isolated per mode once Work/Personal modes land (#34).
28. `[Important]` `[Upstream]` **Automatic model, effort, and lane routing
    (profile + background inference).** Builds on model and effort profiles
    (#15), attachments (#16), and background tasks (#17) so the user rarely
    needs explicit `/deep` or `/bg` prefixes. Options, cheapest first:
    - **Heuristics:** length of the request, keywords ("explain", "debug",
      "research", "derive", "compare", "write a report"), attachments → `deep`;
      short desktop commands → `fast`; long multi-step or research requests →
      automatically route to the background lane (`--background`) so quick
      follow-up voice commands stay unblocked.
    - **Escalation on failure:** start fast; if a tool fails, the answer is
      empty, or the follow-up starts with "no, …", retry the follow-up on the
      deeper profile.
    - **A triage turn (non-voice inputs only):** the fast model classifies the
      request with both a profile (`fast` vs `deep`) and a lane (`interactive`
      vs `background`) before the real turn when heuristics are unsure. **Never
      run LLM triage on voice inputs** (`SUPER + ALT + v`, voice prefix, or
      future hotword detection once implemented) so voice stays zero-overhead;
      keep it only for typed/CLI/other front ends (`rofi-ask` `SUPER + a`, CLI,
      future phone self-chat).

    Model and thought level are set per session (`session/set_config_option`),
    and mid-conversation switching is confirmed to work on `agy` 1.5.13, so
    routing both new sessions and follow-up escalations is cheap. Show which
    profile answered in the notification, so wrong routing is visible.

## Phase 5: Better output UX (viewer, distillation & spoken answers)

Built after the `$XDG_STATE_HOME` history store (#17–#18) and the typed front
end (#4) are in place.

29. `[Important]` **A much better answer view, with actions & interactive
    widgets (WebKitGTK).** Marker has known limits (README: an editor at heart
    with Open/Save buttons, umlauts break syntax highlighting, last release in
    2023). Build a dedicated **WebKitGTK** viewer that offers:
    - copy the whole answer or a single code block;
    - run a code block directly on click (no permission prompt);
    - open links (still disarmed until clicked);
    - a follow-up input field that continues the same conversation;
    - "save to vault" (append to today's note) and "open in editor";
    - a history of recent answers (ties in with `hey-computer last` #14 and the
      local recall index #18);
    - **interactive `show_widget` HTML widgets** baked directly into the viewer
      (personal-use trust model — no strict sandboxed iframe required): a
      `sendPrompt(text)` JS bridge so widget buttons/forms can trigger follow-up
      turns in the same conversation, a theme bridge injecting live light/dark
      CSS variables (pairs with `darkman`), and an uncaught-error reporter that
      surfaces broken widget JS back to the session.

    Short answers should get notification actions too: Copy, Reply (opens the
    typed front end #4 in follow-up mode) and Open in viewer. Keep HTML and
    remote-image disarming for regular Markdown answers.
30. `[Important]` `[Upstream]` **Periodic memory & skill distillation
    (`hey-computer distill`).** Dedicated CLI subcommand (run manually or via a
    weekly background task) that reviews recent `history.db` turns (#17–#18)
    against `instructions.memory_file` (#8) and proposes a minimal diff to
    `memory.md` (promoting durable preferences/facts while respecting the
    4,000-char cap and pruning stale entries) and/or draft `SKILL.md` files for
    workflows repeated 3+ times. Pairs with the improved viewer (#29) so you
    review and apply diffs manually — never auto-applied, avoiding skill
    poisoning.
31. `[Important]` `[Upstream]` **Optional audio feedback hook (`present.speak` /
    earcons, streaming-ready).** For hands-free voice ergonomics, add an
    optional `present.speak` hook (e.g. Piper or Kokoro) or subtle earcons that
    can accept streamed sentence chunks as `agent_message_chunk` updates arrive
    from `conductor.rs`, with `cancel` stopping playback immediately.
32. `[Important]` **Optional spoken answers (streaming TTS).** Expands the
    `present.speak` hook (#31). Use a local TTS engine (Piper or Kokoro) with a
    German and an English voice, streaming completed sentences as chunks arrive
    over ACP to cut time-to-first-audio (detecting DE vs EN on sentence 1).
    Speak short answers whole; if subsequent chunks push past the short
    threshold (or hit code blocks/tables), stop queuing further sentences and
    open the viewer instead (never stream TTS on background or
    `--suppress-token` turns). `cancel` or pressing the talk key stops speech at
    once (and barge-in stops it once hotword detection lands). Lower media
    volume (or pause via playerctl) while speaking. Make it a toggle that is
    easy to reach (voice command, Waybar click), because it's wanted at the desk
    only sometimes.

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
34. `[Important]` **Work and personal modes.** A mode is a named profile of
    everything the agent can reach: MCP servers, skills, instructions, vault,
    accounts and model.
    - **Work:** the employer's Linear (official MCP with read/write access,
      already wired via `mcp_oauth_tokens_file`), GitLab, Slack and Google
      Calendar.
    - **Personal:** personal Gmail and Google Calendar, private GitHub projects
      (`gh`), WhatsApp, Signal.

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
      account) on purpose.
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
  playback (#32) immediately once speaking/wake-word fires (gated via PipeWire
  `module-echo-cancel` / headphones or wake-word-only detection while TTS plays
  to avoid speaker self-triggers). Measure the false-accept rate on real speech
  in both languages before trusting it.

## Done

- `[Lower priority]` `[Upstream]` `[Done: 3c7390b]` **Timed-out turns & generic
  deny-list message (#10, #11).** Marked `TurnAbort::Quiet` as `broken: true`
  so timed-out sessions are closed instead of extending `follow_up_until`,
  replaced the hardcoded *"which would become root"* message with a generic
  deny-list refusal, scoped `acp::tool_command()` to shell execution calls, and
  added outbound trace redaction for attachment/MCP payloads.
- `[Important]` `[Upstream]` `[Done: f359999]` **Model and effort profiles
  (`--profile`) (#15).** Added `[profiles.<name>]` (`model`, `thought_level`,
  `max_turn_secs`, `max_tool_calls`, `mcp_servers`) and `ask -p/--profile`,
  follow-up profile inheritance, in-place mid-conversation
  `session/set_config_option` switching (verified live on `agy` 1.5.13), warm
  spare preservation for non-default profiles, and profile-tagged progress and
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
- `[Important]` `[Upstream]` `[Done: d0b7732, 815dbfb]` **MCP passthrough (#19).**
  Added `src/mcp.rs`, `[mcp.<name>]` (`env_files` / `header_files` loaded at
  prepare time so secrets stay out of `config.toml` and `/nix/store`),
  `agent.mcp_oauth_tokens_file`, per-profile `mcp_servers` allowlists enforced
  per turn by `LanePolicy`, and `programs.hey-computer.mcp.servers` in
  `nix/home-manager.nix`. Writes enabled servers to
  `<agent_dir>/config/mcp_config.json` (mode `0600`) and passes `mcpServers: []`
  on `session/new` so `agy acp` 1.5.13 enables lazy `call_mcp_tool` loading
  without eagerly truncating large tool sets, while preserving conversations on
  mid-session profile switches across different `mcp_servers` allowlists.
- `[Important]` `[Upstream]` `[Done: 3e3b6e1]` **Bounded, frozen-snapshot memory
  and fail-loudly prompt rule (#8).** Added `instructions.memory_file` /
  `instructions.memory_max_chars` (default `4000`, Home Manager `memory.file`
  defaulting to `~/.config/hey-computer/memory.md`, rejected inside the vault or
  via symlinks into the vault), loaded into `AGENTS.md` on agent start and kept
  frozen for the life of the agent process, plus the explicit fail-loudly rule
  in `BUILT_IN`.
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
- `[Important]` `[Upstream]` `[Done: bcf844e]` **Ambient desktop and time
  context injection (#13).** Added opt-in `[context]` (`time`, `desktop`),
  prepending a sanitized `<untrusted_desktop_context>` header with local
  date/time/offset/weekday (`localtime_r`) and active Hyprland workspace/window
  (with stale-signature and empty-workspace fallbacks) per user turn without
  busting `AGENTS.md` caching, plus `debug context`.
- `[Important]` `[Upstream]` `[Done: 199b2a9]` **Quick retrieval (`hey-computer
  last`) (#14).** Persisted the last successful answer atomically to
  `<state_dir>/last.json` (mode `0600`) in `src/store.rs`, added `hey-computer
  last [--copy | --open]` with HTML-disarmed viewer launching and `wl-copy`-safe
  `present.clipboard` handling (defaulting to `${pkgs.wl-clipboard}/bin/wl-copy`
  in the Home Manager module).
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
