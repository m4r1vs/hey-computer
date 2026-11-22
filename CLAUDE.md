# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A user daemon (Rust, edition 2024, tokio) that keeps one ACP agent process (`agy acp`, the
Antigravity CLI) warm and answers one-shot requests: `hey-computer ask` hands it a request,
usually a speech transcript; the agent works on it with tools; the answer is shown as a
notification when short, in a Markdown viewer (Marker by default) when long or rich. README.md
is the user-facing reference: command line, socket protocol, configuration, Home Manager
module, permission model. Keep it, `Config` in `src/config.rs`, and the module's options in
`nix/home-manager.nix` in sync when adding a key.

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

One binary. `daemon` owns the socket; every other subcommand is a client.

- `protocol`, `ipc`: the JSON-lines socket protocol (documented at the top of
  `protocol.rs` and in README) and the client side. `lines`: bounded line reading shared by
  both ends.
- `daemon`: binds `$XDG_RUNTIME_DIR/hey-computer.sock` (0600) **before** preparing the agent
  directory, so a second daemon is refused without touching the first one's files; then
  serves clients and hands requests to the conductor.
- `conductor`: one task that owns the agent child and runs one request at a time. Everything
  that can interrupt a request (newer request, `cancel`, hang-up, agent death, inactivity)
  meets in one `select!`. The `Handle` numbers requests and cancels the previous one as it
  queues the next. Follow-ups reuse the session inside `conversation.follow_up_secs`. A spare
  session is opened at warm start because only the first `session/new` is slow.
- `acp`: hand-written ACP client (newline-delimited JSON-RPC, protocol 1) over the child's
  stdio. Hand-written on purpose: agy sends fields outside the specification, and the
  official crate's interoperability with it was unproven.
- `environment`: the agent's private `--gemini-dir` (settings.json, linked skills) and the
  workspace with the generated `AGENTS.md`. Rewritten before every agent start.
- `policy`: the deny list, matched as command words through wrappers, `sh -c`, pipes and
  `$(…)`. A courtesy; the boundary is `NoNewPrivileges` on the service.
- `present`: `Presenter` trait; `Desktop` runs the notifier (an argument vector,
  notify-send by default, so tests and hand runs can swap in a recording script) and the
  viewer; `Recorder` for tests. `is_short`/`is_rich` decide notification versus viewer.
- `mock`: a scripted ACP agent, run as the hidden `debug mock-agent SCRIPT`, so the package
  installs one binary. Script format and log lines are documented at the top of `mock.rs`.
- `debug`: hidden commands that check one part at a time (`config`, `instructions`,
  `prepare`, `classify`, `present`, `prompt`, `mock-agent`).
- `nix/home-manager.nix`: the module (agent PATH, obsidian shim, skills, viewer launched via
  `systemd-run --user --scope`, service with `NoNewPrivileges`). `nix/module-check.nix`
  evaluates it in a minimal Home Manager configuration (`checks.module`).
- `skills/hyprland-desktop`: the bundled skill, installed to
  `$out/share/hey-computer/skills`. Write only what has been checked against a real
  Hyprland; say so where something comes from `--help` alone.

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
  0.8 s in one run and 5.7 s in another). Hence inactivity timeouts, not total caps.
- agy's terminal sandbox fails on some NixOS hosts; with it on, a `hyprctl version` turn took
  16.6 s, off 6.4 s. `enableTerminalSandbox = false` and `toolPermission = "request-review"`
  are the generated defaults; both changed between the two runs.
- Under `NoNewPrivileges=yes` agy starts and answers normally, and `sudo -n true` run by the
  agent prints `The "no new privileges" flag is set, which prevents sudo from running as
  root.` (checked in a transient user unit with the module's PATH and properties).
- Marker: unique GApplication; with a reachable session bus a second `marker --preview`
  merges into the first window as an editor tab. Unsetting `DBUS_SESSION_BUS_ADDRESS` is not
  enough (GLib falls back to `$XDG_RUNTIME_DIR/bus`); `DBUS_SESSION_BUS_ADDRESS=disabled:`
  gives one preview window per answer. `--name=hey-computer.answer` sets the Wayland app id.
- Hyprland 0.55 with a Lua config: `hyprctl dispatch X` evaluates `hl.dispatch(X)`; errors
  still exit 0; a stale `HYPRLAND_INSTANCE_SIGNATURE` gives exit 4, and
  `hyprctl instances -j` finds the live instance.

Not yet seen: agy's real wording for an expired login (tested only against the mock's
`session_error`). agy's own history under the state directory is never pruned.

## Testing

- `tests/daemon.rs` runs `daemon::serve(config, socket, Arc<Recorder>, shutdown)` in-process,
  with the agent command `[CARGO_BIN_EXE_hey-computer, "debug", "mock-agent", script.json,
  "--log", mock.log]`. Covered: streamed chunks, an allowed permission, a deny-list refusal
  with the daemon's own message, follow-ups inside and outside the window, cancellation by a
  newer request, `cancel`, a client that hangs up, crash and restart, inactivity timeout,
  malformed and oversized agent lines, malformed requests, a second client, a second daemon,
  an expired login (mock), a missing agent, cold start, idle stop.
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

Request text, answer text, tool call arguments and the agent's stderr when it dies
mid-request (it can quote the request). `info` carries ids, sizes, timings and tool kinds
(`tool call (execute)`, `refused a sudo command`). Keep it that way: requests are spoken
words and answers can quote the user's notes.

## Conventions

As in npu-dictate: comments explain why, not what; `tracing` for logs; hidden `debug …`
subcommands for checking one part on its own; `#[serde(default, deny_unknown_fields)]` on
every config section; few dependencies.
