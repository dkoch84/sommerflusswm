# sommerflusswm

A manual, frame-tree tiling **window manager** for [river](https://codeberg.org/river/river)
0.4+ — a poor-man's herbstluftwm successor on Wayland. I hope this reads as a love letter and not a shameless copy. 

Two binaries, mirroring herbstluftwm:

| sommerflusswm | herbstluftwm | role |
|---|---|---|
| **`sfwm`** | `herbstluftwm` | the window manager (a `river-window-management-v1` client) |
| **`sc`**   | `herbstclient` | the control client; config is a bash script calling `sc` |

## Status 

Basic tiling, splitting, movement via keybinds or CLI commands. 


## Build

```sh
cargo build --release        # builds both sfwm and sc
cargo test -p sfwm           # runs the monitor-model unit tests
```

`wayland-client` dlopens `libwayland-client` at runtime, so it must be present to
run (but is not a link-time dependency).

## Run

river advertises `river_window_manager_v1` only to its designated window manager,
so `sfwm` must be launched *by* river.

1. Install river's init (execs `sfwm`):
   ```sh
   install -m755 sfwm/examples/river-init ~/.config/river/init
   ```
2. Install the sommerflusswm autostart (the bash config `sfwm` runs; calls `sc`):
   ```sh
   install -Dm755 sfwm/examples/autostart ~/.config/sommerflusswm/autostart
   ```
3. Put `sfwm` and `sc` on `PATH`, then start river from a TTY:
   ```sh
   river
   ```

Because river isolates the WM in a separate process, an `sfwm` crash does not kill
the session — you can iterate and even hot-swap window managers live.

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
  src/monitor.rs       the Monitor model + overlapping-monitor logic (+ unit tests)
  src/ipc.rs           the sc command dispatcher
  src/protocol.rs      river-window-management-v1 bindings (from the vendored XML)
  protocols/           vendored protocol XML (re-vendor when bumping river)
  examples/            river-init and the sommerflusswm autostart
sc/                    the control client (herbstclient successor)
```