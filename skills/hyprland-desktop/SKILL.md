---
name: hyprland-desktop
description: Queries and drives the user's Hyprland desktop (0.55 or newer, Lua configuration) with hyprctl - windows, workspaces, monitors, starting applications - and covers notifications, media players and volume. Use when a request is about windows, workspaces, opening or closing an application, switching or moving things on screen, playing or pausing music, or changing the volume.
---

# Hyprland desktop

The desktop is Hyprland with a Lua configuration. Unless marked otherwise,
everything below was run against Hyprland 0.55.4.

## Dispatching: Lua expressions, not the old words

`hyprctl dispatch EXPR` runs `hl.dispatch(EXPR)`, so `EXPR` is a Lua
dispatcher expression. The old form `hyprctl dispatch workspace 3` is a Lua
syntax error now. Put the expression in single quotes for the shell and use
double quotes for Lua strings inside it.

```bash
hyprctl dispatch 'hl.dsp.focus({ workspace = 3 })'                   # go to workspace 3
hyprctl dispatch 'hl.dsp.focus({ window = "class:firefox" })'        # focus a window
hyprctl dispatch 'hl.dsp.window.move({ workspace = 2, follow = false })'  # move the active window, stay here
hyprctl dispatch 'hl.dsp.window.close({ window = "address:0x55d0c0ffee00" })'
hyprctl dispatch 'hl.dsp.workspace.toggle_special("name")'           # show or hide a special workspace
hyprctl dispatch 'hl.dsp.window.fullscreen()'                        # fullscreen the active window (not observed)
```

Window selectors that work: `class:NAME` (a regular expression such as
`class:^(firefox)$` works too; the first matching window is used) and
`address:0x…` from `hyprctl clients -j`.

**hyprctl exits 0 even when the dispatch failed.** Read its output:

- `ok`: the dispatcher ran. It does **not** prove that the window you meant
  was found or closed: query again to see what changed.
- `error: …`: a Lua error, for example a mistyped dispatcher name or the old
  syntax.
- `warning: … window not found`: the selector matched nothing.

After a change that matters, check the result with a query.

## Queries

All of these are read-only; `-j` gives JSON (use `jq`).

```bash
hyprctl clients -j        # every window: .class .title .address .workspace.name .floating
hyprctl activewindow -j   # the focused window ({} when none)
hyprctl activeworkspace -j
hyprctl workspaces -j     # .name .windows
hyprctl monitors -j       # .name .activeWorkspace.name .specialWorkspace.name .focused
hyprctl version
```

Example: `hyprctl clients -j | jq -r '.[] | "\(.class)\t\(.workspace.name)\t\(.title)"'`

## When hyprctl cannot reach Hyprland

The assistant may have been started before the current Hyprland session, so
`HYPRLAND_INSTANCE_SIGNATURE` in its environment can be stale or missing:

- stale: `Couldn't connect to …/hypr/<signature>/.socket.sock. (4)`, exit 4;
- missing: `HYPRLAND_INSTANCE_SIGNATURE not set! (is hyprland running?)`, exit 1.

`hyprctl instances -j` ignores the variable and lists the running instances.
Pass the live one explicitly, or export it for the rest of the command:

```bash
hyprctl --instance "$(hyprctl instances -j | jq -r '.[0].instance')" clients -j
hyprctl -i 0 activeworkspace          # the first running instance, by index
export HYPRLAND_INSTANCE_SIGNATURE="$(hyprctl instances -j | jq -r '.[0].instance')"
```

## Starting applications

Start desktop applications **through Hyprland**, never as a child of your own
shell: a child belongs to the assistant's service and would be stopped with
it. `hl.dsp.exec_cmd` hands the command line to Hyprland, which starts it as
its own child:

```bash
hyprctl dispatch 'hl.dsp.exec_cmd("firefox https://example.org")'
hyprctl dispatch 'hl.dsp.exec_cmd("foot", { workspace = "4 silent" })'   # on workspace 4, without going there
hyprctl dispatch 'hl.dsp.exec_cmd("[workspace 5 silent] foot")'          # the same, as a rule prefix
```

The new window opens on the active workspace unless a rule says otherwise.
The command line runs in Hyprland's environment, so the program must be on
Hyprland's PATH. To open a web address in the user's default browser, the
usual way (not run here) is
`hyprctl dispatch 'hl.dsp.exec_cmd("xdg-open https://example.org")'`.

## Notifications

```bash
notify-send "Title" "Body"          # body may use Pango markup: <b>, <i>, &amp;
```

## Media players, volume, and brightness

The following comes from the tools' own `--help` and desktop verification. Prefer `pamixer` over `wpctl` when `pamixer` is on `PATH` so host volume limits and mute indicators match the desktop's media keys:

```bash
playerctl -l                        # players that can be controlled
playerctl status                    # Playing / Paused / Stopped
playerctl play-pause                # also: play, pause, next, previous, stop
playerctl metadata artist title
playerctl -p spotify play-pause     # one player by name
```

```bash
# Prefer pamixer when installed on PATH:
pamixer --get-volume-human          # e.g. "45%" or "muted"
pamixer -i 5                        # louder by 5 %
pamixer -d 5                        # quieter by 5 %
pamixer --set-volume 30             # exactly 30 %
pamixer -t                          # toggle speaker/headphone mute
pamixer --default-source -t         # toggle microphone mute

# Fallback when pamixer is not installed:
wpctl get-volume @DEFAULT_AUDIO_SINK@          # "Volume: 0.45" (1.0 is 100 %)
wpctl set-volume -l 1.0 @DEFAULT_AUDIO_SINK@ 5%+   # louder by 5 %, never above 100 %
wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%-
wpctl set-volume @DEFAULT_AUDIO_SINK@ 0.3          # exactly 30 %
wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle         # or 1 / 0
```

```bash
# Screen brightness (when brightnessctl is installed on PATH):
brightnessctl -m                    # machine-readable current brightness (%)
brightnessctl set 10%+              # brighter by 10 %
brightnessctl set 10%-              # dimmer by 10 %
brightnessctl set 50%               # set to 50 %
```
