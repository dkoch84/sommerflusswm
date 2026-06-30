//! sfwm — sommerfluss window manager.
//!
//! A manual, virtual-monitor tiling window manager for river 0.4+, built as a
//! herbstluftwm successor. Implemented so far: the monitor model with overlapping
//! ("virtual") monitors (milestone 2), keyboard bindings via river-xkb-bindings-v1
//! (part of milestone 5), and the per-tag **frame tree** (milestone 3) — binary
//! split tree, leaves holding window stacks with max/vertical/horizontal/grid
//! layouts. All driven at runtime over an `sc` IPC socket (sommerfluss's
//! `herbstclient`).
//!
//! Two config layers, mirroring hlwm:
//!   1. river's own `init` — configures river and `exec`s this binary.
//!   2. sfwm's `autostart` — a shell script calling `sc` (set_monitors, add_monitor,
//!      raise_monitor, lock_tag, pad, …), a near-direct port of the hlwm autostart.

mod attr;
mod frame;
mod ipc;
mod monitor;
mod protocol;

use frame::{Frame, WinId};
use monitor::{Monitors, Rect, TagId};
use protocol::*;

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use calloop::generic::Generic;
use calloop::{EventLoop, Interest, Mode, PostAction};
use calloop_wayland_source::WaylandSource;

use wayland_client::{
    backend::ObjectId,
    event_created_child,
    globals::{registry_queue_init, GlobalListContents},
    protocol::wl_registry,
    Connection, Dispatch, Proxy, QueueHandle,
};

/// Geometry of a river logical output, in logical coordinates.
#[derive(Default, Clone, Copy)]
struct OutputGeo {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// A logical output as reported by river. Outputs anchor the coordinate space;
/// they are *not* the monitors (those are the WM-side `Monitor` abstraction).
struct OutputInfo {
    #[allow(dead_code)]
    output: RiverOutputV1,
    geo: OutputGeo,
    /// Numeric name of the corresponding `wl_output` global (from the
    /// `river_output_v1.wl_output` event). Resolving this to a human output name
    /// like "DP-1" requires binding the `wl_output` global and is deferred to a
    /// later milestone; the monitor model only needs geometry, which we have.
    #[allow(dead_code)]
    wl_output_name: Option<u32>,
}

/// A managed window. It lives as a [`WinId`] leaf entry in its tag's frame tree;
/// this struct holds the Wayland objects and metadata, keyed by `WinId` in
/// `State::windows`.
struct Window {
    win: RiverWindowV1,
    /// Cached render-list node (`river_node_v1`); created lazily in the render pass.
    node: Option<RiverNodeV1>,
    /// The tag (and thus frame tree) this window lives on.
    tag: TagId,
    app_id: Option<String>,
    title: Option<String>,
    /// Last content dimensions reported by river (for pseudotile/floating).
    dims: (i32, i32),
    /// Fullscreen on its monitor's output.
    fullscreen: bool,
    /// Whether `fullscreen` was applied last manage pass (to emit exit_fullscreen).
    applied_fullscreen: bool,
    /// Whether we've told the client to use server-side decoration (no titlebar).
    /// hlwm-style: the WM owns all decoration; clients draw none.
    ssd_applied: bool,
    /// Pseudotile: keep natural size, centered in the tile instead of filling it.
    pseudotile: bool,
    /// Floating: positioned freely at `float_geo`, above the tiled layout.
    floating: bool,
    /// Geometry used while floating (absolute logical coords).
    float_geo: Rect,
    /// Monotonic float stacking key: higher renders above other floats (hlwm
    /// `raise`/`lower`). Gives floating windows a deterministic order.
    raise_seq: u64,
    /// Whether window rules have been applied (once, when app_id/title first known).
    rules_applied: bool,
    /// Window requested attention (xdg-activation / urgency) and isn't focused.
    urgent: bool,
}

impl Window {
    fn new(win: RiverWindowV1, tag: TagId) -> Window {
        Window {
            win,
            node: None,
            tag,
            app_id: None,
            title: None,
            dims: (0, 0),
            fullscreen: false,
            applied_fullscreen: false,
            ssd_applied: false,
            pseudotile: false,
            floating: false,
            float_geo: Rect::new(0, 0, 0, 0),
            raise_seq: 0,
            rules_applied: false,
            urgent: false,
        }
    }
}

/// Which compositing layer a placement belongs to within a monitor (low→high).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Layer {
    Tiled,
    Floating,
    Fullscreen,
}

/// A window rule: match on app_id/title, apply consequences to new windows.
struct Rule {
    /// (exact?, pattern) matched against app_id / title respectively.
    app_id: Option<(bool, String)>,
    title: Option<(bool, String)>,
    tag: Option<TagId>,
    /// Place the window on whatever tag this monitor currently shows.
    monitor: Option<monitor::MonitorSel>,
    floating: Option<bool>,
    pseudotile: Option<bool>,
    /// Focus the window when it appears.
    focus: Option<bool>,
    /// Switch the focused monitor to the window's tag (follow it).
    switchtag: Option<bool>,
}

/// What applying the rules to a window decided (consumed by `reapply_rules`).
struct RuleOutcome {
    tag: TagId,
    focus: bool,
    switchtag: bool,
}

impl Rule {
    /// A one-line `key=value` rendering for `list_rules`.
    fn describe(&self) -> String {
        let mut p = Vec::new();
        if let Some((exact, v)) = &self.app_id {
            p.push(format!("app_id{}{v}", if *exact { "=" } else { "~" }));
        }
        if let Some((exact, v)) = &self.title {
            p.push(format!("title{}{v}", if *exact { "=" } else { "~" }));
        }
        if let Some(t) = self.tag {
            p.push(format!("tag={t}"));
        }
        if let Some(m) = &self.monitor {
            p.push(format!("monitor={m:?}"));
        }
        if let Some(f) = self.floating {
            p.push(format!("floating={}", if f { "on" } else { "off" }));
        }
        if let Some(ps) = self.pseudotile {
            p.push(format!("pseudotile={}", if ps { "on" } else { "off" }));
        }
        if self.focus == Some(true) {
            p.push("focus=on".into());
        }
        if self.switchtag == Some(true) {
            p.push("switchtag=on".into());
        }
        p.join(" ")
    }
}

/// An interactive pointer operation in progress (move or resize a floating window).
struct PointerOp {
    win: WinId,
    resize: bool,
    start_geo: Rect,
}

/// A registered pointer binding.
struct MouseBind {
    resize: bool,
    seat: RiverSeatV1,
    binding: RiverPointerBindingV1,
}

/// One window's computed placement for a manage/render pass.
struct RenderItem {
    /// Index into `monitors.list` of the monitor this window is shown on.
    mon: usize,
    win: WinId,
    rect: Rect,
    /// False for windows obscured within a `max` leaf — they get hidden.
    visible: bool,
    layer: Layer,
    /// Within-layer stacking key (float `raise_seq`; 0 for tiled).
    seq: u64,
}

/// A keyboard binding: the river binding object plus the `sc` command it runs.
struct KeyBind {
    /// The original spec ("Mod4+Return") for `list_keybinds`.
    spec: String,
    binding: RiverXkbBindingV1,
    command: Vec<String>,
}

/// All window-manager state. This is the calloop loop data, mutated by both the
/// Wayland dispatch (manage/render passes) and the IPC socket — single-threaded,
/// so no locking is required.
struct State {
    wm: Option<RiverWindowManagerV1>,
    /// A queue handle stored so the IPC/keypress paths can create protocol
    /// objects (nodes, key bindings) without one being threaded through.
    qh: QueueHandle<State>,
    outputs: HashMap<ObjectId, OutputInfo>,
    /// Window registry, keyed by `WinId`. The frame trees reference these ids.
    windows: HashMap<WinId, Window>,
    /// Map from a river window object to its `WinId` (for event lookup).
    win_by_obj: HashMap<ObjectId, WinId>,
    /// Next `WinId` to hand out.
    next_win: WinId,
    /// One frame tree per tag, created lazily.
    tags: HashMap<TagId, Frame>,
    /// Gap in logical px between tiled windows/frames (hlwm `window_gap`).
    window_gap: i32,
    /// The virtual-monitor topology (set_monitors / add_monitor / overlays).
    monitors: Monitors,
    /// Seats carry keyboard focus; we focus the focused monitor's shown window.
    seats: Vec<RiverSeatV1>,
    /// The xkb key-binding global (river_xkb_bindings_v1), if river advertises it.
    xkb_bindings: Option<RiverXkbBindingsV1>,
    /// Active key bindings, keyed by the binding object's id.
    keybinds: HashMap<ObjectId, KeyBind>,
    /// Bindings created but not yet `enable()`d — enabling is window-management
    /// state and must happen inside a manage sequence (see do_manage).
    pending_enable: Vec<RiverXkbBindingV1>,

    // --- theming ---
    border_width: i32,
    border_active: (u8, u8, u8, u8),
    border_normal: (u8, u8, u8, u8),
    border_urgent: (u8, u8, u8, u8),

    // --- behaviour settings ---
    /// hlwm `focus_follows_mouse`: pointer entering a window focuses it.
    focus_follows_mouse: bool,
    /// hlwm `raise_on_focus`: focusing a floating window raises it to the top.
    raise_on_focus: bool,
    /// hlwm `smart_frame_surroundings`: drop the gap around a lone frame.
    smart_frame_surroundings: bool,
    /// hlwm `smart_window_surroundings`: drop the border for a lone tiled window.
    smart_window_surroundings: bool,
    /// Layout a freshly-created (empty) tag tree adopts (`default_frame_layout`).
    default_frame_layout: frame::LayoutMode,
    /// Next float stacking key to hand out (see `Window::raise_seq`).
    next_raise: u64,

    // --- rules & tag affinity ---
    rules: Vec<Rule>,
    /// hlwm `my_monitor`: a tag's home monitor, focused first by `use`.
    tag_monitor: HashMap<TagId, monitor::MonitorSel>,
    /// Previously-displayed tag per monitor index (hlwm `use_previous`).
    prev_tag: HashMap<usize, TagId>,

    // --- pointer bindings & interactive ops ---
    /// Pointer bindings keyed by binding id.
    pointer_binds: HashMap<ObjectId, MouseBind>,
    pending_pointer_enable: Vec<RiverPointerBindingV1>,
    /// The window currently under the pointer (for click-to-focus and op start).
    pointer_focus: Option<WinId>,
    pointer_pos: (i32, i32),
    op: Option<PointerOp>,
    /// The rect each window was last given (so an interactive op knows where to
    /// start from, and float-toggle can keep a window in place).
    last_rects: HashMap<WinId, Rect>,
    /// Queued management-sequence actions, applied in the next do_manage.
    pending_close: Vec<WinId>,
    pending_op_start: Vec<(RiverSeatV1, bool)>, // (seat, resize?)
    pending_op_end: Vec<RiverSeatV1>,
    /// Connected `sc --idle` clients receiving the hook stream (hlwm `--idle`).
    idle_clients: Vec<UnixStream>,
    /// User-defined attributes (hlwm `my_*`), created with `new_attr`.
    user_attrs: HashMap<String, String>,
    /// True while the monitor topology is auto-derived from outputs (no explicit
    /// `set_monitors`); lets output hotplug re-detect without clobbering a config.
    auto_monitors: bool,
}

impl State {
    /// Request a manage sequence after an out-of-band state change (new window,
    /// IPC command). Safe to call any time.
    fn request_manage(&self) {
        if let Some(wm) = &self.wm {
            wm.manage_dirty();
        }
    }

    /// Emit a hook to every connected `sc --idle` client (hlwm `emit_hook`).
    /// Fields are tab-separated, one hook per line. Dead clients are dropped.
    fn emit_hook(&mut self, parts: &[&str]) {
        if self.idle_clients.is_empty() {
            return;
        }
        let mut line = parts.join("\t");
        line.push('\n');
        let bytes = line.as_bytes();
        self.idle_clients
            .retain_mut(|c| c.write_all(bytes).and_then(|_| c.flush()).is_ok());
    }

    /// Tag a new window should land on: the focused monitor's tag, else tag 1.
    fn default_tag(&self) -> TagId {
        self.monitors.focused().map(|m| m.tag).unwrap_or(1)
    }

    /// Build one base monitor per output (hlwm `detect_monitors`), the fallback
    /// when the `autostart` never calls `set_monitors`. Rebuilds on output
    /// hotplug *only* while the topology is still auto-derived — an explicit
    /// `set_monitors` clears `auto_monitors` and is never clobbered.
    fn maybe_detect_monitors(&mut self) {
        let mut geos: Vec<OutputGeo> = self
            .outputs
            .values()
            .map(|o| o.geo)
            .filter(|g| g.w > 0 && g.h > 0)
            .collect();
        if geos.is_empty() {
            return;
        }
        // Populate once when empty, or re-derive when the output count changes
        // and we're still in auto mode (a monitor was plugged in/out).
        let count_changed = self.auto_monitors && self.monitors.list.len() != geos.len();
        if !self.monitors.list.is_empty() && !count_changed {
            return;
        }
        geos.sort_by_key(|g| (g.x, g.y));
        let rects: Vec<Rect> = geos.iter().map(|g| Rect::new(g.x, g.y, g.w, g.h)).collect();
        self.monitors.set_monitors(&rects);
    }

    /// Tag of the focused monitor (where new windows and frame commands act).
    fn focused_tag(&self) -> TagId {
        self.monitors.focused().map(|m| m.tag).unwrap_or(1)
    }

    /// Record the focused monitor's current tag as its "previous" tag, before a
    /// tag switch, so `use_previous` can toggle back (hlwm).
    fn remember_prev_tag(&mut self) {
        if let Some(m) = self.monitors.focused() {
            self.prev_tag.insert(self.monitors.focus, m.tag);
        }
    }

    /// Usable rect of the focused monitor (the area its frame tree lays out in).
    fn focused_area(&self) -> Rect {
        self.monitors
            .focused()
            .map(|m| m.usable())
            .unwrap_or(Rect::new(0, 0, 0, 0))
    }

    /// The frame tree for a tag, creating an empty one (using the configured
    /// `default_frame_layout`) on first use.
    fn tag_tree_mut(&mut self, tag: TagId) -> &mut Frame {
        let default = self.default_frame_layout;
        self.tags.entry(tag).or_insert_with(|| Frame::with_layout(default))
    }

    /// The focused monitor's frame tree.
    fn focused_tree_mut(&mut self) -> &mut Frame {
        let tag = self.focused_tag();
        self.tag_tree_mut(tag)
    }

    /// Replace `tag`'s frame tree from a serialized layout string (hlwm `load`).
    /// Window ids that no longer exist are dropped; windows currently on the tag
    /// but absent from the new layout are appended so nothing is lost.
    fn load_layout(&mut self, tag: TagId, s: &str) -> Result<(), String> {
        let mut frame = Frame::deserialize(s).ok_or("load: malformed layout string")?;
        let existing: HashSet<WinId> = self.windows.keys().copied().collect();
        frame.retain_windows(&existing);
        let in_new: HashSet<WinId> = frame.all_windows().into_iter().collect();
        let orphans: Vec<WinId> = self
            .windows
            .iter()
            .filter(|(wid, w)| w.tag == tag && !in_new.contains(wid))
            .map(|(wid, _)| *wid)
            .collect();
        for w in orphans {
            frame.insert_window(w);
        }
        for wid in frame.all_windows() {
            if let Some(w) = self.windows.get_mut(&wid) {
                w.tag = tag;
            }
        }
        self.tags.insert(tag, frame);
        self.request_manage();
        Ok(())
    }

    /// Lay out every placed window across all monitors, in render order
    /// (bottom → top by monitor `z`, then by layer: tiled < floating < fullscreen).
    /// Tiled windows come from each tag's frame tree; floating windows live only
    /// in the registry and render above the tiling at their `float_geo`.
    fn compute_layout(&self) -> Vec<RenderItem> {
        let order = self.monitors.render_order(); // bottom → top
        let mut claimed: HashSet<WinId> = HashSet::new();
        let mut per_mon: HashMap<usize, Vec<RenderItem>> = HashMap::new();

        // Claim windows top → bottom so a higher monitor wins a contested tag.
        for &mi in order.iter().rev() {
            let m = &self.monitors.list[mi];
            let tag = m.tag;
            let usable = m.usable();
            let mut items: Vec<RenderItem> = Vec::new();

            // Tiled windows from the frame tree (floating ones are skipped here
            // and rendered separately below, above the tiling).
            if let Some(tree) = self.tags.get(&tag) {
                // smart_frame_surroundings: a lone frame gets no surrounding gap.
                let gap = if self.smart_frame_surroundings && tree.is_single_leaf() {
                    0
                } else {
                    self.window_gap
                };
                for p in tree.placements(usable, gap) {
                    if self.windows.get(&p.win).map_or(false, |w| w.floating) {
                        continue;
                    }
                    if !claimed.insert(p.win) {
                        continue;
                    }
                    let fs = self.windows.get(&p.win).map_or(false, |w| w.fullscreen);
                    items.push(RenderItem {
                        mon: mi,
                        win: p.win,
                        rect: p.rect,
                        visible: p.visible,
                        layer: if fs { Layer::Fullscreen } else { Layer::Tiled },
                        seq: 0,
                    });
                }
            }

            // Floating windows on this tag (not in the tree).
            for (wid, w) in &self.windows {
                if w.tag != tag || !w.floating || !claimed.insert(*wid) {
                    continue;
                }
                items.push(RenderItem {
                    mon: mi,
                    win: *wid,
                    rect: w.float_geo,
                    visible: true,
                    layer: if w.fullscreen { Layer::Fullscreen } else { Layer::Floating },
                    seq: w.raise_seq,
                });
            }

            per_mon.insert(mi, items);
        }

        // Emit bottom → top; within a monitor, low layer first, then by the
        // float stacking key (so `raise`/`lower` and insertion order are stable).
        let mut out = Vec::new();
        for mi in order {
            if let Some(mut items) = per_mon.remove(&mi) {
                items.sort_by_key(|i| (i.layer, i.seq));
                out.append(&mut items);
            }
        }
        out
    }

    /// The river output a virtual monitor sits on (by geometric containment of
    /// its centre), for fullscreen. Falls back to any output.
    fn output_for_monitor(&self, mi: usize) -> Option<RiverOutputV1> {
        let m = &self.monitors.list[mi];
        let cx = m.rect.x + m.rect.w / 2;
        let cy = m.rect.y + m.rect.h / 2;
        self.outputs
            .values()
            .find(|o| {
                cx >= o.geo.x
                    && cx < o.geo.x + o.geo.w
                    && cy >= o.geo.y
                    && cy < o.geo.y + o.geo.h
            })
            .or_else(|| self.outputs.values().next())
            .map(|o| o.output.clone())
    }

    /// Focus a window by id: select it in its tag's tree and focus the monitor
    /// currently showing that tag. Used by click-to-focus.
    fn focus_window_by_id(&mut self, wid: WinId) {
        let Some(tag) = self.windows.get(&wid).map(|w| w.tag) else {
            return;
        };
        if let Some(tree) = self.tags.get_mut(&tag) {
            tree.focus_window(wid);
        }
        if let Some(mi) = self.monitors.list.iter().position(|m| m.tag == tag) {
            self.monitors.focus = mi;
        }
        if let Some(w) = self.windows.get_mut(&wid) {
            w.urgent = false; // focusing a window clears its urgency
        }
        // hlwm raise_on_focus: lift a focused floating window above the others.
        if self.raise_on_focus && self.windows.get(&wid).map_or(false, |w| w.floating) {
            let seq = self.next_raise;
            self.next_raise += 1;
            if let Some(w) = self.windows.get_mut(&wid) {
                w.raise_seq = seq;
            }
        }
        let title = self.windows.get(&wid).and_then(|w| w.title.clone()).unwrap_or_default();
        self.emit_hook(&["focus_changed", &wid.to_string(), &title]);
    }

    /// Apply matching rules to a freshly-created window (tag/focus/floating/…).
    /// Returns the resolved tag plus the focus/switchtag consequences.
    fn apply_rules(&mut self, wid: WinId) -> RuleOutcome {
        let (app_id, title, mut tag) = {
            let w = &self.windows[&wid];
            (w.app_id.clone(), w.title.clone(), w.tag)
        };
        let mut want_floating = None;
        let mut want_pseudotile = None;
        let mut want_focus = false;
        let mut want_switchtag = false;
        for r in &self.rules {
            if let Some((exact, pat)) = &r.app_id {
                if !match_field(app_id.as_deref(), *exact, pat) {
                    continue;
                }
            }
            if let Some((exact, pat)) = &r.title {
                if !match_field(title.as_deref(), *exact, pat) {
                    continue;
                }
            }
            if let Some(t) = r.tag {
                tag = t;
            }
            // `monitor=` places the window on whatever tag that monitor shows.
            if let Some(sel) = &r.monitor {
                if let Some(t) = self.monitors.tag_of(sel) {
                    tag = t;
                }
            }
            if let Some(f) = r.floating {
                want_floating = Some(f);
            }
            if let Some(p) = r.pseudotile {
                want_pseudotile = Some(p);
            }
            if let Some(f) = r.focus {
                want_focus = f;
            }
            if let Some(s) = r.switchtag {
                want_switchtag = s;
            }
        }
        if let Some(w) = self.windows.get_mut(&wid) {
            w.tag = tag;
            if let Some(f) = want_floating {
                w.floating = f;
            }
            if let Some(p) = want_pseudotile {
                w.pseudotile = p;
            }
        }
        RuleOutcome { tag, focus: want_focus, switchtag: want_switchtag }
    }

    /// Apply rules once, the first time app_id/title is known, moving the window
    /// between tag trees / floating as the rules dictate.
    fn reapply_rules(&mut self, wid: WinId) {
        if self.rules.is_empty() {
            return;
        }
        let old_tag = match self.windows.get(&wid) {
            Some(w) if !w.rules_applied => w.tag,
            _ => return,
        };
        if let Some(w) = self.windows.get_mut(&wid) {
            w.rules_applied = true;
        }
        let outcome = self.apply_rules(wid);
        let new_tag = outcome.tag;
        let floating = self.windows.get(&wid).map_or(false, |w| w.floating);
        if new_tag != old_tag {
            if let Some(t) = self.tags.get_mut(&old_tag) {
                t.remove_window(wid);
            }
            self.tag_tree_mut(new_tag).insert_window(wid);
        }
        if floating {
            let geo = self.default_float_geo();
            self.make_floating(wid, geo);
        }
        // switchtag: pull the window's tag onto the focused monitor (follow it).
        if outcome.switchtag {
            self.monitors.show_on_focused(new_tag);
        }
        // focus: select the window (and the monitor showing its tag).
        if outcome.focus {
            self.focus_window_by_id(wid);
        }
        self.request_manage();
    }

    /// A reasonable default floating rect: centred half-size on the focused monitor.
    fn default_float_geo(&self) -> Rect {
        let a = self.focused_area();
        Rect::new(a.x + a.w / 4, a.y + a.h / 4, (a.w / 2).max(160), (a.h / 2).max(120))
    }

    /// The window that should hold keyboard focus: the focused monitor's tag
    /// tree's focused window.
    fn focused_window(&self) -> Option<WinId> {
        let tag = self.focused_tag();
        self.tags.get(&tag).and_then(|t| t.focused_window())
    }

    /// Register a key binding (hlwm `keybind`). Creates the river binding object
    /// now and queues it for `enable()` in the next manage sequence.
    fn add_keybind(&mut self, spec: String, mods_bits: u32, keysym: u32, command: Vec<String>) -> Result<(), String> {
        let binding = {
            let mgr = self
                .xkb_bindings
                .as_ref()
                .ok_or("river_xkb_bindings_v1 unavailable")?;
            let seat = self.seats.first().ok_or("no seat available yet")?;
            let mods = protocol::river_seat_v1::Modifiers::from_bits_retain(mods_bits);
            mgr.get_xkb_binding(seat, keysym, mods, &self.qh, ())
        };
        self.keybinds.insert(
            binding.id(),
            KeyBind {
                spec,
                binding: binding.clone(),
                command,
            },
        );
        self.pending_enable.push(binding);
        self.request_manage();
        Ok(())
    }

    /// Remove all key bindings (hlwm `keyunbind --all`).
    fn clear_keybinds(&mut self) {
        for (_, kb) in self.keybinds.drain() {
            kb.binding.destroy();
        }
        self.pending_enable.clear();
    }

    /// Register a pointer binding (hlwm `mousebind`). `resize` false = move.
    fn add_mousebind(&mut self, mods_bits: u32, button: u32, resize: bool) -> Result<(), String> {
        let seat = self.seats.first().ok_or("no seat available yet")?.clone();
        let mods = protocol::river_seat_v1::Modifiers::from_bits_retain(mods_bits);
        let binding = seat.get_pointer_binding(button, mods, &self.qh, ());
        self.pointer_binds.insert(
            binding.id(),
            MouseBind {
                resize,
                seat,
                binding: binding.clone(),
            },
        );
        self.pending_pointer_enable.push(binding);
        self.request_manage();
        Ok(())
    }

    /// Remove all pointer bindings (hlwm `mouseunbind --all`).
    fn clear_mousebinds(&mut self) {
        for (_, mb) in std::mem::take(&mut self.pointer_binds) {
            mb.binding.destroy();
        }
        self.pending_pointer_enable.clear();
    }

    /// Make a window floating at `geo`. The window stays a leaf in its tag's
    /// tree (so it remains focusable / navigable); the tiling pass simply skips
    /// floating windows and they render above the tiles at `float_geo`.
    fn make_floating(&mut self, wid: WinId, geo: Rect) {
        let seq = self.next_raise;
        if let Some(w) = self.windows.get_mut(&wid) {
            if !w.floating {
                w.floating = true;
                w.float_geo = geo;
                w.raise_seq = seq;
                self.next_raise += 1;
            }
        }
    }

    /// Raise (`to_top` true) or lower the focused window in the float stack.
    fn restack_focused(&mut self, to_top: bool) {
        let Some(wid) = self.focused_window() else { return };
        let seq = if to_top {
            let s = self.next_raise;
            self.next_raise += 1;
            s
        } else {
            // Below every current float.
            self.windows.values().map(|w| w.raise_seq).min().unwrap_or(1).saturating_sub(1)
        };
        if let Some(w) = self.windows.get_mut(&wid) {
            w.raise_seq = seq;
        }
        self.request_manage();
    }

    /// The manage pass: window-management state only (propose_dimensions,
    /// set_tiled, fullscreen, close, focus, interactive ops). Runs between
    /// `manage_start` and `manage_finish`.
    fn do_manage(&mut self) {
        // Tell every new window to use server-side decoration so it draws no
        // titlebar/borders of its own — sfwm owns all decoration, like hlwm.
        // (No effect on clients that only support CSD.)
        for w in self.windows.values_mut() {
            if !w.ssd_applied {
                w.win.use_ssd();
                w.ssd_applied = true;
            }
        }

        // Enabling bindings is window-management state — do it inside the sequence.
        for b in self.pending_enable.drain(..) {
            b.enable();
        }
        for b in self.pending_pointer_enable.drain(..) {
            b.enable();
        }

        // Queued window closes.
        for wid in std::mem::take(&mut self.pending_close) {
            if let Some(w) = self.windows.get(&wid) {
                w.win.close();
            }
        }

        // Start/stop interactive pointer operations.
        for (seat, resize) in std::mem::take(&mut self.pending_op_start) {
            if let Some(wid) = self.pointer_focus {
                let geo = self.last_rects.get(&wid).copied().unwrap_or_else(|| {
                    Rect::new(self.pointer_pos.0, self.pointer_pos.1, 320, 240)
                });
                self.make_floating(wid, geo);
                self.op = Some(PointerOp {
                    win: wid,
                    resize,
                    start_geo: geo,
                });
                seat.op_start_pointer();
            }
        }
        for seat in std::mem::take(&mut self.pending_op_end) {
            seat.op_end();
            self.op = None;
        }

        // Plan per-window state from the layout, precomputing outputs/usable rects
        // so the mutable apply loop doesn't re-borrow self.
        let layout = self.compute_layout();
        let plan: Vec<(WinId, Layer, Rect, usize)> = layout
            .iter()
            .filter(|i| i.visible || i.layer == Layer::Fullscreen)
            .map(|i| (i.win, i.layer, i.rect, i.mon))
            .collect();
        let mut mon_usable: HashMap<usize, Rect> = HashMap::new();
        let mut mon_output: HashMap<usize, RiverOutputV1> = HashMap::new();
        for &(_, _, _, mi) in &plan {
            mon_usable.entry(mi).or_insert_with(|| self.monitors.list[mi].usable());
            if let Some(o) = self.output_for_monitor(mi) {
                mon_output.entry(mi).or_insert(o);
            }
        }

        let mut fullscreen_now: HashSet<WinId> = HashSet::new();
        for (wid, layer, rect, mi) in plan {
            if layer == Layer::Fullscreen {
                fullscreen_now.insert(wid);
                if let (Some(w), Some(out)) = (self.windows.get_mut(&wid), mon_output.get(&mi)) {
                    if !w.applied_fullscreen {
                        w.win.fullscreen(out);
                        w.applied_fullscreen = true;
                    }
                }
                continue;
            }
            let usable = mon_usable[&mi];
            if let Some(w) = self.windows.get_mut(&wid) {
                if w.applied_fullscreen {
                    w.win.exit_fullscreen();
                    w.applied_fullscreen = false;
                }
                if w.pseudotile {
                    w.win.propose_dimensions(0, 0); // window keeps its natural size
                    w.win.set_tiled(river_window_v1::Edges::empty());
                } else {
                    w.win.propose_dimensions(rect.w, rect.h);
                    let edges = if w.floating {
                        river_window_v1::Edges::empty()
                    } else {
                        tiled_edges(rect, usable)
                    };
                    w.win.set_tiled(edges);
                }
            }
        }
        // Windows whose fullscreen turned off while not in the plan.
        let to_exit: Vec<WinId> = self
            .windows
            .iter()
            .filter(|(wid, w)| w.applied_fullscreen && !fullscreen_now.contains(wid))
            .map(|(wid, _)| *wid)
            .collect();
        for wid in to_exit {
            if let Some(w) = self.windows.get_mut(&wid) {
                w.win.exit_fullscreen();
                w.applied_fullscreen = false;
            }
        }

        // Keyboard focus follows the focused monitor's focused window.
        let focus_win = self
            .focused_window()
            .and_then(|wid| self.windows.get(&wid))
            .map(|w| w.win.clone());
        for seat in &self.seats {
            match &focus_win {
                Some(w) => seat.focus_window(w),
                None => seat.clear_focus(),
            }
        }
    }

    /// The render pass: rendering state only (set_position, place_*, show/hide,
    /// borders). Runs between `render_start` and `render_finish`.
    fn do_render(&mut self, qh: &QueueHandle<Self>) {
        let layout = self.compute_layout();
        let visible: HashSet<WinId> =
            layout.iter().filter(|i| i.visible).map(|i| i.win).collect();
        let focused = self.focused_window();

        // Remember where each window was placed (for interactive ops / float toggle).
        self.last_rects.clear();
        for i in &layout {
            self.last_rects.insert(i.win, i.rect);
        }

        // smart_window_surroundings: count tiled+visible windows per monitor so a
        // lone tiled window can be drawn with no border.
        let mut tiled_per_mon: HashMap<usize, usize> = HashMap::new();
        if self.smart_window_surroundings {
            for i in &layout {
                if i.visible && i.layer == Layer::Tiled {
                    *tiled_per_mon.entry(i.mon).or_insert(0) += 1;
                }
            }
        }

        let bw = self.border_width;
        let active = expand_color(self.border_active);
        let normal = expand_color(self.border_normal);
        let urgent = expand_color(self.border_urgent);

        let mut ordered_nodes: Vec<RiverNodeV1> = Vec::with_capacity(layout.len());
        for item in &layout {
            if !item.visible {
                continue;
            }
            // Position: pseudotile centres the window's natural size in its tile.
            let pos = match self.windows.get(&item.win) {
                Some(w) if w.pseudotile && !w.floating => {
                    let (dw, dh) = w.dims;
                    let dw = if dw > 0 { dw } else { item.rect.w };
                    let dh = if dh > 0 { dh } else { item.rect.h };
                    (
                        item.rect.x + (item.rect.w - dw) / 2,
                        item.rect.y + (item.rect.h - dh) / 2,
                    )
                }
                Some(_) => (item.rect.x, item.rect.y),
                None => continue,
            };

            let node = {
                let w = self.windows.get_mut(&item.win).unwrap();
                if w.node.is_none() {
                    w.node = Some(w.win.get_node(qh, ()));
                }
                w.node.clone().unwrap()
            };
            node.set_position(pos.0, pos.1);

            let w = self.windows.get(&item.win).unwrap();
            w.win.show();
            let lone = item.layer == Layer::Tiled
                && tiled_per_mon.get(&item.mon).copied() == Some(1);
            if bw > 0 && item.layer != Layer::Fullscreen && !lone {
                let c = if Some(item.win) == focused {
                    active
                } else if w.urgent {
                    urgent
                } else {
                    normal
                };
                w.win.set_borders(all_edges(), bw, c.0, c.1, c.2, c.3);
            } else {
                w.win.set_borders(river_window_v1::Edges::empty(), 0, 0, 0, 0, 0);
            }
            ordered_nodes.push(node);
        }

        // Hide every window not in the visible set (other tags, max-obscured, …).
        for (wid, w) in &self.windows {
            if !visible.contains(wid) {
                w.win.hide();
            }
        }

        // Enforce the exact bottom → top order.
        if let Some(first) = ordered_nodes.first() {
            first.place_bottom();
        }
        for pair in ordered_nodes.windows(2) {
            pair[1].place_above(&pair[0]);
        }
    }
}

/// All four edges (for full borders).
fn all_edges() -> river_window_v1::Edges {
    use river_window_v1::Edges;
    Edges::Top | Edges::Bottom | Edges::Left | Edges::Right
}

/// Expand an 8-bit-per-channel colour to the protocol's 0..=0xffffffff range
/// (each byte replicated, so 0xff → 0xffffffff). Assumes opaque/premultiplied.
fn expand_color(c: (u8, u8, u8, u8)) -> (u32, u32, u32, u32) {
    let e = |v: u8| (v as u32) * 0x0101_0101;
    (e(c.0), e(c.1), e(c.2), e(c.3))
}

/// Does `value` match `pat` (exact when `exact`, else substring)?
fn match_field(value: Option<&str>, exact: bool, pat: &str) -> bool {
    match value {
        Some(v) if exact => v == pat,
        Some(v) => v.contains(pat),
        None => false,
    }
}

/// Which edges of `rect` are interior to `usable` (adjacent to another tile),
/// for `set_tiled`.
fn tiled_edges(rect: Rect, usable: Rect) -> river_window_v1::Edges {
    use river_window_v1::Edges;
    let mut e = Edges::empty();
    if rect.y > usable.y {
        e |= Edges::Top;
    }
    if rect.y + rect.h < usable.y + usable.h {
        e |= Edges::Bottom;
    }
    if rect.x > usable.x {
        e |= Edges::Left;
    }
    if rect.x + rect.w < usable.x + usable.w {
        e |= Edges::Right;
    }
    e
}

/// Resolve the IPC socket path. Shared (by duplication) with `sc`.
fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("SOMMERFLUSS_SOCKET") {
        return PathBuf::from(p);
    }
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".to_string());
    PathBuf::from(dir).join(format!("sfwm-{display}.sock"))
}

/// Launch the user's `autostart` script, if present, with the socket path in the
/// environment so the `sc` calls inside it connect back to us. Non-fatal if absent.
fn spawn_autostart(sock: &std::path::Path) {
    let path = std::env::var("SOMMERFLUSS_CONFIG").map(PathBuf::from).unwrap_or_else(|_| {
        let cfg = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_default();
                PathBuf::from(home).join(".config")
            });
        cfg.join("sommerfluss").join("autostart")
    });
    if !path.exists() {
        eprintln!("sfwm: no autostart at {} (skipping)", path.display());
        return;
    }
    match std::process::Command::new(&path)
        .env("SOMMERFLUSS_SOCKET", sock)
        .spawn()
    {
        Ok(_) => eprintln!("sfwm: launched autostart {}", path.display()),
        Err(e) => eprintln!("sfwm: failed to launch autostart {}: {e}", path.display()),
    }
}

fn main() {
    let conn = Connection::connect_to_env()
        .expect("could not connect to a Wayland display — is WAYLAND_DISPLAY set? run this under river");
    let (globals, event_queue) =
        registry_queue_init::<State>(&conn).expect("failed to initialize the Wayland registry");
    let qh = event_queue.handle();

    // river only advertises this global to the designated window-manager client.
    let wm: RiverWindowManagerV1 = globals
        .bind(&qh, 1..=5, ())
        .expect("river_window_manager_v1 not found — run under river 0.4+ as its window manager");

    // Keyboard bindings live in a sibling protocol, advertised alongside the WM
    // global. Optional: if river is too old to advertise it, keybinds just fail
    // gracefully rather than preventing startup.
    let xkb_bindings: Option<RiverXkbBindingsV1> = match globals.bind(&qh, 1..=3, ()) {
        Ok(b) => Some(b),
        Err(e) => {
            eprintln!("sfwm: river_xkb_bindings_v1 unavailable ({e}); keybinds disabled");
            None
        }
    };

    let mut state = State {
        wm: Some(wm),
        qh: qh.clone(),
        outputs: HashMap::new(),
        windows: HashMap::new(),
        win_by_obj: HashMap::new(),
        next_win: 1,
        tags: HashMap::new(),
        window_gap: 0,
        monitors: Monitors::new(),
        seats: Vec::new(),
        xkb_bindings,
        keybinds: HashMap::new(),
        pending_enable: Vec::new(),
        border_width: 0,
        border_active: (0x4e, 0x9b, 0xcf, 0xff),
        border_normal: (0x1d, 0x25, 0x2b, 0xff),
        border_urgent: (0xff, 0x6c, 0x6b, 0xff),
        focus_follows_mouse: false,
        raise_on_focus: false,
        smart_frame_surroundings: false,
        smart_window_surroundings: false,
        default_frame_layout: frame::LayoutMode::Vertical,
        next_raise: 1,
        rules: Vec::new(),
        tag_monitor: HashMap::new(),
        prev_tag: HashMap::new(),
        pointer_binds: HashMap::new(),
        pending_pointer_enable: Vec::new(),
        pointer_focus: None,
        pointer_pos: (0, 0),
        op: None,
        last_rects: HashMap::new(),
        pending_close: Vec::new(),
        pending_op_start: Vec::new(),
        pending_op_end: Vec::new(),
        idle_clients: Vec::new(),
        user_attrs: HashMap::new(),
        auto_monitors: true,
    };

    // --- calloop event loop: Wayland + the IPC socket on one thread ---
    let mut event_loop: EventLoop<State> =
        EventLoop::try_new().expect("failed to create the calloop event loop");
    let handle = event_loop.handle();

    WaylandSource::new(conn.clone(), event_queue)
        .insert(handle.clone())
        .expect("failed to insert the Wayland source into the event loop");

    // IPC listening socket.
    let sock = socket_path();
    let _ = std::fs::remove_file(&sock); // clear a stale socket from a prior run
    let listener = std::os::unix::net::UnixListener::bind(&sock)
        .unwrap_or_else(|e| panic!("failed to bind IPC socket {}: {e}", sock.display()));
    listener
        .set_nonblocking(true)
        .expect("failed to set the IPC socket non-blocking");
    eprintln!("sfwm: IPC socket at {}", sock.display());

    handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            |_readiness, listener, state: &mut State| {
                // Drain all pending connections (level-triggered).
                loop {
                    match listener.accept() {
                        Ok((stream, _addr)) => ipc::handle_connection(stream, state),
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            eprintln!("sfwm: IPC accept error: {e}");
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .expect("failed to insert the IPC source into the event loop");

    spawn_autostart(&sock);

    eprintln!("sfwm: connected, entering event loop");
    let res = event_loop.run(None, &mut state, |_state| {});
    let _ = std::fs::remove_file(&sock);
    res.expect("event loop failed");
}

// --- river_window_manager_v1: the heart of the manage/render loop ---------------

impl Dispatch<RiverWindowManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        wm: &RiverWindowManagerV1,
        event: river_window_manager_v1::Event,
        _: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use river_window_manager_v1::Event;
        match event {
            Event::ManageStart => {
                state.do_manage();
                wm.manage_finish();
            }

            Event::RenderStart => {
                state.do_render(qh);
                wm.render_finish();
            }

            Event::Window { id } => {
                let tag = state.default_tag();
                let wid = state.next_win;
                state.next_win += 1;
                state.win_by_obj.insert(id.id(), wid);
                state.windows.insert(wid, Window::new(id, tag));
                // New windows land in the focused frame of the focused tag's tree.
                // Rules (which key on app_id/title) are applied once those arrive.
                state.tag_tree_mut(tag).insert_window(wid);
                wm.manage_dirty();
            }

            Event::Output { id } => {
                let oid = id.id();
                state.outputs.insert(
                    oid,
                    OutputInfo {
                        output: id,
                        geo: OutputGeo::default(),
                        wl_output_name: None,
                    },
                );
                wm.manage_dirty();
            }

            Event::Seat { id } => {
                state.seats.push(id);
            }

            Event::Unavailable => {
                eprintln!("sfwm: another window manager is already connected to river");
                std::process::exit(1);
            }
            Event::Finished => std::process::exit(0),
            Event::SessionLocked | Event::SessionUnlocked => {}
        }
    }

    event_created_child!(State, RiverWindowManagerV1, [
        river_window_manager_v1::EVT_WINDOW_OPCODE => (RiverWindowV1, ()),
        river_window_manager_v1::EVT_OUTPUT_OPCODE => (RiverOutputV1, ()),
        river_window_manager_v1::EVT_SEAT_OPCODE   => (RiverSeatV1, ()),
    ]);
}

// --- per-object dispatch --------------------------------------------------------

impl Dispatch<RiverWindowV1, ()> for State {
    fn event(
        state: &mut Self,
        win: &RiverWindowV1,
        event: river_window_v1::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use river_window_v1::Event;
        let id = win.id();
        match event {
            Event::Closed => {
                if let Some(wid) = state.win_by_obj.remove(&id) {
                    if let Some(w) = state.windows.remove(&wid) {
                        if let Some(tree) = state.tags.get_mut(&w.tag) {
                            tree.remove_window(wid);
                        }
                    }
                }
                win.destroy();
                state.request_manage();
            }
            Event::AppId { app_id } => {
                if let Some(&wid) = state.win_by_obj.get(&id) {
                    if let Some(w) = state.windows.get_mut(&wid) {
                        w.app_id = app_id;
                    }
                    state.reapply_rules(wid);
                }
            }
            Event::Title { title } => {
                if let Some(&wid) = state.win_by_obj.get(&id) {
                    if let Some(w) = state.windows.get_mut(&wid) {
                        w.title = title.clone();
                    }
                    if Some(wid) == state.focused_window() {
                        let t = title.unwrap_or_default();
                        state.emit_hook(&["window_title_changed", &wid.to_string(), &t]);
                    }
                    state.reapply_rules(wid);
                }
            }
            Event::Dimensions { width, height } => {
                if let Some(w) = state.win_by_obj.get(&id).and_then(|wid| state.windows.get_mut(wid)) {
                    w.dims = (width, height);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<RiverOutputV1, ()> for State {
    fn event(
        state: &mut Self,
        out: &RiverOutputV1,
        event: river_output_v1::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use river_output_v1::Event;
        let id = out.id();
        match event {
            Event::Dimensions { width, height } => {
                if let Some(o) = state.outputs.get_mut(&id) {
                    o.geo.w = width;
                    o.geo.h = height;
                }
                state.maybe_detect_monitors();
            }
            Event::Position { x, y } => {
                if let Some(o) = state.outputs.get_mut(&id) {
                    o.geo.x = x;
                    o.geo.y = y;
                }
                state.maybe_detect_monitors();
            }
            Event::WlOutput { name } => {
                if let Some(o) = state.outputs.get_mut(&id) {
                    o.wl_output_name = Some(name);
                }
            }
            Event::Removed => {
                state.outputs.remove(&id);
                out.destroy();
                state.maybe_detect_monitors(); // hotplug: re-derive if auto
                state.request_manage();
            }
            _ => {}
        }
    }
}

impl Dispatch<RiverSeatV1, ()> for State {
    fn event(
        state: &mut Self,
        seat: &RiverSeatV1,
        event: river_seat_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use river_seat_v1::Event;
        match event {
            Event::PointerEnter { window } => {
                let wid = state.win_by_obj.get(&window.id()).copied();
                state.pointer_focus = wid;
                if state.focus_follows_mouse {
                    if let Some(wid) = wid {
                        state.focus_window_by_id(wid);
                        state.request_manage();
                    }
                }
            }
            Event::PointerLeave => {
                state.pointer_focus = None;
            }
            Event::PointerPosition { x, y } => {
                state.pointer_pos = (x, y);
            }
            // Click (or touch) on a window → focus it.
            Event::WindowInteraction { window } => {
                if let Some(&wid) = state.win_by_obj.get(&window.id()) {
                    state.focus_window_by_id(wid);
                    state.request_manage();
                }
            }
            // Interactive move/resize: op_delta is cumulative since op start.
            Event::OpDelta { dx, dy } => {
                if let Some(op) = state.op.as_ref().map(|o| (o.win, o.resize, o.start_geo)) {
                    let (win, resize, sg) = op;
                    if let Some(w) = state.windows.get_mut(&win) {
                        w.float_geo = if resize {
                            Rect::new(sg.x, sg.y, (sg.w + dx).max(60), (sg.h + dy).max(40))
                        } else {
                            Rect::new(sg.x + dx, sg.y + dy, sg.w, sg.h)
                        };
                    }
                    state.request_manage();
                }
            }
            Event::OpRelease => {
                state.pending_op_end.push(seat.clone());
                state.request_manage();
            }
            _ => {}
        }
    }
}

impl Dispatch<RiverPointerBindingV1, ()> for State {
    fn event(
        state: &mut Self,
        binding: &RiverPointerBindingV1,
        event: river_pointer_binding_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use river_pointer_binding_v1::Event;
        match event {
            Event::Pressed => {
                if let Some(mb) = state.pointer_binds.get(&binding.id()) {
                    let (resize, seat) = (mb.resize, mb.seat.clone());
                    state.pending_op_start.push((seat, resize));
                    state.request_manage();
                }
            }
            Event::Released => {
                if let Some(mb) = state.pointer_binds.get(&binding.id()) {
                    state.pending_op_end.push(mb.seat.clone());
                    state.request_manage();
                }
            }
        }
    }
}

impl Dispatch<RiverNodeV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &RiverNodeV1,
        _: river_node_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// The xkb-bindings manager global has no events.
impl Dispatch<RiverXkbBindingsV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &RiverXkbBindingsV1,
        _: river_xkb_bindings_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<RiverXkbBindingV1, ()> for State {
    fn event(
        state: &mut Self,
        binding: &RiverXkbBindingV1,
        event: river_xkb_binding_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use river_xkb_binding_v1::Event;
        // Act on press, like hlwm. Release/stop_repeat are ignored for now.
        if let Event::Pressed = event {
            let cmd = state.keybinds.get(&binding.id()).map(|kb| kb.command.clone());
            if let Some(cmd) = cmd {
                let reply = ipc::dispatch(state, &cmd);
                if let Some(rest) = reply.strip_prefix("error:") {
                    eprintln!("sfwm: keybind '{}' ->error:{}", cmd.join(" "), rest.trim_end());
                }
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
