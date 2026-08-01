# sommerflusswm

A manual, frame-tree tiling **window manager** for [river](https://codeberg.org/river/river)
0.4+, a poor-man's herbstluftwm successor on Wayland. I hope this reads as a love
letter and not a shameless copy.

Two binaries, mirroring herbstluftwm:

| sommerflusswm | herbstluftwm | role |
|---|---|---|
| **`sfwm`** | `herbstluftwm` | the window manager (a `river-window-management-v1` client) |
| **`sc`**   | `herbstclient` | the control client; config is a bash script calling `sc` |

## Status

0.1.0. Usable, but early. Expect rough edges and missing hlwm commands.

**Layout and windows.** Per-tag binary frame tree with splits, resizing and
frame removal; leaf layouts max, vertical, horizontal, grid. Floating,
fullscreen and pseudotile windows, per-window rules, borders, gaps, dim
inactive, layout dump/load.

**Monitors.** hlwm-style virtual monitors, including overlapping overlay
monitors with `raise_monitor`, `lock_tag` and `pad`. Auto-derived from the real
outputs until an autostart takes ownership, so hotplug just works.

**Input.** Keybinds through `river-xkb-bindings-v1`, mousebinds (move, resize,
zoom), and 3/4-finger touchpad swipes read straight from libinput, since river
does not forward gestures to the WM. Gestures need the user in the `input`
group; without it they are quietly disabled.

**Scripting.** Hooks (`sc --idle`, `emit_hook`), the object/attribute tree, and
the hlwm combinators: `chain`, `and`, `or`, `try`, `silent`, `compare`,
`substitute`, `sprintf`.

**Chrome, drawn by the WM itself.** Status bar (one per monitor, with executor
modules, separators, spacers and an SNI system tray), wallpaper, freedesktop
notifications, a fuzzy app launcher, `sc menu` (dmenu-style: pick a stdin line,
print it) and `sc select_region` (drag or click a window, prints a `grim -g`
rect). All of it renders at output scale, so HiDPI stays crisp.

`sc help` prints the full command reference and needs no running WM.

### Why the WM draws its own bar

Under X11 a window manager only manages windows, and the panel, tray,
notification daemon and wallpaper setter are separate programs you pick to
taste. That choice is not available here: those programs use `wlr-layer-shell`
on Wayland, and layer-shell surfaces cannot composite under sfwm. So sfwm
provides them itself, or the session has none. It owns
`org.kde.StatusNotifierWatcher` and `org.freedesktop.Notifications` and paints
every surface with one small text-and-image engine.

A companion package, **sommerflusswm-appearance**, adds a GUI for wallpapers,
icon themes and cursor themes. It lives in this workspace but ships separately
so the WM does not pull in eframe and OpenGL.

## Build

```sh
cargo build --release -p sfwm -p sc   # the window manager and control client
cargo test -p sfwm                    # monitor, frame tree and IPC unit tests
```

`wayland-client` dlopens `libwayland-client` at runtime, so it must be present
to run but is not a link-time dependency. Runtime deps: `river`, `wayland`,
`libxkbcommon`, `libinput`. Optional: `grim`, `slurp`, `satty` and
`wl-clipboard` for screenshots, `xdg-desktop-portal-wlr` for screen sharing,
`brightnessctl` for laptop brightness keys.

Arch packaging lives in `packaging/`. `packaging/release [wm|appearance|all]
[--bump-rel]` builds both packages from the working tree with makepkg and
publishes them to a local pacman repo; set `REPO_DIR` in that script to yours.

## Run

river advertises `river_window_manager_v1` only to its designated window
manager, so `sfwm` must be launched *by* river.

1. Install river's init, which execs `sfwm` as its last line:
   ```sh
   install -Dm755 sfwm/examples/river-init ~/.config/river/init
   ```
2. Install the sommerflusswm autostart, the bash config `sfwm` runs:
   ```sh
   install -Dm755 sfwm/examples/autostart ~/.config/sommerflusswm/autostart
   ```
   With no user autostart, sfwm falls back to the packaged
   `/usr/share/sommerflusswm/autostart`, a working generic default.
3. Put `sfwm` and `sc` on `PATH` and start river from a TTY, or pick **sfwm** at
   a display manager (the package ships a `wayland-sessions` entry).

Because river isolates the WM in a separate process, an `sfwm` crash does not
kill the session. You can iterate and even hot-swap window managers live.

### IPC socket

`sfwm` listens on `$SOMMERFLUSSWM_SOCKET`, defaulting to
`$XDG_RUNTIME_DIR/sfwm-$WAYLAND_DISPLAY.sock`. `sc` resolves the same path. Try:

```sh
sc list_monitors
sc add_monitor 3840x2160+1440+0 8 float1 && sc raise_monitor float1 && sc lock_tag 8 float1
sc list_outputs
```

## Layout

```
sfwm/                  the window manager
  src/main.rs          connection, event loop (calloop), manage/render passes
  src/frame.rs         the per-tag frame tree (pure, unit-tested)
  src/monitor.rs       the Monitor model and overlapping-monitor logic
  src/ipc.rs           the sc command dispatcher
  src/attr.rs          the object/attribute tree
  src/tray.rs          StatusNotifierItem watcher and host
  src/notify.rs        the org.freedesktop.Notifications service
  src/launcher.rs      .desktop enumeration and fuzzy matching
  src/gestures.rs      libinput swipe recognition
  src/protocol.rs      river protocol bindings, from the vendored XML
  protocols/           vendored protocol XML (re-vendor when bumping river)
  examples/            river-init, autostart, bar modules, helper scripts,
                       session entry, portal config, personal machine configs
sc/                    the control client (herbstclient successor)
sfwm-appearance/       the appearance GUI (separate package)
packaging/             PKGBUILDs and the release script
docs/wayland-notes.md  design notes on the X11-to-Wayland gaps
```

## License

GPL-3.0.
