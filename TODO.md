# `hey-computer` TODOs

> **Tags:** `[Upstream]` = `hey-computer` repo, `[Config]` = `~/NixConfig` / desktop setup, `[npu-dictate]` = dictation frontend, `[Research]` = exploration / spike, `[Obsidian]` = vault organization
> **UI naming:** Use **"Ask Computer"** (not "Ask Hey Computer") in user-facing prompts, launchers, keybind descriptions, and UI labels.
> **Trust policy:** No confirmation/permission prompts — trust the agent to act directly; add permission prompts later only if needed. (Previously disregarded: vault Git auto-backup, `permissions.ask` confirmation tier, `hey-computer log`, env-var filtering.)

---

## 1. Viewer, Shortcuts & Desktop UX

- [ ] `[Upstream]` `[Config]` **Quick-reopen shortcut & persistent viewer window (`Ctrl+Super+A`).**
  - Add a `Ctrl+Super+A` shortcut to open the last opened thread at its exact scroll/reading position.
  - Consider making `Cmd+Q` / `Esc` hide the GTK app instead of closing it.
  - Evaluate placing the `hey-computer` frontend in a dedicated Hyprland special workspace.
- [ ] `[Upstream]` **Share / export answers in multiple formats.**
  - Add ability to share/export answers as Markdown (and other formats), including via a keyboard shortcut.
- [ ] `[Config]` **Disable `hey-computer` in super-light performance mode.**
  - Pause/stop `hey-computer` when switching into super-light power/performance mode.

---

## 2. New Features & Workflows

- [ ] `[npu-dictate]` `[Config]` **Hotword detection & TTS barge-in.**
  - Always-on, local detection of `"Hey Computer"`, then recording until silence (VAD) and transcribing with the existing `npu-dictate` backend.
  - Evaluate small wake-word models such as `openWakeWord` (custom phrase training) running on CPU or NPU; likely belongs in `npu-dictate` rather than `hey-computer`, with no daemon dependencies.
  - Requirements: keep audio strictly local until the hotword fires; visible Waybar mic indicator; mute toggle; pause while another app holds the mic (calls) or while media is playing to avoid false triggers.
  - **TTS barge-in:** Interrupt active TTS playback immediately once speaking/wake-word fires via `hey-computer cancel` / `quiet-speech` (post-turn TTS cancellation and push-to-talk key-down silence are already wired; gate hotword barge-in via PipeWire `module-echo-cancel` / headphones or wake-word-only detection during TTS playback to avoid speaker self-triggers).
  - Measure the false-accept rate on real German and English speech before trusting it.
- [ ] `[Upstream]` **WhatsApp message-to-self replies.**
  - Allow the agent to respond to Marius's own messages via WhatsApp (evaluate whitelisting self-chat so outbound write is enabled strictly for self-messages).

---

## 3. Integrations & Tooling

- [ ] `[Config]` `[Upstream]` **`spotify_player` integration & Spotify info shortcut.**
  - Add `spotify_player` support/integration to `hey-computer` so the agent can search for tracks/albums and play them.
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

## 4. Architecture, Packaging & Research

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
