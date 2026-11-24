---
name: openhue
description: Controls Philips Hue lights, rooms, and scenes through the openhue CLI (turning lights or rooms on/off, setting brightness, colors, color temperature, activating scenes, and querying current lighting state). Use when a request is about lights, lamps, room lighting, brightness, or lighting scenes.
---

# Philips Hue (`openhue`)

Control Philips Hue rooms, lights, and scenes with `openhue`.

## Inspecting state

Prefer the plain-text tables — they are compact and easy to scan. Avoid dumping raw `openhue get rooms --json` without filtering through `jq`, because the raw JSON carries hundreds of kilobytes of nested device metadata.

```bash
openhue get rooms                   # rooms with status ([on] / [  ]) and brightness
openhue get lights                  # every light with status, brightness, and room
openhue get scenes                  # every scene with room and active status
```

## Controlling rooms, lights, and scenes

```bash
# Rooms
openhue set room "<Room>" --on
openhue set room "<Room>" --off
openhue set room "<Room>" --on --brightness 60
openhue set room "<Room>" --on --color warm_white
openhue set room "<Room>" --on --rgb '#FFAA44'
openhue set room "<Room>" --on -t 350                      # color temperature in Mirek
openhue set room "<Room>" --on --brightness 40 --transition-time 5s

# Individual lights (always pass --room when targeting by name)
openhue set light --room "<Room>" "<Light>" --on
openhue set light --room "<Room>" "<Light>" --off
openhue set light --room "<Room>" "<Light>" --on --brightness 50
openhue set light --room "<Room>" "<Light>" --on --color blue
openhue set light --room "<Room>" "<Light>" --on -t 250

# Scenes (always pass --room when targeting by name)
openhue set scene --room "<Room>" "<Scene>"
openhue set scene --room "<Room>" "<Scene>" -a dynamic     # dynamic palette
```

## Important rules

- **Always include `--on`** whenever setting `--brightness`, `--color`, `--rgb`, or `-t` — `openhue` ignores those flags unless `--on` is also passed.
- **Always disambiguate with `--room "<Room>"`** when addressing a light or scene by name, because multiple rooms commonly share names such as `Ceiling` or `Nightlight`.
- **Color temperature (`-t`) is in Mirek (`153`–`500`), not Kelvin:**
  - `153` ≈ 6500 K (cool daylight)
  - `200` ≈ 5000 K (energizing cool white)
  - `250` ≈ 4000 K (neutral white)
  - `350` ≈ 2850 K (warm white)
  - `450`–`500` ≈ 2000–2200 K (cozy / candlelight)
- **Bulb capabilities vary:** Dimmable white bulbs only support on/off and brightness (not `--color`, `--rgb`, or `-t`). Do not claim a color change succeeded if `openhue` reports an error.
- **Relative brightness adjustments** ("dim by 20%", "a bit brighter"): if no local helper exists, read the current percentage from `openhue get rooms` or `openhue get lights`, compute the new target clamped to `1..100` (or `--off` at `0`), and apply with `--on --brightness <N>`.
- **All-day / time-based smart scenes:** `openhue` CLI does not natively control Hue CLIP v2 `smart_scene` schedules. If the user's configuration provides a local lighting helper or home-specific skill, prefer it for all-day scenes and multi-room shortcuts.
- **Keep credentials private:** Never print or quote the Hue Bridge API key from `~/.openhue/config.yaml` or `$XDG_CONFIG_HOME/openhue/config.yaml`.
