# `hey-computer` TODOs

> **Tags:** `[Upstream]` = `hey-computer` repo, `[Config]` = `~/NixConfig` / desktop setup, `[Tool]` = own flake tool, `[npu-dictate]` = dictation frontend, `[Research]` = exploration / spike, `[Obsidian]` = vault organization
> **UI naming:** Use **"Ask Computer"** (not "Ask Hey Computer") in user-facing prompts, launchers, keybind descriptions, and UI labels.
> **Trust policy:** No confirmation/permission prompts — trust the agent to act directly; add permission prompts later only if needed. (Previously disregarded: vault Git auto-backup, `permissions.ask` confirmation tier, `hey-computer log`, env-var filtering.)

---

## 1. Bugs & Fixes

- [x] `[Upstream]` **Hide system prompts in the GTK viewer for scheduled & notification runs.**
  - Morning brief and other scheduled/repeating actions display the full system prompt covering most of the screen when opened in the GTK viewer — hide it entirely for scheduled/repeating runs.
  - Notification-acting has the same issue: a simple notification shows the full `<notification_task>` + `<untrusted_notification>` + `<untrusted_chat_context>` prompt wrapper (see Obsidian note `[[Hey Computer Notification Prompt Bug]]`).
- [x] `[Upstream]` `[Config]` **Fix notification hold-and-enrich timing and audio sync.**
  - The drafted reply is not always ready before the hold-back window expires — either always use an extremely fast model for notification triage/drafting or slightly increase the hold window.
  - The notification sound for a held-back notification currently plays immediately; bring it back in sync with when the notification is actually released.
- [x] `[Config]` **Fix Waybar `background` state styling.**
  - When `hey-computer` background work is active (`waybar-hey-computer` `background` class), the module renders with an ugly background in Waybar.
- [x] `[Config]` `[Upstream]` **Do not persist `hey-computer` notifications in SwayNC history.**
  - Prevent `hey-computer` notifications from staying persisted in SwayNC's notification panel.
- [x] `[Upstream]` **Fix clipboard clearing when closing the GTK visualizer.**
  - Copying text and then closing the visualizer clears the clipboard (likely Wayland clipboard ownership); resolved by routing viewer copies (including native `Ctrl+C`) through `present.clipboard` (`wl-copy`) with GTK fallback.
- [ ] `[Upstream]` **Auto-fix failed widget creations before rendering.**
  - When widget creation fails, render/validate it and auto-fix any errors before displaying it instead of showing an error.

---

## 2. Viewer, Shortcuts & Desktop UX

- [x] `[Upstream]` `[Config]` **Quick-reopen shortcut & persistent viewer window (`Ctrl+Super+A`).**
  - Implemented `hey-computer view --resume` / `hey-computer-viewer --resume` persisting last viewed thread/schedule and exact scroll position in `<state_dir>/viewer_last.json` (`0600`), and bound `SUPER + CTRL + a` to it in `~/NixConfig/home-manager/modules/hyprland.nix`.
  - *(Optional follow-ups)* Consider making `Cmd+Q` / `Esc` hide the GTK app instead of closing it, and evaluate a dedicated Hyprland special workspace.
- [ ] `[Upstream]` **Share / export answers in multiple formats.**
  - Add ability to share/export answers as Markdown (and other formats), including via a keyboard shortcut.
- [ ] `[Config]` **Disable `hey-computer` in super-light performance mode.**
  - Pause/stop `hey-computer` when switching into super-light power/performance mode.
- [ ] `[Config]` **Context-attached `rofi-ask` (`rofi-ask.nix`).**
  - Extend `home-manager/scripts/rofi-ask.nix` to read `HEY_COMPUTER_ATTACH_FILE` (newline/colon-separated paths → repeatable `-f`), `HEY_COMPUTER_ATTACH_IMAGE` (paths → repeatable `-i`), and `HEY_COMPUTER_PROMPT_PREFIX` (contextual placeholder) so screenshots, Tmux panes, and Yazi can invoke a universal **"Ask Computer about *this*"** popup.
- [ ] `[Config]` **Webcam-aware meeting mode (`webcam-privacy.nix`).**
  - On `/dev/video*` open, immediately run `waybar-hey-computer quiet-speech` and pause `hey-computer speak` so TTS never talks over a call; optionally enable SwayNC DND; if `location == "home"`, bump `Marius' Room` `Desk` Hue lamp brightness and restore state on camera close.

---

## 3. New Features & Workflows

- [x] `[Upstream]` `[Config]` **`plan-my-day` workflow.**
  - Especially for workdays: build a *"roter Faden"* for the day with concrete tasks and sub-TODOs, periodically reassess progress throughout the day, and optionally block time in Google Calendar for specific tasks.
- [ ] `[npu-dictate]` `[Config]` **Hotword detection & TTS barge-in.**
  - Always-on, local detection of `"Hey Computer"`, then recording until silence (VAD) and transcribing with the existing `npu-dictate` backend.
  - Evaluate small wake-word models such as `openWakeWord` (custom phrase training) running on CPU or NPU; likely belongs in `npu-dictate` rather than `hey-computer`, with no daemon dependencies.
  - Requirements: keep audio strictly local until the hotword fires; visible Waybar mic indicator; mute toggle; pause while another app holds the mic (calls) or while media is playing to avoid false triggers.
  - **TTS barge-in:** Interrupt active TTS playback immediately once speaking/wake-word fires via `hey-computer cancel` / `quiet-speech` (post-turn TTS cancellation and push-to-talk key-down silence are already wired; gate hotword barge-in via PipeWire `module-echo-cancel` / headphones or wake-word-only detection during TTS playback to avoid speaker self-triggers).
  - Measure the false-accept rate on real German and English speech before trusting it.
- [ ] `[Upstream]` **WhatsApp message-to-self replies.**
  - Allow the agent to respond to Marius's own messages via WhatsApp (evaluate whitelisting self-chat so outbound write is enabled strictly for self-messages).
- [ ] `[Upstream]` **Enable write access for suggested replies (WhatsApp/Signal).**
- [ ] `[Upstream]` **Add third model `"carbon"` for even deeper thoughts.**
- [ ] `[Config]` **Zero-token pre-meeting dossier (`waybar-gcal` cache).**
  - Run a 5-minute 0-token pre-collector reading `$XDG_RUNTIME_DIR/waybar-gcal/events.tsv` (zero API calls); 5–10 minutes before non-routine events (skipping the 10:00 Daily covered by `pre-daily`), wake `hey-computer` to gather relevant Gmail/Slack/Linear/Obsidian context on the topic or attendees.
- [ ] `[Config]` **End-of-day journal & task wrap-up.**
  - Companion to the Morning Brief in `Journal/YYYY-MM-DD.md`: around ~17:45 on workdays (or on demand via *"wrap up my day"*), synthesize `hey-computer recall --on today`, today's Linear/GitLab activity, and local git commits into an idempotent `## Evening Wrap-Up` section and review carried-over `- [ ]` tasks.
- [ ] `[Config]` **Include local news in Morning Brief.**
  - Include local news with links to actual stories instead of AI summaries; make it more extensive on non-work days with longer reads and recommendations for further reading.

---

## 4. Integrations & Tooling

- [ ] `[Config]` `[Upstream]` **`spotify_player` integration & Spotify info shortcut.**
  - Add `spotify_player` support/integration to `hey-computer` and document its CLI subcommands (`search`, `playback start`, `playlist`, `get key playback`, `connect`) in `desktop-scripts/SKILL.md` so prompts can search/play albums & playlists, queue tracks, or start instrumental focus music.
  - Add a Spotify info keyboard shortcut powered by `hey-computer`.
  - [ ] *Related prerequisite:* Update the `spotify_player` fork.
  - [ ] *Unblocks:* Recommendations for daily briefs on new releases and music (combined with Bandcamp/etc. and musical reads, maybe even a Fantano video).
- [ ] `[Config]` `[Obsidian]` **Tornqvist Coffee integration.**
  - Auto-inject coffees into an Obsidian template with empty picture and empty notes, pulling info from the roaster's website as well.
- [ ] `[Config]` **Pull *Album of the Day* into the Morning Brief.**
  - Integrate `random-album-of-the-day` into the Morning Brief with follow-up actions / rotation.
- [ ] `[Config]` **Replace ChatGPT browser fallback in `rofi-calculator.nix` (`SUPER+c`).**
  - Route Wolfram|Alpha misses (and `SKIP_WOLFRAM=1` inputs like `How`/`paste`/`.`) to `hey-computer ask --source typed` (or `--profile fast --new --print` to populate Rofi's Copy / Type / Paste rows inline) instead of opening `chatgpt.com` in Brave.
- [ ] `[Config]` **One-click "Ask Computer" toast action on `screenshot.nix` (`SUPER+s` / `SUPER+SHIFT+s`).**
  - Add `--action="ask=Ask Computer"` to `screenshot.nix`'s SwayNC toast to launch `rofi-ask` with `HEY_COMPUTER_ATTACH_IMAGE="$OUTPUT"` (pairs with Swappy annotations).
- [ ] `[Config]` **Context-aware LLM mode in `rofi-translate.nix` (`SUPER+SHIFT+t`).**
  - Keep `translate-shell` (`trans -b`) for instant single-word lookups, and add smart prefixes (`polish:`, `slack:`, `de!:`) or multi-sentence clipboard fallback calling `hey-computer ask --profile fast --new --print`.
- [ ] `[Config]` **Smart ticket extraction & quick brief in `open-linear-ticket.nix` (`SUPER+SHIFT+x`).**
  - Extract `[A-Z]{2,10}-[0-9]+` even from branch names or surrounding text, and add a `--action="brief=Brief in Computer"` toast action to fetch the Linear ticket + linked GitLab MRs + Slack mentions into a background brief.
- [ ] `[Config]` **Phone bridge via KDE Connect (`kdeconnect.nix`).**
  - Expose `kdeconnect-cli` in `extraPackages` + `desktop-scripts/SKILL.md` for `--share-text` / `--share` (links, train times, answers), `--ring`, and phone battery inspection.
- [ ] `[Config]` **Newsboat RSS digest (`newsboat.nix`).**
  - Add a read-only `newsboat-unread` helper querying `~/.local/share/newsboat/cache.db` for RSS Q&A, optional weekend reading digests, and chaining YouTube RSS items into the `yt-dlp` skill.
- [ ] `[Tool]` `[Config]` **Soccer matchday awareness (`golazo`).**
  - Expose a plain/JSON CLI subcommand in `golazo` + lightweight pre-collector to surface FC St. Pauli / Bundesliga kickoff times in weekend Morning Briefs and answer matchday questions without web search.
- [ ] `[Config]` **Zsh helpers & inline command generator (`zsh/init.nix`, superseding `atai`).**
  - Add `??` / `ask` shell function detecting piped stdin (`-f <tmp>`), `wtf` grabbing the last command (`fc -ln -1`) + exit status + ~60 lines of `tmux capture-pane`, and a ZLE `Alt+a` widget converting natural language in the command buffer into a runnable zsh command via `hey-computer ask --profile fast --new --print`.
- [ ] `[Config]` **Tmux pane snapshot → `hey-computer` (`tmux.nix`).**
  - Bind `prefix + A` to dump `tmux capture-pane -p -S -150` to a temp file and open `rofi-ask` with `HEY_COMPUTER_ATTACH_FILE` (works inside SSH sessions and TUIs).
- [ ] `[Config]` **Yazi integration (`yazi/default.nix`).**
  - Bind `A` in Yazi to pass hovered/selected files (`-i` for images, `-f` for text/code/logs) to `rofi-ask`.
  - Add `hey-computer view %s1` as an opener for `*.md` files.
  - Add the ability for `hey-computer` to open Yazi (e.g., at a target path/file for Google Drive upload, drag-and-drop, or quick file handoff).
- [ ] `[Config]` **Neovim integration (`neovim/nvim/lua/plugins/`).**
  - Add visual-mode `<leader>ha` sending selection + file path/filetype + active LSP diagnostics to `hey-computer ask` for fast side-questions.
  - Add a Telescope picker over `hey-computer recall --json --plain` to search past answers and yank/insert snippets into Obsidian notes (especially inside the `F11` `ghostty.obsidian` scratchpad).
- [ ] `[Upstream]` `[Config]` **Add `defuddle` to `hey-computer`.**
  - Integrate `defuddle` for cleaner web page extraction.
- [ ] `[Upstream]` `[Config]` **Bidirectional `claude-code` / `agy` integration.**
  - **Coding agent → `hey-computer`:** Let `claude-code` / `agy` delegate to `hey-computer` (saving TODOs, reading Obsidian, etc.) without polluting the coding agent's context; consider moving Slack out of there too.
  - **`hey-computer` → coding agent:** Let `hey-computer`'s agent start and orchestrate `claude-code` / `agy` sessions in `~/Projects` and `~/NixConfig`.
  - **GitLab MR review & pipeline diagnosis (`rofi-gitlab-search.nix`):** Add actions on `SUPER+SHIFT+g` (Summarize & Review MR with diff/risks/Linear context/unresolved threads, and Diagnose Failed Pipeline from CI logs) powered by the `claude-code` / `agy` coding integration.

---

## 5. Architecture, Packaging & Research

- [ ] `[Upstream]` `[Config]` **Consolidate Nix definitions into `hey-computer`.**
  - Move `hey-computer`-related Nix definitions currently living in `~/NixConfig` into the `hey-computer` repository where it makes sense.
- [ ] `[Config]` **Deduplicate proactive prompt files in `~/NixConfig`.**
  - Extract duplicated `morningDigestPromptFile` and `preDailyPromptFile` between `home-manager/scripts/hey-computer-proactive.nix` and `home-manager/modules/hey-computer.nix` to a single shared source before adding more proactive jobs.
- [ ] `[Config]` **Maintain downstream host (`yannix`) gating for all new NixConfig integrations.**
  - Keep all new dev/work/personal integrations (keybinds, Rofi actions, shell/editor hooks) gated behind `hasHeyComputer` or `host.isDev` per `NixConfig/CLAUDE.md`.
- [ ] `[Upstream]` `[Research]` **Move background work to a server application hosted on `nixner`.**
  - [ ] *Unblocks:* Raspberry Pi or similar low-powered (possibly ARM-based) satellite device with on-device hotword detection, STT, and TTS models — research viable hardware and models.
  - [ ] *Unblocks:* Phone support (and potentially wearables / ITO).
- [ ] `[Research]` **Obsidian memory sync / RAG integration.**
  - Research whether syncing `hey-computer` memory to Obsidian or adding a RAG-style layer makes sense for deeper vault integration.
- [ ] `[Research]` **Browser extension (Chromium / Firefox) for live browser context.**
  - Research a browser extension so `hey-computer` better understands active browser pages and context.
- [ ] `[Obsidian]` **Structure `hey-computer` ideas better in Obsidian.**
  - Organize `hey-computer` notes/ideas into a cleaner structure inside the vault.

---

## Recently Completed

- [x] Add `hey-computer` to the Whisper glossary.
- [x] Add `playerctl` support to `hey-computer`.
- [x] Investigate duplicate Morning Brief appearing twice.
