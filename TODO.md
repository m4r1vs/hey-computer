# `hey-computer` TODOs (from Obsidian)

> **Source:** `Journal/2026-11-26.md` (plus linked note `Hey Computer Notification Prompt Bug.md`) in Obsidian (`Marius' Remote Vault`)
> **Exported:** 2026-11-26
> **Tags:** `[Upstream]` = `hey-computer` repo, `[Config]` = `~/NixConfig` / desktop setup, `[Research]` = exploration / spike

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

---

## 2. Viewer, Shortcuts & Desktop UX

- [ ] `[Upstream]` `[Config]` **Quick-reopen shortcut & persistent viewer window (`Ctrl+Super+A`).**
  - Add a `Ctrl+Super+A` shortcut to open the last opened thread at its exact scroll/reading position.
  - Consider making `Cmd+Q` / `Esc` hide the GTK app instead of closing it.
  - Evaluate placing the `hey-computer` frontend in a dedicated Hyprland special workspace.
- [ ] `[Upstream]` **Share / export answers in multiple formats.**
  - Add ability to share/export answers as Markdown (and other formats), including via a keyboard shortcut.
- [ ] `[Config]` **Disable `hey-computer` in super-light performance mode.**
  - Pause/stop `hey-computer` when switching into super-light power/performance mode.

---

## 3. New Features & Workflows

- [ ] `[Upstream]` `[Config]` **`plan-my-day` workflow.**
  - Especially for workdays: build a *"roter Faden"* for the day with concrete tasks and sub-TODOs, periodically reassess progress throughout the day, and optionally block time in Google Calendar for specific tasks.
- [ ] `[Upstream]` **WhatsApp message-to-self replies.**
  - Allow the agent to respond to Marius's own messages via WhatsApp (evaluate whitelisting self-chat so outbound write is enabled strictly for self-messages).

---

## 4. Integrations & Tooling

- [ ] `[Config]` `[Upstream]` **`spotify_player` integration & Spotify info shortcut.**
  - Add `spotify_player` support/integration to `hey-computer` so the agent can search for tracks/albums and play them *(merged duplicate Obsidian items)*.
  - Add a Spotify info keyboard shortcut powered by `hey-computer`.
  - [ ] *Related prerequisite:* Update the `spotify_player` fork.
- [ ] `[Config]` **Integrate `hey-computer` into existing desktop scripts.**
  - Example: pull *Album of the Day* into the Morning Brief with follow-up actions / rotation *(see also `NIXCONFIG_INTEGRATIONS_DRAFT.md`)*.
- [ ] `[Upstream]` `[Config]` **Add `defuddle` to `hey-computer`.**
  - Integrate `defuddle` for cleaner web page extraction.
- [ ] `[Upstream]` `[Config]` **Bidirectional `claude-code` / `agy` integration.**
  - **Coding agent → `hey-computer`:** Let `claude-code` / `agy` delegate to `hey-computer` (saving TODOs, reading Obsidian, etc.) without polluting the coding agent's context; consider moving Slack out of there too.
  - **`hey-computer` → coding agent:** Let `hey-computer`'s agent start and orchestrate `claude-code` / `agy` sessions in `~/Projects` and `~/NixConfig`.

---

## 5. Architecture, Packaging & Research

- [ ] `[Upstream]` `[Config]` **Consolidate Nix definitions into `hey-computer`.**
  - Move `hey-computer`-related Nix definitions currently living in `~/NixConfig` into the `hey-computer` repository where it makes sense.
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

## Recently Completed (already checked `[x]` in Obsidian)

- [x] Add `hey-computer` to the Whisper glossary.
- [x] Add `playerctl` support to `hey-computer`.
