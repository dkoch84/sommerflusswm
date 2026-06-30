# Wayland-specific notes (the "K" items)

herbstluftwm targets X11; sfwm targets river/Wayland. A few capabilities are
either different in kind or live in *separate* Wayland protocols rather than in
`river-window-management-v1`. This file tracks their status and design.

## Output hotplug — DONE

The monitor topology is auto-derived from river outputs only while no explicit
`set_monitors` has run (`State::auto_monitors`). In that mode, plugging or
unplugging an output re-derives one base monitor per output. An `autostart` that
calls `set_monitors` (or `detect_monitors`) takes ownership of the topology and
is never clobbered by hotplug.

## Dimming inactive windows (in-WM, to replace picom) — DESIGNED

herbstluftwm users often run picom to dim unfocused windows. river exposes enough
to do this *inside* sfwm, no external compositor effect needed:

- `river_window_v1.get_decoration_above(surface)` attaches a WM-owned
  `river_decoration_v1` surface that renders **above** the window content and
  borders (per the protocol's documented z-order). A semi-transparent black fill
  over an unfocused window is exactly a dim.
- Fill source: one shared `wp_single_pixel_buffer_v1` 1×1 buffer holding a
  pre-multiplied black at the configured alpha (single-pixel buffers are
  immutable and shareable across every overlay). Scale it to each window with
  `wp_viewporter` (`set_destination(w, h)`).
- Lifecycle, per render pass (decoration `set_offset`/`sync_next_commit` are
  *render* state, so this fits inside `do_render` before `render_finish`):
  - unfocused + dim enabled → ensure the window has a decoration surface, attach
    the shared buffer, viewport `set_destination` to the content rect,
    `set_offset(0,0)`, `sync_next_commit`, `wl_surface.commit`.
  - focused, or dim disabled → attach a null buffer + commit (draws nothing), or
    destroy the decoration.
  - track last (shown, size) per window so we only re-commit on change.
- New globals to bind: `wl_compositor` (core), `wp_viewporter`,
  `wp_single_pixel_buffer_manager_v1`. Config surface: `set inactive_dim <0..1>`
  (0 = off).

Status: not yet implemented. It is pixel/buffer code that can't be validated
headlessly, so it wants a live river session to verify (and a wrong buffer-commit
ordering is a protocol error that drops the WM connection). Best done as its own
focused, VM-tested change rather than blind.

## HiDPI / fractional scale — TODO

river reports outputs and window geometry in logical coordinates, which is what
the monitor model and frame tree already use, so layout is scale-correct today.
Per-output scale factor isn't surfaced to the WM for anything sfwm currently
needs; revisit if/when a feature needs physical pixels.

## Status bars (layer-shell) — BY DESIGN, external

A bar is a `wlr-layer-shell` client, not something the WM draws. sfwm provides
everything a bar needs and stays out of its way:
- `sc tag_status [monitor]` — panel-ready tag view (`#`/`%`/`!`/`:`/`.`).
- `sc --idle` — the hook stream (`tag_changed`, `focus_changed`,
  `window_title_changed`, plus user `emit_hook`) to drive redraws.
- `pad` reserves monitor edges for the bar.
This mirrors hlwm + a panel (lemonbar/polybar) rather than a built-in bar.

## xdg-activation / urgency — PARTIAL

`border_color_urgent` and a per-window `urgent` flag exist and are honoured
(cleared on focus, surfaced in `tag_status`/`attr`). What's missing is the
*event source*: marking a window urgent needs an activation/urgency signal that
`river-window-management-v1` does not currently deliver. Wire it up when the
protocol exposes it (or via `xdg-activation-v1` if surfaced).

## idle-inhibit / session lock — minimal

`session_locked` / `session_unlocked` are received and currently no-ops. Idle
inhibition is a separate protocol (`idle-inhibit-unstable-v1`) and not yet
handled.
