# Personal machine configs (not installed)

These are the author's real per-machine autostarts, kept as living examples of
fuller setups. The package installs only the generic `../autostart`.

- `autostart.hydra` — 4-monitor desktop: explicit `set_monitors` rects, two
  raised tag-locked overlay monitors (floating scratchpads over the 4K panels),
  a full tint2-style bar cluster + tray on every monitor, per-monitor pads.
  Physical output layout (positions/rotations/modes) lives in
  `~/.config/river/init` via wlr-randr.
- `autostart.barky` — HiDPI laptop (3840x2400 @ scale 2; the scale is set in
  `~/.config/river/init`, not here). Touchpad gestures, brightness keys,
  personal program spawns.
- `autostart.cubey` — single 3440x1440 desktop with an Apple Magic Trackpad
  (same gesture setup as a laptop).

Install one as `~/.config/sommerflusswm/autostart` and edit; sfwm falls back
to the packaged default when that file doesn't exist.
