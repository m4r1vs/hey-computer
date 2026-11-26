# Draft: NixConfig Script & Workflow Integrations for `hey-computer`

> **Status:** Draft brainstorm (2026-11-26)
> **Scope:** Ideas for integrating existing custom scripts, launchers, daemons,
> and tools in `~/NixConfig` with `hey-computer` (`[Config]` = NixConfig Home
> Manager/scripts, `[Upstream]` = `hey-computer`, `[Tool]` = own flake tool).
> User-facing labels follow the **"Ask Computer"** naming convention from
> `IMPROVEMENTS.md`.

## Context & Architectural Premise

`hey-computer` already provides a warm dual-profile daemon (`fast`/`deep`),
background task lane, SQLite FTS5 `recall`, Piper EN/DE streaming TTS, native
Markdown/widget viewer (`hey-computer view`), SwayNC hold-and-enrich plumbing,
`draft` checkpoints, and 0-token shell pre-collectors (`location`, `workday`,
`weather`).

Most opportunities below come from two under-used seams:

1. **Scripts → `hey-computer`:** `hey-computer ask` already accepts `-f <file>`,
   `-i <image>`, `--print`, `--background`, `--profile`, and `--reply-to`, yet
   almost none of the existing Rofi menus, notification actions, or terminal
   tools call it.
2. **`hey-computer` → Scripts & Tools:** Several rich tools in `NixConfig`
   (wallpaper palettes, full `spotify_player` CLI, KDE Connect, Newsboat RSS,
   `golazo`, homelab/Kubernetes CLI) are not yet exposed via `desktopScripts` /
   `skills.extra`.

---

## 0. Keystone Unlock: Context-Attached `rofi-ask`

- `[Config]` **Attachment env vars in `home-manager/scripts/rofi-ask.nix`.**
  Currently `rofi-ask` only reads `HEY_COMPUTER_REPLY_TO`. Extending it (~15
  lines) to also read:
  - `HEY_COMPUTER_ATTACH_FILE` (newline/colon-separated paths → repeatable `-f`)
  - `HEY_COMPUTER_ATTACH_IMAGE` (paths → repeatable `-i`)
  - `HEY_COMPUTER_PROMPT_PREFIX` (optional contextual placeholder, e.g.
    `"Ask Computer about screenshot.png..."`)

  turns `rofi-ask` into a universal **"Ask Computer about *this*"** popup that
  screenshots, clipboard history, Obsidian notes, Tmux panes, and Yazi file
  selections can all invoke identically.

---

## 1. Direct Upgrades to Existing Scripts & Rofi Menus

- `[Config]` **Replace the ChatGPT browser fallback in `rofi-calculator.nix` (`SUPER+c`).**
  - When Wolfram|Alpha returns `"Wolfram|Alpha did not understand your input"`
    (or on `How`/`paste`/`.` where `SKIP_WOLFRAM=1`), `rofi-calculator.nix`
    currently falls back to opening `https://chatgpt.com/?q=...` in Brave (next
    to a commented-out `totoro` stub).
  - Route misses to `hey-computer ask --source typed` (or `--profile fast --new
    --print` to populate Rofi's existing Copy / Type / Paste rows inline). Keeps
    deterministic Wolfram|Alpha math while eliminating external browser tabs.

- `[Config]` **One-click "Ask Computer" on `screenshot.nix` (`SUPER+s` / `SUPER+SHIFT+s`) and `ocr-screenshot.nix` (`SUPER+CTRL+s`).**
  - `screenshot.nix` already pops a SwayNC toast with `--action="gimp=Open in
    Gimp"` and `--action="path=Copy Path"`. Add `--action="ask=Ask Computer"` to
    launch `rofi-ask` with `HEY_COMPUTER_ATTACH_IMAGE="$OUTPUT"`.
  - Pairs especially well with `SUPER+SHIFT+s` (Swappy): circle/annotate a UI
    bug or diagram, click **Ask Computer** on the notification, and query the
    annotated image without re-dragging `desktop-context capture region`.
  - In `ocr-screenshot.nix`, add a vision fallback/action when Tesseract returns
    empty or garbled text (handwriting, LaTeX math, tables, code screenshots):
    send `-i "$TMP_IMG"` to `hey-computer ask --profile fast --new --print` for
    clean Markdown/LaTeX/plain extraction.

- `[Config]` **Multi-select Clipboard → Ask Computer in `rofi-cliphist.nix` (`SUPER+SHIFT+v`).**
  - `rofi-cliphist` already uses `-multi-select` and decodes both text and
    binary images.
  - Add a custom keybind (`Alt+a`, matching `rofi-hey-computer-history.nix`'s
    `kb-custom-*` pattern) that decodes the selected entry or entries into temp
    files/images and opens `rofi-ask` with them attached via `-f` / `-i`.
  - Example: select an error log from 3 copies ago + a screenshot from 5 copies
    ago → `Alt+a` → *"compare these two and tell me what broke."*

- `[Config]` **MR Review & Pipeline Diagnosis in `rofi-gitlab-search.nix` (`SUPER+SHIFT+g`).**
  - Selecting an MR currently offers `Open in GitLab`, `Copy URL`, and `Copy
    Branch`.
  - Add background `deep`-profile actions leveraging the already-wired `gitlab`
    + `linear` MCPs:
    - **Summarize & Review MR:** runs `hey-computer ask --background --profile
      deep` to summarize the diff, risks, linked Linear ticket context, and
      unresolved threads into `hey-computer-viewer`.
    - **Diagnose Failed Pipeline:** pulls failed CI job logs via GitLab MCP and
      summarizes root cause.

- `[Config]` **Semantic Search & "Ask about Note" in `rofi-obsidian.nix` (`SUPER+SHIFT+o`).**
  - Add `Alt+a` on any selected note to open `rofi-ask` with
    `-f "$VAULT_PATH/$NOTE.md"` (*"extract action items"*, *"turn into a Linear
    ticket"*, *"find related notes"*).
  - Route custom unmatched input starting with `? ` to `hey-computer ask` to
    search/synthesize across the vault instead of attempting an invalid
    `obsidian://` URI.

- `[Config]` **Context-Aware LLM Mode in `rofi-translate.nix` (`SUPER+SHIFT+t`).**
  - Keep `translate-shell` (`trans -b`) for instant single-word lookups, and add
    smart prefixes (or automatic fallback for multi-sentence clipboard text)
    calling `hey-computer ask --profile fast --new --print`:
    - `polish:<text>` — idiomatic English/German rewrite
    - `slack:<text>` — concise coworker phrasing
    - `de!:<text>` — natural German keeping English technical terms (matching
      `hey-computer.nix` language rules)

- `[Config]` **Smart Ticket Extraction + Quick Brief in `open-linear-ticket.nix` (`SUPER+SHIFT+x`).**
  - Extract `[A-Z]{2,10}-[0-9]+` even when the clipboard contains a branch name
    or surrounding text, and add a `--action="brief=Brief in Computer"` toast
    button to fetch the Linear ticket + linked GitLab MRs + Slack mentions into
    a background brief.

- `[Config]` **Quick `?` Prefix in `rofi-search.nix` (`SUPER+d`).**
  - Typing `? <question>` in the main `combi` launcher routes to `hey-computer
    ask --source typed` instead of spawning a Google Search browser window.

---

## 2. Untapped Scripts & Tools to Expose *to* `hey-computer`

- `[Config]` **Wallpaper + Palette ↔ Philips Hue Synergy (`rofi-wallpaper.nix`, `custom-wallpaper-theme.nix`, `hue-lights.nix`).**
  - Extract a non-interactive `set-wallpaper <name|random>` script from
    `rofi-wallpaper.nix` and expose it in `desktopScripts`.
  - Since `custom-wallpaper-theme` writes `~/.theme/palette.json` (`primary_hex`,
    `secondary_hex`) for all 45+ curated wallpapers (re-theming Waybar, Rofi,
    SwayNC, Tmux, and Obsidian), `hey-computer` can handle *"give me a cozy
    sunset / rainy Hamburg / calm focus vibe"*: set a matching wallpaper, read
    `~/.theme/palette.json`, and sync `Marius' Room` `Corner`/`Ceiling` Hue
    lights to complement the new desktop palette.

- `[Config]` **Full Spotify Control (`spotify-player.nix`).**
  - Currently `hey-computer` only exposes `spotify-like`,
    `random-album-of-the-day`, and `playerctl`.
  - Document `spotify_player` CLI subcommands (`search`, `playback start`,
    `playlist`, `get key playback`, `connect`) in `desktop-scripts/SKILL.md` so
    voice/typed prompts can play specific albums/playlists, queue tracks, or
    start instrumental focus music.

- `[Config]` **Phone Bridge via KDE Connect (`kdeconnect.nix`).**
  - Expose `kdeconnect-cli` in `extraPackages` + `desktop-scripts/SKILL.md`:
    - *"Send those train times / this link / that answer to my phone"*
      (`--share-text` / `--share`)
    - *"Ring my phone"* (`--ring`) or inspect phone battery before leaving.
    - Natural companion to the `transit` skill right before walking out the
      door.

- `[Config]` **Newsboat RSS Digest (`newsboat.nix`).**
  - `newsboat` runs with `autoReload = true` and maintains an SQLite database at
    `~/.local/share/newsboat/cache.db`.
  - Add a read-only `newsboat-unread` helper so `hey-computer` can answer
    *"anything worth reading in my RSS feeds today?"*, feed an optional weekend
    reading digest, or chain YouTube RSS items straight into the `yt-dlp` skill.

- `[Config]` **Homelab & Fleet Read-Only Observability (`hosts/nixner`, `hosts/kubenix`, `kubectl`).**
  - Add a strictly read-only `homelab` skill (`kubectl get/describe/logs`, `ssh
    nixner 'comin status'`, binary cache / Minecraft / GitLab runner status).
  - Pair with a **0-token scheduled health pre-collector** that stays silent
    (`HEARTBEAT_OK`) unless a pod is crashlooping, `comin` failed a deploy, or a
    `nixner` unit is degraded.

- `[Tool]` `[Config]` **Soccer Matchday Awareness (`golazo`).**
  - If `golazo` exposes a plain/JSON CLI subcommand, a lightweight pre-collector
    can surface FC St. Pauli / Bundesliga kickoff times in weekend Morning
    Briefs and answer matchday questions without web search.

- `[Config]` **Decouple `hey-computer.enable` from `host.hasNpuServer` (`home-manager/home.nix`).**
  - Currently `hey-computer.enable = host.isDev && host.hasNpuServer &&
    host.windowManager == "hyprland"`, restricting it to `worknix`.
  - Typed `rofi-ask`, `history`, `schedules`, `waybar-hey-computer`, CLI `ask`,
    and proactive briefs do not require local NPU dictation. Decoupling the
    daemon from `hasNpuServer` (while keeping `npu-dictate` bindings gated on
    `hasNpuServer`) brings the harness to `desknix` and `nixpad` too.

---

## 3. Event-Driven & Proactive Desktop Automations

- `[Config]` **Auto-Upgrade Changelog Brief & Build-Failure Triage (`auto-upgrade.nix`, `firmware-update-check.nix`).**
  - **On successful `nix build`:** write `nix store diff-closures
    /nix/var/nix/profiles/system "$SYSTEM_PATH"` to a temp file and add
    `--action="diff=Summarize Changes"` on the `"System Update Ready"` toast so
    `hey-computer` summarizes meaningful bumps (kernel, Hyprland, Neovim, own
    flakes) before switching.
  - **On failed `nix build`:** capture the error tail and submit `hey-computer
    ask --background --profile deep -f "$LOG"` (read-only diagnosis, respecting
    the `NixConfig` off-limits rule) so a root-cause analysis (e.g., which
    upstream bump broke a patch in `patches/`) is waiting in the viewer.
  - **On firmware updates:** add `--action="notes=Inspect Risks"` to summarize
    LVFS release notes and flag whether BIOS/dbx PCR 0/7 TPM re-enrollment is
    actually required.

- `[Config]` **Webcam-Aware Meeting Mode (`webcam-privacy.nix` + `hey-computer-proactive location` + `hue-lights`).**
  - `webcam-privacy.nix` already monitors `/dev/video*` via `inotifywait`.
  - On camera open: immediately run `waybar-hey-computer quiet-speech` and pause
    `hey-computer speak` so TTS never talks over a call; optionally enable
    SwayNC DND; if `location == "home"`, bump `Marius' Room` `Desk` lamp to a
    flattering call brightness and restore state on camera close.

- `[Config]` **Zero-Token Pre-Meeting Dossier (reusing `waybar-gcal.nix`'s cache).**
  - `waybar-gcal` already writes `$XDG_RUNTIME_DIR/waybar-gcal/events.tsv`.
  - A 0-token pre-collector checks that cached TSV every 5 minutes (zero API
    calls): when a non-routine event is 5–10 minutes away (skipping the 10:00
    Daily covered by `pre-daily`), wake `hey-computer` to gather relevant
    Gmail/Slack/Linear/Obsidian context on the topic or attendees.

- `[Config]` **Commute Disruption Watch (`transit` + `hey-computer-proactive location`).**
  - On `workday` when `location == "work"` (Samsung office monitor detected)
    between 16:45–19:00, a 0-token pre-collector checks `transit departures` for
    the homeward HVV line. Silent on normal service; wakes `hey-computer` only
    on `CANCELLED`, `SEV`, or `>= +10m` delays to notify with an alternate route
    (and optionally share it to the phone via KDE Connect).

- `[Config]` **End-of-Day Journal & Task Wrap-Up.**
  - Companion to the Morning Brief in `Journal/YYYY-MM-DD.md`: around ~17:45 on
    workdays (or via `"Hey Computer, wrap up my day"`), synthesize
    `hey-computer recall --on today`, today's Linear/GitLab activity, and local
    git commits into an idempotent `## Evening Wrap-Up` section and review
    carried-over `- [ ]` tasks.

- `[Config]` **Orchestrated "Desktop Scenes" Skill.**
  - Document multi-command rituals in `desktop-scripts/SKILL.md` (or a small
    `desktop-scene` helper) combining already-exposed scripts:
    - **Deep work:** `toggle-zen` (Waybar off + DND) + Buds ANC + `caffeinate` +
      warm `Desk` lamp (if home) + instrumental Spotify.
    - **Leaving office:** `netbird-vpn down` + `powermode light` + `transit`
      route home (+ KDE Connect share).
    - **Wind-down:** `hue-lights` `Relax` + warm `nightlight` + `darkman set
      dark` + pause work notifications.

- `[Config]` **Complete the SwayNC Reactive Drafts Loop (`hey-computer.nix`, `swaync/default.nix`, `rofi-hey-computer-history.nix`).**
  - Note: `services.configured.swaync.holdForHeyComputer` defaults to `true`
    when `hey-computer.enable` is true, while
    `programs.configured.hey-computer.notifications.enable` currently defaults
    to `false` (not yet turned on on `worknix`).
  - Once enabled:
    - Surface pending `hey-computer draft list --json` entries as a pinned
      section in `rofi-hey-computer-history.nix` (next to `active_tasks`, with
      `Enter` → `draft edit`, `Alt+c` → `draft copy`, `Alt+x` → `draft
      dismiss`).
    - Show pending draft counts in `waybar-hey-computer`'s tooltip/icon.
    - Consider adding `slack` with `work_only = true` (already gated by
      `workdayFile`).

---

## 4. Terminal, Editor, Waybar & Lockscreen Integrations

- `[Config]` **Zsh Helpers & Inline Command Generator (`zsh/init.nix`, superseding `atai`).**
  - `??` / `ask` shell function: detects piped stdin (`journalctl -u foo -n 100
    | ?? "why did this fail?"`), writes stdin to a temp file, and invokes
    `hey-computer ask -f <tmp> --print` (or viewer).
  - `wtf` function: grabs the last command (`fc -ln -1`), exit status, and last
    ~60 lines of `tmux capture-pane` and sends them to `hey-computer ask`.
  - ZLE widget (e.g. `Alt+a` in `viins`): converts a natural-language prompt in
    the command buffer into a runnable zsh command via `hey-computer ask
    --profile fast --new --print`, aware of the local NixOS/zsh/lsd/kubectl
    environment.

- `[Config]` **Tmux Pane Snapshot → `hey-computer` (`tmux.nix`).**
  - Bind `prefix + A` to dump `tmux capture-pane -p -S -150` to a temp file and
    open `rofi-ask` with `HEY_COMPUTER_ATTACH_FILE`. Works seamlessly inside SSH
    sessions (`nixner`, `kubenix`) and TUIs (`k9s`, `lazygit`, `htop`).

- `[Config]` **Yazi File Ask & Markdown Preview (`yazi/default.nix`).**
  - Bind `A` in Yazi to pass hovered/selected files (`-i` for images, `-f` for
    text/code/logs) to `rofi-ask`.
  - Add `hey-computer view %s1` as an opener for `*.md` files so any Markdown
    document can be previewed in `hey-computer-viewer`'s Reforma-1969 editorial
    layout.

- `[Config]` **Neovim Integration (`neovim/nvim/lua/plugins/`).**
  - Visual-mode `<leader>ha`: sends selection + file path/filetype + active LSP
    diagnostics to `hey-computer ask` for fast side-questions without starting a
    full coding agent session.
  - Telescope picker over `hey-computer recall --json --plain` to search past
    answers and yank/insert snippets into Obsidian notes (especially inside the
    `F11` `ghostty.obsidian` scratchpad).

- `[Config]` **Waybar Secondary-Click AI Actions (`waybar/default.nix`).**
  - `custom/github` right-click → `hey-computer ask --background "Triage my
    unread GitHub notifications"`
  - `custom/gcal` middle-click → `hey-computer ask "Brief me on my next calendar
    event and any related emails/notes/tickets"`
  - `custom/weather` middle-click → `hey-computer ask "Do I need an umbrella or
    warmer jacket today based on hourly rain/wind?"`

- `[Config]` **Privacy-Safe Lockscreen Counter (`hyprlock.nix`).**
  - Display a subtle non-sensitive status line from `hey-computer status --json`
    (e.g., `󱚣 1 task running` or `󰖨 Morning Brief ready`) below the weather
    label—never exposing private content on a locked screen.

---

## 5. Housekeeping Notes Spotted During Review

- `[Config]` **Prompt duplication:** `morningDigestPromptFile` and
  `preDailyPromptFile` in `home-manager/scripts/hey-computer-proactive.nix`
  duplicate the exact prompt strings in `home-manager/modules/hey-computer.nix`.
  Worth extracting to a single shared source before adding more proactive jobs.
- `[Config]` **Downstream host (`yannix`) boundary:** Per `NixConfig/CLAUDE.md`,
  all new dev/work/personal integrations (keybinds, Waybar clicks, Rofi actions)
  should stay gated behind `hasHeyComputer` or `host.isDev` so downstream hosts
  remain unaffected.

---

## Suggested Implementation Order (Highest Bang-for-Buck)

1. Extend `rofi-ask.nix` with `-f` / `-i` attachment env vars (unblocks
   screenshots, OCR, cliphist, Obsidian, Tmux, and Yazi in one change).
2. Replace `rofi-calculator.nix`'s ChatGPT browser fallback with `hey-computer`.
3. Add `Summarize & Review MR` / `Diagnose Failed Pipeline` to
   `rofi-gitlab-search.nix`.
4. Wire `auto-upgrade.nix` closure diffs and build-failure logs into
   `hey-computer`.
5. Expose `set-wallpaper`, `kdeconnect-cli`, `spotify_player`, and Desktop
   Scenes in `desktopScriptsSkill`.
