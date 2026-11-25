# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A user daemon (Rust, edition 2024, tokio) that keeps one ACP agent process (`agy acp`, the
Antigravity CLI) warm and answers one-shot requests: `hey-computer ask` hands it a request,
usually a speech transcript; the agent works on it with tools; the answer is shown as a
notification (plus optional DE/EN sentence-streamed TTS when asked via `--voice`) when short,
or in the native GTK4 + WebKitGTK 6.0 viewer (`hey-computer-viewer`) when long, rich, or
carrying `mcp-ui` widgets. README.md is the user-facing reference: command line, socket
protocol, configuration, Home Manager module, permission model. Keep it, `Config` in
`src/config.rs`, and the module's options in `nix/home-manager.nix` in sync when adding a key.

The repository is public. Never commit local paths, host names, personal names, e-mail
addresses (git's author field aside), credentials, or recorded agy logs: they carry account
and session identifiers. Test fixtures and mock scripts are written by hand with invented
values.

## Commands

Inside `nix develop` (cargo, clippy, rustfmt, rust-analyzer, alejandra, jq, socat,
notify-send; `RUST_LOG=hey_computer=debug,info`):

```sh
cargo test                                      # unit + integration tests (mock agent)
cargo test --test daemon a_newer_request        # one integration test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
alejandra -q flake.nix nix/*.nix                # Nix formatting (never `nix fmt` without a path)
nix build                                       # ./result
nix flake check                                 # build+tests, clippy, rustfmt+alejandra, module
```

`nix flake check` is the gate; every commit should pass it. Flakes only see git-tracked
files: `git add` new files before building.

## Architecture

One package, two binaries: `hey-computer` (`daemon`, `ask`, `cancel`, `status`, `speak`,
`distill`, `view`, `last`, `tasks`, `recall`, `schedule`, internal `mcp-ui`, and `debug` —
pure Rust, never linking GTK4/WebKitGTK) and `hey-computer-viewer` (native GTK4 + WebKitGTK
6.0 viewer with `application_id = "hey-computer.answer"`, `NON_UNIQUE`). `daemon` owns the
socket; every other subcommand is a client.

- `protocol`, `ipc`: the JSON-lines socket protocol (documented at the top of
  `protocol.rs` and in README) and the client side. `lines`: bounded line reading shared by
  both ends.
- `daemon`: binds `$XDG_RUNTIME_DIR/hey-computer.sock` (0600) **before** preparing the agent
  directory, so a second daemon is refused without touching the first one's files; then
  serves clients and hands requests to the conductor.
- `conductor`: coordinates the shared warm `agy acp` process across one
  interactive lane (1 active request, newer asks supersede, follow-ups reuse
  the session inside `conversation.follow_up_secs` and inherit the active
  profile, in-place `session/set_config_option` switching) and a bounded FIFO
  background lane (`[tasks]`, concurrent sessions on the same `agy` process,
  never superseded by interactive asks, `--suppress-token` silent checks). A
  spare session is opened at warm start for the default profile because only
  the first `session/new` is slow; non-default profiles preserve it.
- `acp`: hand-written ACP client (newline-delimited JSON-RPC, protocol 1) over
  the child's stdio, split into `Connection` (process lifecycle) + cloneable
  `Client` + per-`sessionId` RAII `SessionSubscription` demuxed inside
  `read_loop`. Outbound trace logging redacts attachment payloads, file bodies,
  and `mcpServers` secrets. `tool_command()` is scoped to `execute` /
  `run_command` so MCP tool calls with a `command` argument are not misparsed
  as shell commands. Hand-written on purpose: agy sends fields outside the
  specification, and the official crate's interoperability with it was unproven.
- `attachments`: zero-dependency loader for `--file` (UTF-8 embedded
  `TextResourceContents`) and `--image` (magic-byte verified PNG/JPEG/WebP/GIF
  with RFC 4648 base64 encoding), enforcing `O_NONBLOCK | O_NOCTTY` regular-file
  checks and size caps (`8` files, `512 KiB` text, `10 MiB` image, `16 MiB`
  total raw bytes).
- `mcp`: builds `<agent_dir>/config/mcp_config.json` (mode `0600`) from
  `[mcp.<name>]` stdio/HTTP/SSE definitions, loading `env_files` / `header_files`
  at prepare time (trimmed, `16 KiB` cap, skipping servers with missing secret
  files without logging secret values). Per-profile `mcp_servers` allowlists
  and restricted-lane MCP blocking are enforced on `call_mcp_tool` / `mcp_*`
  permission requests in `policy.rs`.
- `environment`: the agent's private `--gemini-dir` (settings.json, linked skills),
  the `0700` `artifacts/` scratch dir (age-pruned alongside `history_days`), and the
  workspace with the generated `AGENTS.md` (including `## Turn budgets` and live
  `MEMORY: <used> / <max>` capacity header across `memory_file` + `memory_files`,
  automatically refreshed when idle outside follow-up windows if memory files change).
  Rewritten before every agent start.
- `context`: opt-in per-turn ambient time (`localtime_r`), Hyprland active
  window/workspace, focused terminal/editor `/proc` CWD, and focus-aware Ghostty/terminal
  `tmux` session/window/pane metadata prepended inside `<untrusted_desktop_context>`,
  plus authoritative per-turn `<budget>` headers.
- `store`: private `0600` SQLite WAL + FTS5 database at
  `<state_dir>/history.db` (schema v3 with `turns` + `schedules` including
  declarative `managed` jobs, `PRAGMA secure_delete = ON`, automatic FTS5/SQLite
  `maintain_connection`, dedicated `hc-store` actor thread with
  `wal_checkpoint(TRUNCATE)` on shutdown, crash recovery, monotonic ID seeding
  from `sqlite_sequence`, legacy `last.json` migration, safe FTS5 query sanitizer
  + BM25/recency ranking, `<untrusted_recall>` formatter) backing `last`,
  `tasks`, `recall`, and `schedule`.
- `scheduler`: SQLite-backed hybrid job scheduler (`[scheduler]`, `hey-computer schedule`)
  with local-time human/cron specs, `--script` pre-collectors (0 tokens on empty stdout),
  `--no-agent` timed notifications, `Policy::denied_word` validation, and split
  `--failure-deliver` routing (`notify` with exponential backoff, `log`, `silent`).
- `routing`: opt-in 3-tier automatic profile and lane routing (`[routing]`,
  `InputSource` `--voice`/`--source`): zero-latency heuristics (including follow-up
  correction escalation and auto-backgrounding), Tier 2 retry-on-failure escalation,
  and Tier 3 restricted LLM triage strictly for non-voice unsure new turns.
- `policy`: `LanePolicy` (`standard` and Phase 6-ready `restricted`) + deny
  list matched as command words through wrappers, `sh -c`, pipes and `$(…)`,
  and behind `systemd-run`. A courtesy, not a boundary.
- `present`: `Presenter` trait; `Desktop` runs the notifier (an argument vector,
  notify-send by default, so tests and hand runs can swap in a recording script)
  and the viewer, including profile-tagged progress/answer headers and bounded
  non-blocking `notify-send --action=open=Open` waiters for background task
  completions; `Recorder` for tests. `is_short`/`is_rich` decide notification
  versus viewer. `to_markup` cuts code spans out before pairing `**`, so the
  Pango markup always nests. `disarm_html` rewrites every `<` outside code
  (`&lt;` in text, `\lt` in math) and turns remote images into links (Marker
  fetched them at once) before the answer file is written: Marker renders in a
  web view. "Answer opened" is sent once the viewer spawns; a viewer that fails
  within 1.5 s replaces it with an error notification.
- `mock`: a scripted ACP agent, run as the hidden `debug mock-agent SCRIPT`, so the package
  installs one binary. Script format and log lines are documented at the top of `mock.rs`.
- `debug`: hidden commands that check one part at a time (`config`, `instructions`, `context`,
  `prepare`, `classify`, `present`, `prompt`, `mock-agent`).
- `nix/home-manager.nix`: the module (agent PATH including `cfg.package`,
  obsidian shim, `recall.enable`, `mcp.servers` lowering `{ file = ...; }` to
  `env_files`/`header_files`, skills, viewer launched via
  `systemd-run --user --scope`, service with `NoNewPrivileges`). `nix/module-check.nix`
  evaluates it in a minimal Home Manager configuration (`checks.module`).
- `skills/{hyprland-desktop,openhue,recall,tmux,schedule}`: bundled skills installed to
  `$out/share/hey-computer/skills`. Write only what has been checked against a
  real system; say so where something comes from `--help` alone.

## Facts the code depends on (agy 1.5.13, measured)

- Handshake: `initialize` → `session/new {cwd, mcpServers}` →
  `session/set_config_option` (model, thought_level; mode=default if needed) →
  `session/prompt`; answers stream as `session/update`; the prompt returns a `stopReason`.
  No `authenticate` (the login is in the keyring, reached over the session bus).
  `--model/--effort/--mode` before `acp` are ignored; model and effort are per session.
- `session/cancel` takes effect in about 10 ms; a chunk can still arrive between the cancel
  and the response, so a cancelled turn's queued updates are drained before the next one.
- In default mode a tool call arrives as `session/request_permission` with `allow_once` /
  `reject_once`. A rejected call ends the turn with **no text**, so the daemon writes the
  message itself (`failed`/`denied`).
- agy loads `AGENTS.md`, `GEMINI.md` and `.agents/` from every workspace directory as trusted
  instructions: an `AGENTS.md` in a vault passed as `additionalDirectories` made it append a
  word to every answer. The vault is therefore never a workspace directory. The agent reaches
  it like any other folder (reads and edits arrived without a permission request, `find` as an
  allowed `execute`), and the same `AGENTS.md` then had no effect, also on a follow-up after
  the agent had read and edited the vault.
- Timings: agent start 0.09–0.17 s; first session 0.8–1.2 s, later 0.02–0.06 s; a plain
  answer 1.5–2.8 s; one shell command 5–10 s (tool call to permission request alone was
  0.8 s in one run and 5.7 s in another). Hence inactivity timeouts alongside generous hard turn caps (`max_turn_secs`, `max_tool_calls`).
- agy's terminal sandbox fails on some NixOS hosts; with it on, a `hyprctl version` turn took
  16.6 s, off 6.4 s. `enableTerminalSandbox = false` and `toolPermission = "request-review"`
  are the generated defaults; both changed between the two runs.
- Under `NoNewPrivileges=yes` agy starts and answers normally, and `sudo -n true` run by the
  agent prints `The "no new privileges" flag is set, which prevents sudo from running as
  root.` (checked in a transient user unit with the module's PATH and properties). It is
  not a sandbox: `systemd-run --user …` from inside such a unit runs with `NoNewPrivs: 0`
  (checked), and so does anything the compositor or D-Bus activation starts. The README's
  permission model says so plainly; never describe NoNewPrivileges as a boundary.
- Marker: unique GApplication; with a reachable session bus a second `marker --preview`
  merges into the first window as an editor tab. Unsetting `DBUS_SESSION_BUS_ADDRESS` is not
  enough (GLib falls back to `$XDG_RUNTIME_DIR/bus`); `DBUS_SESSION_BUS_ADDRESS=disabled:`
  gives one preview window per answer. `--name=hey-computer.answer` sets the Wayland app id.
- Hyprland 0.55 with a Lua config: `hyprctl dispatch X` evaluates `hl.dispatch(X)`; errors
  still exit 0; a stale `HYPRLAND_INSTANCE_SIGNATURE` gives exit 4, and
  `hyprctl instances -j` finds the live instance.
- Without a login (seen with the keyring out of reach, `DBUS_SESSION_BUS_ADDRESS=disabled:`, or
  with GNOME Keyring still locked at boot), agy starts normally and fails `session/new` with
  `-32000 Authentication required` (ACP's `auth_required`). Once silent sign-in fails, that `agy
  acp` process latches its unauthenticated state and never re-reads the keyring on later
  `session/new` calls; after a 5 s keyring timeout it also writes
  `antigravity-cli/cache/antigravity-keyring-unavailable` and skips the keyring for 1 h. The
  daemon therefore clears that marker in `environment::prepare`, stops the agent on
  `auth_required` without scheduling a background restart (so the next request spawns a fresh
  agent), and turns the error into a message naming `agy acp --login`.
- agy's conversations, transcripts and logs (`agy/antigravity-cli/{conversations,brain,log}`)
  are pruned to `agent.history_days` before each agent start, never while agy runs.
- Multi-session concurrency (measured on agy 1.5.13): default single-process `agy acp`
  executes concurrent `session/prompt` calls across distinct sessions with accurate
  `sessionId` routing on `session/update` and `session/request_permission` and isolated
  `session/cancel`. Passing `--process-per-session` regressed second-session creation from
  0.02 s to 1.06 s, so single-process mode is kept.
- Mid-conversation `session/set_config_option` (measured): works for both `model` and
  `thought_level` while preserving earlier turn context.
- Attachments (measured): `type: "image"` (including 6 MiB raw / 8 MiB base64 PNG) and
  `type: "resource"` (`TextResourceContents`) work; PDF `BlobResourceContents` is ignored
  by agy 1.5.13.
- MCP loading (measured on agy 1.5.13): servers passed in ACP `session/new` `mcpServers`
  are declared **eagerly** with `call_mcp_tool` absent; when combined schemas exceed the
  eager budget (~87 smallest tools / ~1 KB each across 197 tools), larger tools (e.g.
  `slack` `conversations_history` and `nixos` `nix`) are silently dropped. Writing servers
  to `<agent_dir>/config/mcp_config.json` (`serverUrl` for HTTP/SSE, `command`/`args`/`env`
  for stdio) and sending `mcpServers: []` on `session/new` enables `call_mcp_tool` lazy
  loading with all tools intact and schemas kept on disk. OAuth-authenticated remote HTTP
  MCP servers read tokens from `<agent_dir>/antigravity-cli/mcp_oauth_tokens.json` (without
  it, `session/new` succeeds silently with 0 tools for that server);
  `agent.mcp_oauth_tokens_file` symlinks an existing token store into the private agent dir.
  Lazy `call_mcp_tool` permission requests arrive with `name: "call_mcp_tool"`,
  `kind: "execute"`, and `rawInput: {ServerName, ToolName, Arguments}` (so `acp::tool_command`
  explicitly excludes `call_mcp_tool`, and `LanePolicy` gates `ServerName`). Stdio MCP
  children persist across `session/close` until `agy` exits (cleaned up by `Connection`'s
  process-group kill).

## Testing

- `tests/daemon.rs` runs `daemon::serve(config, socket, Arc<Recorder>, shutdown)` in-process,
  with the agent command `[CARGO_BIN_EXE_hey-computer, "debug", "mock-agent", script.json,
  "--log", mock.log]`. Covered: streamed chunks, an allowed permission, a deny-list refusal
  with the daemon's own message, follow-ups inside and outside the window, cancellation by a
  newer request, `cancel`, a client that hangs up, crash and restart, inactivity timeout,
  malformed and oversized agent lines, malformed requests, a second client, a second daemon,
  a missing login (mock, in agy's words), a login that appears later without restarting the
  daemon, a missing agent, cold start, idle stop, a shutdown
  during a turn and during a hanging start (both must end within 2 s), a correction while the
  agent starts (one start, not two), a config out of range, a file at the socket path. The
  mock's `init_delay_ms` makes a slow start. `tests/cli.rs` runs the binary as a voice front
  end would: every refusal of an `ask` without `--print` must be a notification.
- The late-chunk check in `a_newer_request_cancels_the_running_one` fails every time when
  neither `cancel_turn` nor `drain_stale` reads the cancelled turn's last update; keep the
  run loop `biased` (inbox first), or it passes by luck.
- Real checks against agy cost the user's quota and run commands without confirmation: use
  only harmless prompts (one word, arithmetic, `hyprctl version`, a note in a scratch vault),
  a scratch `state_dir`, an own socket (`--socket` or `HEY_COMPUTER_SOCKET`), and a recording
  script as `present.notifier` and `present.viewer`. Never point a test at a real vault.
- The obsidian shim talks to a running Obsidian over `$XDG_RUNTIME_DIR/.obsidian-cli.sock`:
  keep it (and the obsidian-cli skill) out of real agy runs unless the runtime dir is a
  scratch one.
- Anything that opens a window belongs in a nested or headless compositor, not on the
  user's session.

## Never logged above debug

Request text, answer text, memory file contents, ambient desktop context (window titles),
tool call arguments, attachment contents/base64, recall queries/answers, MCP secret values,
and the agent's stderr when it dies mid-request (it can quote the request). `info` carries
ids, sizes, timings and tool kinds (`tool call (execute)`, `refused a sudo command`). Keep it
that way: requests are spoken words and answers can quote the user's notes; base64 payloads,
memory contents, and MCP secrets are never logged even at `trace`.

## Conventions

As in npu-dictate: comments explain why, not what; `tracing` for logs; hidden `debug …`
subcommands for checking one part on its own; `#[serde(default, deny_unknown_fields)]` on
every config section; few dependencies.
