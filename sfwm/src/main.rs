//! sfwm — sommerflusswm window manager.
//!
//! A manual, virtual-monitor tiling window manager for river 0.4+, built as a
//! herbstluftwm successor. Implemented so far: the monitor model with overlapping
//! ("virtual") monitors (milestone 2), keyboard bindings via river-xkb-bindings-v1
//! (part of milestone 5), and the per-tag **frame tree** (milestone 3) — binary
//! split tree, leaves holding window stacks with max/vertical/horizontal/grid
//! layouts. All driven at runtime over an `sc` IPC socket (sommerflusswm's
//! `herbstclient`).
//!
//! Two config layers, mirroring hlwm:
//!   1. river's own `init` — configures river and `exec`s this binary.
//!   2. sfwm's `autostart` — a shell script calling `sc` (set_monitors, add_monitor,
//!      raise_monitor, lock_tag, pad, …), a near-direct port of the hlwm autostart.

mod attr;
mod frame;
mod gestures;
mod ipc;
mod launcher;
mod monitor;
mod notify;
mod protocol;
mod tray;

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

use std::os::fd::AsFd;

use wayland_client::{
    backend::ObjectId,
    event_created_child,
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_keyboard::{self, WlKeyboard},
        wl_pointer::{self, WlPointer},
        wl_registry, wl_seat::{self, WlSeat}, wl_shm, wl_shm::WlShm,
        wl_shm_pool::WlShmPool, wl_surface::WlSurface,
    },
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1;
use wayland_protocols::wp::viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter};

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
    /// If set, this window is a dock (status bar): sticky across tags,
    /// unfocusable, untiled, pinned full-width to the named edge.
    dock: Option<DockAnchor>,
    /// The "dim inactive" decoration overlay, created lazily (see [`DimOverlay`]).
    dim: Option<DimOverlay>,
}

/// A WM-owned decoration surface drawn *above* an inactive window to dim it
/// (the in-WM replacement for picom's inactive-dim). The fill is a shared 1x1
/// single-pixel buffer scaled to the window by a viewport.
struct DimOverlay {
    deco: RiverDecorationV1,
    surface: WlSurface,
    viewport: WpViewport,
    /// Whether the overlay currently has the dim buffer attached.
    shown: bool,
    /// Last destination size set on the viewport (to avoid redundant commits).
    size: (i32, i32),
}

/// A WM-owned status bar: a `river_shell_surface_v1` placed in the render list,
/// drawn by sfwm into a `wl_shm` buffer (no Xwayland, no toolkit). Holds an
/// ordered list of modules (executors / separators / spacers) drawn left→right.
struct Bar {
    surface: WlSurface,
    shell: RiverShellSurfaceV1,
    node: RiverNodeV1,
    /// Monitor index the bar lives on (`bar create mon=N`; falls back to 0).
    mon: usize,
    /// Edge the bar is pinned to (full-width on its monitor, minus margins).
    anchor: DockAnchor,
    height: i32,
    /// Inset from the monitor edges (tint2's `panel_margin`): `margin_x` shrinks
    /// the width on both sides, `margin_y` offsets from the anchored edge — lets
    /// the bar "float" instead of running flush to the screen edges.
    margin_x: i32,
    margin_y: i32,
    /// Background fill (r, g, b, a).
    bg: (u8, u8, u8, u8),
    /// Default text colour and font size for modules that don't override them.
    fg: (u8, u8, u8, u8),
    font_size: f32,
    /// Modules in left→right order; a `Spacer` pushes the rest to the right.
    modules: Vec<BarModule>,
    /// Current shm buffer + its backing file. The contents change every frame,
    /// so it's rebuilt each render; the previous one is retired a frame later
    /// (`old`) to give the compositor time to release it before we drop it.
    buffer: Option<WlBuffer>,
    backing: Option<std::fs::File>,
    old: Option<(WlBuffer, std::fs::File)>,
    /// Surface-local x-ranges recorded on the last render, so a wl_pointer button
    /// press over the bar can be routed to the executor / tray icon under it.
    hit: Vec<HitZone>,
    /// Global position of the bar's top-left (buffer origin) from the last render;
    /// added to a hit zone's local x to give screen coords for tray Activate/menu.
    origin: (i32, i32),
}

/// A clickable region of the bar (surface-local x-range) and what it triggers.
struct HitZone {
    x0: i32,
    x1: i32,
    kind: HitKind,
}

/// What clicking a [`HitZone`] does.
enum HitKind {
    /// An executor module (id) — runs its `lclick`/`rclick` shell command.
    Exec(u64),
    /// A tray icon (item key) — Activate / SecondaryActivate / open its menu.
    Tray(String),
}

/// How a separator is drawn (tint2's separator styles).
#[derive(Clone, Copy)]
enum SepStyle {
    /// A thin vertical rule (the default).
    Line,
    /// Nothing drawn — just `size` px of empty space (invisible separator).
    Empty,
    /// A small centred dot.
    Dot,
}

/// One item in the status bar.
enum BarModule {
    /// Runs a shell command and shows its stdout (tint2's `execp`).
    Executor(Executor),
    /// A `size`-px-wide gap, optionally with a rule/dot drawn in `color`.
    Separator {
        size: i32,
        color: (u8, u8, u8, u8),
        style: SepStyle,
    },
    /// Flexible gap: absorbs leftover width so following modules right-align.
    Spacer,
    /// System-tray slot: renders the SNI tray icons (`State::tray_items`) inline,
    /// each `size`×`size` px with `spacing` px between. `size` 0 = auto (bar height).
    Tray { size: i32, spacing: i32 },
}

/// How a wallpaper image is fitted to its monitor (nitrogen's modes).
#[derive(Clone, Copy, PartialEq, Eq)]
enum WallMode {
    /// Scale to cover the monitor, crop-centre the overflow.
    Fill,
    /// Scale to fit inside the monitor, letterbox the rest.
    Fit,
    /// Scale to the exact monitor size (ignore aspect ratio).
    Stretch,
    /// No scaling; centre the image (crop/letterbox as needed).
    Center,
    /// Repeat the image to fill the monitor.
    Tile,
}

/// What a wallpaper draws: a flat colour or an image fitted by `WallMode`.
#[derive(Clone)]
enum WallpaperContent {
    Color((u8, u8, u8, u8)),
    Image {
        path: std::path::PathBuf,
        mode: WallMode,
    },
}

/// A WM-owned wallpaper for one monitor: a `river_shell_surface_v1` placed at the
/// very bottom of the render list (below all windows), drawn by sfwm into a
/// `wl_shm` buffer — sfwm's own background, since external tools (swaybg) can't
/// composite under it. Mirrors `Bar`, but its contents are static, so the
/// (expensive) decode + scale only runs when the size or content changes
/// (`last_sig`); other frames just re-place the node.
struct Wallpaper {
    surface: WlSurface,
    shell: RiverShellSurfaceV1,
    node: RiverNodeV1,
    content: WallpaperContent,
    buffer: Option<WlBuffer>,
    backing: Option<std::fs::File>,
    old: Option<(WlBuffer, std::fs::File)>,
    /// (width, height, content-hash) of the last buffer built.
    last_sig: Option<(i32, i32, u64)>,
}

/// A notification popup: one `river_shell_surface_v1`, top-right, drawn by sfwm.
/// Its contents are fixed once shown, so the buffer is built lazily exactly once;
/// only its position changes as popups above it expire.
struct Notification {
    id: u32,
    summary: String,
    body: String,
    /// 0 low, 1 normal, 2 critical (freedesktop urgency hint).
    urgency: u8,
    surface: WlSurface,
    shell: RiverShellSurfaceV1,
    node: RiverNodeV1,
    buffer: Option<WlBuffer>,
    backing: Option<std::fs::File>,
    /// Pixel height of the drawn popup (computed from the wrapped text).
    height: i32,
}

/// Themeable look of notification popups (set via `sc set notify_*`).
#[derive(Clone, Copy)]
struct NotifTheme {
    bg: (u8, u8, u8, u8),
    fg: (u8, u8, u8, u8),
    body_fg: (u8, u8, u8, u8),
    accent: (u8, u8, u8, u8),
    accent_critical: (u8, u8, u8, u8),
    width: i32,
    /// Default expiry (ms) applied when a notification requests -1; 0 = sticky.
    timeout_ms: i32,
}

impl Default for NotifTheme {
    fn default() -> Self {
        NotifTheme {
            bg: (0x1d, 0x25, 0x2b, 0xff),
            fg: (0xf7, 0xf8, 0xf3, 0xff),
            body_fg: (0xc9, 0xcc, 0xc4, 0xff),
            accent: (0x4e, 0x9b, 0xcf, 0xff),
            accent_critical: (0xff, 0x4d, 0x65, 0xff),
            width: 380,
            timeout_ms: 5000,
        }
    }
}

/// Themeable look of the launcher overlay (set via `sc set launcher_*`).
#[derive(Clone, Copy)]
struct LauncherTheme {
    /// Fullscreen backdrop; its alpha controls the see-through dim (`#rrggbbaa`).
    dim: (u8, u8, u8, u8),
    /// Search box + result-row background.
    bg: (u8, u8, u8, u8),
    /// Query text + unselected item text (and the cursor).
    fg: (u8, u8, u8, u8),
    /// Selected row background / text.
    sel_bg: (u8, u8, u8, u8),
    sel_fg: (u8, u8, u8, u8),
    /// Maximum panel width in px.
    width: i32,
}

impl Default for LauncherTheme {
    fn default() -> Self {
        LauncherTheme {
            dim: (0x0a, 0x0c, 0x0f, 0xc8),
            bg: (0x1d, 0x25, 0x2b, 0xff),
            fg: (0xf7, 0xf8, 0xf3, 0xff),
            sel_bg: (0x4e, 0x9b, 0xcf, 0xff),
            sel_fg: (0xff, 0xff, 0xff, 0xff),
            width: 760,
        }
    }
}

/// What the launcher does when a row is chosen.
enum LauncherAction {
    /// drun: launch the app whose Exec command is `execs[i]` (parallel to entries).
    Apps(Vec<String>),
    /// dmenu (`sc menu`): write the chosen entry back to this client (Esc → empty).
    Menu(UnixStream),
}

/// The fullscreen fuzzy launcher: one translucent `river_shell_surface_v1`
/// covering the focused monitor, keyboard-focused via `focus_shell_surface`.
/// sfwm draws it (KISS/instant, no external process). Backs both the app launcher
/// (`sc launcher`) and dmenu (`sc menu`).
struct Launcher {
    surface: WlSurface,
    shell: RiverShellSurfaceV1,
    node: RiverNodeV1,
    buffer: Option<WlBuffer>,
    backing: Option<std::fs::File>,
    old: Option<(WlBuffer, std::fs::File)>,
    /// The current search text.
    query: String,
    /// The full item list (display text).
    entries: Vec<String>,
    /// What Enter does with the chosen entry.
    action: LauncherAction,
    /// Indices into `entries`, best match first.
    matches: Vec<usize>,
    /// Highlighted row (index into `matches`).
    selected: usize,
    /// First visible row (scroll offset into `matches`).
    scroll: usize,
    /// Hash of (size, query, selected, scroll) so we only re-rasterize on change.
    last_sig: Option<u64>,
}

/// A WM-drawn tray context menu (com.canonical.dbusmenu), rendered as an
/// interactive fullscreen overlay like the launcher: a transparent backdrop that
/// captures all pointer input (click-outside closes it) with the menu drawn as
/// one or more cascading columns anchored at the clicked tray icon. sfwm draws
/// it and routes hover/click itself — appindicator apps (nm-applet, etc.) can't
/// pop their own menus under sfwm, so the WM speaks dbusmenu directly.
struct TrayMenu {
    surface: WlSurface,
    shell: RiverShellSurfaceV1,
    node: RiverNodeV1,
    buffer: Option<WlBuffer>,
    backing: Option<std::fs::File>,
    old: Option<(WlBuffer, std::fs::File)>,
    /// Tray item key — used to send `MenuClicked` back to the item.
    key: String,
    /// The menu tree (root level).
    root: Vec<tray::MenuNode>,
    /// Global anchor (the tray icon position); columns fan out from here.
    anchor: (i32, i32),
    /// Monitor rect the overlay covers (for clamping columns on-screen).
    mon: Rect,
    /// Cascade path: `open_path[i]` is the row index (into column `i`'s visible
    /// rows) of the submenu that opened column `i+1`. Column 0 is always the root.
    open_path: Vec<usize>,
    /// Hover highlight as (column, row-in-column).
    hover: Option<(usize, usize)>,
    /// Column geometry recorded on the last render, in surface-local coords, for
    /// hit-testing pointer motion/clicks.
    columns: Vec<MenuColumn>,
    /// Hash of the visual state, so we only re-rasterize on change.
    last_sig: Option<u64>,
}

/// One rendered menu column's placement + per-row hit rectangles (surface-local).
struct MenuColumn {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    rows: Vec<MenuRow>,
}

/// A single row's vertical extent within its column (surface-local y).
struct MenuRow {
    y0: i32,
    y1: i32,
}

impl TrayMenu {
    /// The visible node lists shown per open column, following `open_path`.
    /// Column 0 is the root; each further column is the opened submenu's children.
    fn columns_nodes(&self) -> Vec<Vec<&tray::MenuNode>> {
        let mut cols: Vec<Vec<&tray::MenuNode>> = Vec::new();
        let mut level: Vec<&tray::MenuNode> = self.root.iter().filter(|n| n.visible).collect();
        cols.push(level.clone());
        for &sel in &self.open_path {
            let Some(node) = level.get(sel) else { break };
            if !node.has_submenu {
                break;
            }
            let next: Vec<&tray::MenuNode> = node.children.iter().filter(|n| n.visible).collect();
            if next.is_empty() {
                break;
            }
            cols.push(next.clone());
            level = cols.last().unwrap().clone();
        }
        cols
    }
}

/// How often an executor's command is run.
#[derive(Clone, Copy)]
enum ExecMode {
    /// Re-run every N seconds (N == 0 means run exactly once).
    Interval(u64),
    /// A long-running command; each line it prints becomes the displayed text.
    Continuous,
}

/// A status-bar executor: a shell command whose stdout is displayed, refreshed
/// on an interval or streamed continuously. The command runs on a worker thread
/// and sends its output back over `State::bar_tx`, so a slow command (e.g. a
/// `curl` weather fetch) never blocks the WM.
struct Executor {
    id: u64,
    fg: Option<(u8, u8, u8, u8)>,
    bg: Option<(u8, u8, u8, u8)>,
    pad: i32,
    /// Per-module font family (tint2's `execp_font`). Needed for icon fonts
    /// (Font Awesome / Weather Icons) whose Private-Use-Area glyphs cosmic-text's
    /// automatic fallback won't reach — name the font explicitly here.
    family: Option<String>,
    /// Per-module font size in px; falls back to the bar default when None.
    size: Option<f32>,
    /// Click actions (run via `sh -c`); stored now, pointer-routing wired later.
    #[allow(dead_code)]
    lclick: Option<String>,
    #[allow(dead_code)]
    rclick: Option<String>,
    /// Most recent output line/blob to display.
    text: String,
    /// Set true to tell the worker thread to stop (on `bar clear`/recreate).
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The running child (continuous mode) so it can be killed on teardown.
    child: Option<std::process::Child>,
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
            dock: None,
            dim: None,
        }
    }
}

/// Which compositing layer a placement belongs to within a monitor (low→high).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Layer {
    Tiled,
    Floating,
    /// Docks (status bars) render above tiled/floating windows but below
    /// fullscreen, matching how a fullscreen window covers a panel in hlwm.
    Dock,
    Fullscreen,
}

/// Which edge a dock window (status bar) is pinned to. A dock spans its
/// monitor's full width and is sticky (shown on every tag), unfocusable, and
/// untiled — the Wayland analog of hlwm's `manage=off` dock windows. Since
/// river's WM protocol exposes no X11 `_NET_WM_WINDOW_TYPE=DOCK` hint, docks are
/// designated by an `app_id`/class rule (e.g. `rule class=tint2 dock=top`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DockAnchor {
    Top,
    Bottom,
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
    /// Designate the window a dock (status bar) pinned to this edge.
    dock: Option<DockAnchor>,
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
        match self.dock {
            Some(DockAnchor::Top) => p.push("dock=top".into()),
            Some(DockAnchor::Bottom) => p.push("dock=bottom".into()),
            None => {}
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
    /// Touchpad gesture recognition (direct libinput; None if unavailable).
    gestures: Option<gestures::Gestures>,
    /// Gesture bindings: normalized spec ("swipe3-left") → command argv.
    gesturebinds: HashMap<String, Vec<String>>,
    /// Tags in floating mode (hlwm `floating <tag> on`): every window on such a
    /// tag is laid out floating, regardless of its per-window flag.
    floating_tags: HashSet<u32>,

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

    // --- inactive-window dimming (in-WM, replaces a picom effect) ---
    /// Globals needed for the dim overlay; absent → dimming silently disabled.
    compositor: Option<WlCompositor>,
    viewporter: Option<WpViewporter>,
    spb: Option<WpSinglePixelBufferManagerV1>,
    /// Shared 1x1 premultiplied-black buffer at the configured alpha.
    dim_buffer: Option<WlBuffer>,
    /// Shared-memory global, for drawing the status bar (and later overlays).
    shm: Option<WlShm>,
    /// The WM-drawn status bar, once `sc bar create` has made it.
    bar: Option<Bar>,
    /// Per-monitor wallpapers (`sc wallpaper`), keyed by monitor index.
    wallpapers: HashMap<usize, Wallpaper>,
    /// calloop handle, stored so bar executors can be (de)registered after the
    /// loop is built. Set in `run()` once the loop exists.
    loop_handle: Option<calloop::LoopHandle<'static, State>>,
    /// Executor worker threads send `(module_id, text)` here; the channel source
    /// updates the module and triggers a redraw on the main thread.
    bar_tx: Option<calloop::channel::Sender<(u64, String)>>,
    /// Text shaping/rasterization state for the bar (lazily built on first draw).
    font_system: Option<cosmic_text::FontSystem>,
    swash_cache: Option<cosmic_text::SwashCache>,
    /// Monotonic id handed to each new bar executor.
    next_bar_module: u64,
    /// Dim strength for unfocused windows, 0.0 (off) ..= 1.0 (hlwm-ish setting).
    inactive_dim: f64,
    /// Active notification popups (most-recent last), each its own shell surface.
    notifications: Vec<Notification>,
    /// Notification look (colours/width/timeout), set via `sc set notify_*`.
    notif_theme: NotifTheme,
    /// A `wl_keyboard` (bound via `wl_seat`) — only used to drive the launcher,
    /// which is the one WM-owned surface that takes keyboard focus.
    wl_keyboard: Option<WlKeyboard>,
    /// xkb state for turning keycodes into text/keysyms while the launcher is open.
    xkb_state: Option<xkbcommon::xkb::State>,
    /// The fullscreen fuzzy launcher, while open.
    launcher: Option<Launcher>,
    /// Launcher look (colours/dim/width), set via `sc set launcher_*`.
    launcher_theme: LauncherTheme,
    /// Cached `.desktop` apps (loaded on first launcher open).
    apps: Vec<launcher::DesktopApp>,
    /// SNI system-tray items (from the tray D-Bus thread), in stable insertion
    /// order; drawn by a `BarModule::Tray` slot.
    tray_items: Vec<tray::TrayItem>,
    /// Sends click/scroll commands to the tray thread; None until the tray starts.
    tray_cmd: Option<std::sync::mpsc::Sender<tray::TrayCmd>>,
    /// The `wl_pointer` (bound via `wl_seat`) — drives bar/tray click routing.
    /// sfwm's shell surfaces receive wl_pointer directly (river protocol), so this
    /// is how bar executors and tray icons get their clicks.
    wl_pointer: Option<WlPointer>,
    /// True while the pointer is over the bar's surface (wl_pointer Enter/Leave).
    pointer_over_bar: bool,
    /// Latest surface-local pointer position over the bar (wl_pointer Enter/Motion).
    bar_pointer: (i32, i32),
    /// Last pointer button pressed (evdev code), so the `shell_surface_interaction`
    /// fallback can route with the correct button when wl_pointer focus isn't
    /// delivered to our shell surface. Defaults to BTN_LEFT.
    last_pointer_button: u32,
    /// The open tray context menu (dbusmenu overlay), while one is showing.
    tray_menu: Option<TrayMenu>,
    /// True while the pointer is over the tray-menu surface (wl_pointer Enter/Leave).
    pointer_over_menu: bool,
    /// Latest surface-local pointer position over the tray menu.
    menu_pointer: (i32, i32),
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
                // Inset the tiling area by the gap so the margin is uniform on
                // every side — including the monitor edge. Without this, edge
                // windows sit flush and their outer border (drawn just outside
                // the content box) is clipped off-screen.
                let area = Rect::new(
                    usable.x + gap,
                    usable.y + gap,
                    (usable.w - 2 * gap).max(0),
                    (usable.h - 2 * gap).max(0),
                );
                for p in tree.placements(area, gap) {
                    // Floating windows and docks are rendered separately, not
                    // from the frame tree.
                    if self
                        .windows
                        .get(&p.win)
                        .map_or(false, |w| self.win_floats(w) || w.dock.is_some())
                    {
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
                if w.tag != tag || !self.win_floats(w) || !claimed.insert(*wid) {
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

        // Dock windows (status bars): sticky across tags, full-width pinned to
        // their anchor edge, on their home monitor. Resolved by containment of
        // the parked origin so a dock stays put as tags switch underneath it.
        for (wid, w) in &self.windows {
            let Some(anchor) = w.dock else { continue };
            if !claimed.insert(*wid) {
                continue;
            }
            let (px, py) = (w.float_geo.x, w.float_geo.y);
            let mi = self
                .monitors
                .list
                .iter()
                .position(|m| {
                    px >= m.rect.x
                        && px < m.rect.x + m.rect.w
                        && py >= m.rect.y
                        && py < m.rect.y + m.rect.h
                })
                .filter(|&i| per_mon.contains_key(&i))
                .or_else(|| per_mon.keys().min().copied());
            let Some(mi) = mi else { continue };
            let m = &self.monitors.list[mi];
            let want_h = if w.dims.1 > 0 { w.dims.1 } else { 28 };
            let h = want_h.min(m.rect.h.max(1));
            let y = match anchor {
                DockAnchor::Top => m.rect.y,
                DockAnchor::Bottom => m.rect.y + m.rect.h - h,
            };
            let rect = Rect::new(m.rect.x, y, m.rect.w, h);
            per_mon.entry(mi).or_default().push(RenderItem {
                mon: mi,
                win: *wid,
                rect,
                visible: true,
                layer: Layer::Dock,
                seq: w.raise_seq,
            });
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
        if self.raise_on_focus && self.windows.get(&wid).map_or(false, |w| self.win_floats(w)) {
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
        let mut want_dock = None;
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
            if let Some(d) = r.dock {
                want_dock = Some(d);
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
            w.dock = want_dock;
        }
        // A dock never steals focus.
        if want_dock.is_some() {
            want_focus = false;
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
        if let Some(anchor) = self.windows.get(&wid).and_then(|w| w.dock) {
            self.make_dock(wid, anchor);
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

    /// Whether a window is laid out floating: its own flag, or its whole tag is
    /// in floating mode (hlwm `floating <tag> on`).
    fn win_floats(&self, w: &Window) -> bool {
        w.floating || self.floating_tags.contains(&w.tag)
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

    /// Designate `wid` a dock pinned to `anchor`: sticky, unfocusable, untiled,
    /// full-width on its monitor. The exact rect (edge, width, height) is derived
    /// each layout pass from the live monitor geometry; here we only record the
    /// anchor, a stacking key, and a home position (the focused monitor's origin)
    /// so the layout pass can resolve which output it belongs to.
    fn make_dock(&mut self, wid: WinId, anchor: DockAnchor) {
        let seq = self.next_raise;
        let origin = self
            .monitors
            .list
            .get(self.monitors.focus)
            .map(|m| (m.rect.x, m.rect.y))
            .unwrap_or((0, 0));
        if let Some(w) = self.windows.get_mut(&wid) {
            w.dock = Some(anchor);
            w.floating = false;
            w.raise_seq = seq;
            w.float_geo = Rect::new(origin.0, origin.1, 0, 0);
        }
        self.next_raise += 1;
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

        // Windows that float only because their tag is floating may never have
        // been given a float rect (float_geo defaults to 0x0) — seed one from
        // where they were last placed, or the default centred rect.
        let needs_geo: Vec<WinId> = self
            .windows
            .iter()
            .filter(|(_, w)| {
                self.floating_tags.contains(&w.tag)
                    && (w.float_geo.w <= 0 || w.float_geo.h <= 0)
            })
            .map(|(wid, _)| *wid)
            .collect();
        for wid in needs_geo {
            let geo = self
                .last_rects
                .get(&wid)
                .copied()
                .filter(|r| r.w > 0 && r.h > 0)
                .unwrap_or_else(|| self.default_float_geo());
            if let Some(w) = self.windows.get_mut(&wid) {
                w.float_geo = geo;
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
            let floats = self.windows.get(&wid).map_or(false, |w| self.win_floats(w));
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
                    let edges = if floats || w.dock.is_some() {
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

        // Keyboard focus: the tray menu (for Escape) then the launcher grab it
        // while open, otherwise it follows the focused monitor's focused window.
        let menu_shell = self.tray_menu.as_ref().map(|m| m.shell.clone());
        let launcher_shell = self.launcher.as_ref().map(|l| l.shell.clone());
        let focus_win = self
            .focused_window()
            .and_then(|wid| self.windows.get(&wid))
            .map(|w| w.win.clone());
        for seat in &self.seats {
            match (&menu_shell, &launcher_shell, &focus_win) {
                (Some(shell), _, _) => seat.focus_shell_surface(shell),
                (None, Some(shell), _) => seat.focus_shell_surface(shell),
                (None, None, Some(w)) => seat.focus_window(w),
                (None, None, None) => seat.clear_focus(),
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
                Some(w) if w.pseudotile && !self.win_floats(w) => {
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
            if bw > 0 && item.layer != Layer::Fullscreen && item.layer != Layer::Dock && !lone {
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

        // Draw the per-monitor wallpapers (bottom-most), then enforce the exact
        // bottom → top order: [wallpapers, windows, bar].
        let wp_nodes = self.render_wallpaper(qh);
        let mut stack = wp_nodes;
        stack.extend(ordered_nodes);
        if let Some(first) = stack.first() {
            first.place_bottom();
        }
        for pair in stack.windows(2) {
            pair[1].place_above(&pair[0]);
        }

        // Draw + place the WM-owned status bar above everything.
        let top_node = stack.last().cloned();
        self.render_bar(qh, top_node.as_ref());

        self.apply_dim(qh, &layout, focused);

        // Notification popups sit on top of everything (placed last)...
        self.render_notifications(qh);
        // ...except the launcher overlay, which is the very topmost when open...
        self.render_launcher(qh);
        // ...and the tray context menu, topmost of all while it's open.
        self.render_menu(qh);
    }

    /// Draw and place the status bar, inside the render sequence. Rebuilds the
    /// shm buffer only when the size changes; always re-syncs + commits so the
    /// surface lands atomically with `render_finish` (protocol requirement).
    /// `top_node` is the topmost window node, so the bar sits above all windows.
    fn render_bar(&mut self, qh: &QueueHandle<Self>, top_node: Option<&RiverNodeV1>) {
        let Some(shm) = self.shm.clone() else { return };
        let bar_mon = self.bar.as_ref().map_or(0, |b| b.mon);
        let Some(rect) = self
            .monitors
            .list
            .get(bar_mon)
            .or_else(|| self.monitors.list.first())
            .map(|m| m.rect)
        else {
            return;
        };
        if self.bar.is_none() {
            return;
        }
        // Build the (expensive) font system once, on first draw.
        if self.font_system.is_none() {
            self.font_system = Some(cosmic_text::FontSystem::new());
            self.swash_cache = Some(cosmic_text::SwashCache::new());
        }
        // Take the font state out so we can borrow self.bar mutably alongside it.
        let mut fs = self.font_system.take().unwrap();
        let mut sc = self.swash_cache.take().unwrap();
        let bar = self.bar.as_mut().unwrap();

        let h = bar.height.max(1);
        let w = (rect.w - 2 * bar.margin_x).max(1);
        let x = rect.x + bar.margin_x;
        let y = match bar.anchor {
            DockAnchor::Top => rect.y + bar.margin_y,
            DockAnchor::Bottom => rect.y + rect.h - h - bar.margin_y,
        };
        bar.origin = (x, y);
        let font_size = bar.font_size;
        // Render every item, including SNI "Passive" ones — many apps (appindicator,
        // nm-applet) sit in Passive and would otherwise never show.
        let tray_count = self.tray_items.len() as i32;

        // --- draw the modules into a fresh BGRA buffer ---
        let mut data = vec![0u8; w as usize * h as usize * 4];
        fill_rect(&mut data, w, h, 0, 0, w, h, bar.bg);

        // Measure each module's width; spacers take the leftover.
        let mut widths: Vec<i32> = Vec::with_capacity(bar.modules.len());
        let mut fixed = 0i32;
        let mut spacers = 0i32;
        for m in &bar.modules {
            let wd = match m {
                BarModule::Separator { size, .. } => *size,
                BarModule::Spacer => {
                    spacers += 1;
                    0
                }
                BarModule::Executor(e) => {
                    measure_text(&mut fs, &e.text, e.size.unwrap_or(font_size), e.family.as_deref())
                        + 2 * e.pad
                }
                BarModule::Tray { size, spacing } => {
                    let isize = if *size > 0 { *size } else { (h - 8).max(8) };
                    tray_count * (isize + spacing)
                }
            };
            if !matches!(m, BarModule::Spacer) {
                fixed += wd;
            }
            widths.push(wd);
        }
        let leftover = (w - fixed).max(0);
        let spacer_w = if spacers > 0 { leftover / spacers } else { 0 };

        let mut cx = 0i32;
        // Click zones recorded during the draw, assigned to bar.hit afterwards
        // (can't borrow bar.hit mutably while iterating bar.modules).
        let mut hits: Vec<HitZone> = Vec::new();
        for (m, wd) in bar.modules.iter().zip(widths.iter()) {
            match m {
                BarModule::Spacer => cx += spacer_w,
                BarModule::Separator { size, color, style } => {
                    match style {
                        SepStyle::Line => {
                            fill_rect(&mut data, w, h, cx + size / 2, h / 4, 1, (h / 2).max(1), *color);
                        }
                        SepStyle::Dot => {
                            fill_rect(&mut data, w, h, cx + size / 2 - 1, h / 2 - 1, 2, 2, *color);
                        }
                        SepStyle::Empty => {} // invisible: just the gap
                    }
                    cx += size;
                }
                BarModule::Executor(e) => {
                    if let Some(bg) = e.bg {
                        fill_rect(&mut data, w, h, cx, 0, *wd, h, bg);
                    }
                    let color = e.fg.unwrap_or(bar.fg);
                    let sz = e.size.unwrap_or(font_size);
                    let pen_y = ((h - sz as i32) / 2).max(0);
                    draw_text(
                        &mut data, w, h, cx + e.pad, pen_y, &e.text, sz, color,
                        e.family.as_deref(), &mut fs, &mut sc,
                    );
                    if e.lclick.is_some() || e.rclick.is_some() {
                        hits.push(HitZone { x0: cx, x1: cx + *wd, kind: HitKind::Exec(e.id) });
                    }
                    cx += *wd;
                }
                BarModule::Tray { size, spacing } => {
                    let isize = if *size > 0 { *size } else { (h - 8).max(8) };
                    let iy = ((h - isize) / 2).max(0);
                    for item in &self.tray_items {
                        match &item.icon {
                            Some(icon) => draw_icon(&mut data, w, h, cx, iy, isize, icon),
                            // No icon resolved: draw a visible placeholder (accent box
                            // + the title's first letter) so the item isn't invisible.
                            None => {
                                fill_rect(&mut data, w, h, cx, iy, isize, isize, (0x4e, 0x9b, 0xcf, 0xff));
                                let ch = item
                                    .title
                                    .chars()
                                    .find(|c| c.is_alphanumeric())
                                    .unwrap_or('?')
                                    .to_uppercase()
                                    .to_string();
                                let lsz = (isize as f32 * 0.6).max(8.0);
                                let lw = measure_text(&mut fs, &ch, lsz, None);
                                draw_text(
                                    &mut data, w, h, cx + (isize - lw) / 2,
                                    iy + ((isize - lsz as i32) / 2).max(0), &ch, lsz,
                                    (0xff, 0xff, 0xff, 0xff), None, &mut fs, &mut sc,
                                );
                            }
                        }
                        hits.push(HitZone {
                            x0: cx,
                            x1: cx + isize + spacing,
                            kind: HitKind::Tray(item.key.clone()),
                        });
                        cx += isize + spacing;
                    }
                }
            }
        }
        bar.hit = hits;

        // Upload as a fresh shm buffer; retire the buffer that's now two frames
        // old (the compositor has had time to release it).
        if let Some((b, _f)) = bar.old.take() {
            b.destroy();
        }
        bar.old = match (bar.buffer.take(), bar.backing.take()) {
            (Some(b), Some(f)) => Some((b, f)),
            _ => None,
        };
        match shm_file(&data) {
            Ok(file) => {
                let pool = shm.create_pool(file.as_fd(), data.len() as i32, qh, ());
                let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
                pool.destroy();
                bar.buffer = Some(buffer);
                bar.backing = Some(file);
            }
            Err(e) => {
                eprintln!("sfwm: bar: shm buffer failed: {e}");
                return; // font_system left None → rebuilt next frame (rare path)
            }
        }

        // Commit synced to render_finish (protocol: sync_next_commit, then commit
        // the surface, both before render_finish).
        bar.shell.sync_next_commit();
        if let Some(buf) = &bar.buffer {
            bar.surface.attach(Some(buf), 0, 0);
            bar.surface.damage_buffer(0, 0, w, h);
        }
        bar.surface.commit();
        bar.node.set_position(x, y);
        match top_node {
            Some(n) => bar.node.place_above(n),
            None => bar.node.place_top(),
        }

        self.font_system = Some(fs);
        self.swash_cache = Some(sc);
    }

    /// Route a pointer button press over the bar (at surface-local x `lx`) to
    /// whatever module is under the cursor: an executor's `lclick`/`rclick` shell
    /// command, or a tray icon's SNI action (left = Activate, middle =
    /// SecondaryActivate, right = open the dbusmenu overlay).
    fn bar_click(&mut self, lx: i32, button: u32) {
        const BTN_RIGHT: u32 = 0x111;
        const BTN_MIDDLE: u32 = 0x112;
        let Some(bar) = self.bar.as_ref() else { return };
        let (origin, height) = (bar.origin, bar.height);
        let Some(zone) = bar.hit.iter().find(|z| lx >= z.x0 && lx < z.x1) else {
            eprintln!("sfwm: bar: click at x={lx} hit no module ({} zones)", bar.hit.len());
            return;
        };
        eprintln!("sfwm: bar: click x={lx} button={button} → zone {}..{}", zone.x0, zone.x1);
        let zx0 = zone.x0;
        let kind = match &zone.kind {
            HitKind::Exec(id) => HitKind::Exec(*id),
            HitKind::Tray(k) => HitKind::Tray(k.clone()),
        };
        match kind {
            HitKind::Exec(id) => {
                let cmd = bar.modules.iter().find_map(|m| match m {
                    BarModule::Executor(e) if e.id == id => {
                        if button == BTN_RIGHT {
                            e.rclick.clone()
                        } else {
                            e.lclick.clone()
                        }
                    }
                    _ => None,
                });
                if let Some(cmd) = cmd {
                    let _ = std::process::Command::new("sh").arg("-c").arg(cmd).spawn();
                }
            }
            HitKind::Tray(key) => {
                let Some(tx) = self.tray_cmd.as_ref() else { return };
                // Menu-only items (ItemIsMenu) have no useful Activate — left-click
                // should raise their menu, same as a right-click.
                let is_menu = self
                    .tray_items
                    .iter()
                    .find(|i| i.key == key)
                    .is_some_and(|i| i.is_menu);
                // The icon's screen position anchors the item's menu (SNI convention):
                // just below the icon, at its left edge.
                let (x, y) = (origin.0 + zx0, origin.1 + height);
                let cmd = match button {
                    // Right-click (and left-click on menu-only items) opens the
                    // dbusmenu overlay; the tray thread fetches it and replies with
                    // TrayEvent::Menu for us to draw.
                    BTN_RIGHT => tray::TrayCmd::OpenMenu { key, x, y },
                    BTN_MIDDLE => tray::TrayCmd::SecondaryActivate { key, x, y },
                    _ if is_menu => tray::TrayCmd::OpenMenu { key, x, y },
                    _ => tray::TrayCmd::Activate { key, x, y }, // BTN_LEFT
                };
                let _ = tx.send(cmd);
            }
        }
    }

    /// Route a scroll over the bar to the tray icon under the cursor (SNI `Scroll`).
    fn bar_scroll(&mut self, axis: wl_pointer::Axis, value: f64) {
        if value == 0.0 {
            return;
        }
        let lx = self.bar_pointer.0;
        let key = self.bar.as_ref().and_then(|bar| {
            bar.hit.iter().find(|z| lx >= z.x0 && lx < z.x1).and_then(|z| match &z.kind {
                HitKind::Tray(k) => Some(k.clone()),
                _ => None,
            })
        });
        if let (Some(key), Some(tx)) = (key, self.tray_cmd.as_ref()) {
            let horizontal = matches!(axis, wl_pointer::Axis::HorizontalScroll);
            // SNI Scroll wants a small integer delta; sign follows the scroll direction.
            let delta = if value > 0.0 { 1 } else { -1 };
            let _ = tx.send(tray::TrayCmd::Scroll { key, delta, horizontal });
        }
    }

    /// Apply a tray item add/change/remove from the D-Bus thread, then redraw.
    fn handle_tray_event(&mut self, ev: tray::TrayEvent) {
        match ev {
            tray::TrayEvent::Upsert(item) => {
                if let Some(existing) = self.tray_items.iter_mut().find(|i| i.key == item.key) {
                    *existing = item;
                } else {
                    self.tray_items.push(item);
                }
            }
            tray::TrayEvent::Remove(key) => self.tray_items.retain(|i| i.key != key),
            tray::TrayEvent::Menu { key, x, y, items } => {
                self.open_tray_menu(key, items, (x, y));
                return; // open_tray_menu already requested a manage
            }
        }
        self.request_manage();
    }

    /// Open (or replace) the tray context-menu overlay for `key`, anchored at the
    /// global point `anchor` (just below the clicked icon). An empty menu is
    /// ignored. Mirrors the launcher's fullscreen-surface setup.
    fn open_tray_menu(&mut self, key: String, items: Vec<tray::MenuNode>, anchor: (i32, i32)) {
        self.close_tray_menu();
        if items.iter().all(|n| !n.visible) {
            return;
        }
        let (Some(comp), Some(wm)) = (self.compositor.clone(), self.wm.clone()) else {
            return;
        };
        // Anchor to whichever monitor contains the icon (fall back to the first).
        let mon = self
            .monitors
            .list
            .iter()
            .find(|m| {
                anchor.0 >= m.rect.x
                    && anchor.0 < m.rect.x + m.rect.w
                    && anchor.1 >= m.rect.y
                    && anchor.1 < m.rect.y + m.rect.h
            })
            .or_else(|| self.monitors.list.first());
        let Some(mon_rect) = mon.map(|m| m.rect) else {
            return;
        };
        let qh = self.qh.clone();
        let surface = comp.create_surface(&qh, ());
        let shell = wm.get_shell_surface(&surface, &qh, ());
        let node = shell.get_node(&qh, ());
        self.tray_menu = Some(TrayMenu {
            surface,
            shell,
            node,
            buffer: None,
            backing: None,
            old: None,
            key,
            root: items,
            anchor,
            mon: mon_rect,
            open_path: Vec::new(),
            hover: None,
            columns: Vec::new(),
            last_sig: None,
        });
        self.request_manage();
    }

    /// Tear down the tray menu overlay (no selection).
    fn close_tray_menu(&mut self) {
        if let Some(m) = self.tray_menu.take() {
            drop_launcher_surface(m.surface, m.shell, m.node, m.buffer, m.old);
            self.pointer_over_menu = false;
            self.request_manage();
        }
    }

    /// Pointer moved over the menu overlay: update the hover highlight and open or
    /// collapse submenu columns as the cursor crosses submenu rows (cascade).
    fn menu_pointer_moved(&mut self, sx: i32, sy: i32) {
        self.menu_pointer = (sx, sy);
        let Some(menu) = self.tray_menu.as_ref() else { return };
        let hit = menu_hit(&menu.columns, sx, sy);
        let mut new_open = menu.open_path.clone();
        if let Some((col, row)) = hit {
            let cols = menu.columns_nodes();
            let has_sub = cols
                .get(col)
                .and_then(|c| c.get(row))
                .map(|n| n.has_submenu && !n.is_separator)
                .unwrap_or(false);
            new_open.truncate(col);
            if has_sub {
                new_open.push(row);
            }
        }
        let changed = hit != menu.hover || new_open != menu.open_path;
        if changed {
            let menu = self.tray_menu.as_mut().unwrap();
            menu.hover = hit;
            menu.open_path = new_open;
            self.request_manage();
        }
    }

    /// A click landed on the menu overlay at surface-local (`sx`,`sy`): activate a
    /// leaf row, open a submenu row, or (outside every column) dismiss the menu.
    fn menu_click(&mut self, sx: i32, sy: i32) {
        let Some(menu) = self.tray_menu.as_ref() else { return };
        let Some((col, row)) = menu_hit(&menu.columns, sx, sy) else {
            self.close_tray_menu(); // clicked off the menu → dismiss
            return;
        };
        let cols = menu.columns_nodes();
        let Some(node) = cols.get(col).and_then(|c| c.get(row)) else {
            return;
        };
        let (id, enabled, is_sep, has_sub) =
            (node.id, node.enabled, node.is_separator, node.has_submenu);
        let key = menu.key.clone();
        if is_sep || !enabled {
            return;
        }
        if has_sub {
            let menu = self.tray_menu.as_mut().unwrap();
            menu.open_path.truncate(col);
            menu.open_path.push(row);
            self.request_manage();
            return;
        }
        if let Some(tx) = self.tray_cmd.as_ref() {
            let _ = tx.send(tray::TrayCmd::MenuClicked { key, id });
        }
        self.close_tray_menu();
    }

    /// Keyboard while the menu is open: Escape dismisses it (mouse drives the rest).
    fn menu_key(&mut self, sym: u32) {
        use xkbcommon::xkb::keysyms as ks;
        if sym == ks::KEY_Escape {
            self.close_tray_menu();
        }
    }

    /// Draw the tray context menu overlay: a transparent fullscreen surface with
    /// one or more cascading opaque columns. Rebuilds the buffer only when the
    /// size/anchor/open-path/hover changes. Runs inside the render sequence.
    fn render_menu(&mut self, qh: &QueueHandle<Self>) {
        if self.tray_menu.is_none() {
            return;
        }
        let Some(shm) = self.shm.clone() else { return };

        // Palette (kept in step with the launcher overlay).
        const BG: (u8, u8, u8, u8) = (0x1d, 0x25, 0x2b, 0xff);
        const FG: (u8, u8, u8, u8) = (0xf7, 0xf8, 0xf3, 0xff);
        const SEL_BG: (u8, u8, u8, u8) = (0x4e, 0x9b, 0xcf, 0xff);
        const SEL_FG: (u8, u8, u8, u8) = (0xff, 0xff, 0xff, 0xff);
        const DIS_FG: (u8, u8, u8, u8) = (0xf7, 0xf8, 0xf3, 0x66);
        const SEP: (u8, u8, u8, u8) = (0xff, 0xff, 0xff, 0x22);
        const BORDER: (u8, u8, u8, u8) = (0x0a, 0x0c, 0x0f, 0xff);

        const ROW: i32 = 26;
        const SEPH: i32 = 7;
        const PADX: i32 = 10;
        const TOGW: i32 = 18; // toggle/checkbox gutter
        const ICONW: i32 = 20; // icon gutter (columns with any row icon)
        const ARROW: i32 = 16; // submenu-arrow gutter
        const FONT: f32 = 14.0;
        const MINW: i32 = 150;
        const MAXW: i32 = 460;

        let mon = self.tray_menu.as_ref().unwrap().mon;
        let w = mon.w.max(1);
        let h = mon.h.max(1);
        let ax = self.tray_menu.as_ref().unwrap().anchor.0 - mon.x;
        let ay = self.tray_menu.as_ref().unwrap().anchor.1 - mon.y;

        if self.font_system.is_none() {
            self.font_system = Some(cosmic_text::FontSystem::new());
            self.swash_cache = Some(cosmic_text::SwashCache::new());
        }
        let mut fs = self.font_system.take().unwrap();
        let mut sc = self.swash_cache.take().unwrap();

        // --- lay out the cascade columns (surface-local geometry) ---
        let cols_nodes = self.tray_menu.as_ref().unwrap().columns_nodes();
        let open_path = self.tray_menu.as_ref().unwrap().open_path.clone();
        let mut columns: Vec<MenuColumn> = Vec::with_capacity(cols_nodes.len());
        for (ci, nodes) in cols_nodes.iter().enumerate() {
            let has_icon = nodes.iter().any(|n| n.icon.is_some());
            let mut textw = 0;
            for n in nodes.iter() {
                if !n.is_separator {
                    textw = textw.max(measure_text(&mut fs, &n.label, FONT, None));
                }
            }
            let iconw = if has_icon { ICONW } else { 0 };
            let colw = (PADX + TOGW + iconw + textw + ARROW + PADX).clamp(MINW, MAXW);
            let colh: i32 = nodes
                .iter()
                .map(|n| if n.is_separator { SEPH } else { ROW })
                .sum::<i32>()
                .max(ROW);

            let (cx, cy) = if ci == 0 {
                (ax.min(w - colw).max(0), ay.min(h - colh).max(0))
            } else {
                let parent = &columns[ci - 1];
                let py0 = open_path
                    .get(ci - 1)
                    .and_then(|&r| parent.rows.get(r))
                    .map(|r| r.y0)
                    .unwrap_or(parent.y);
                let mut x = parent.x + parent.w;
                if x + colw > w {
                    x = parent.x - colw; // no room right → flip to the left
                }
                let x = x.max(0).min((w - colw).max(0));
                (x, py0.min(h - colh).max(0))
            };

            let mut rows = Vec::with_capacity(nodes.len());
            let mut ry = cy;
            for n in nodes.iter() {
                let rh = if n.is_separator { SEPH } else { ROW };
                rows.push(MenuRow { y0: ry, y1: ry + rh });
                ry += rh;
            }
            columns.push(MenuColumn { x: cx, y: cy, w: colw, h: colh, rows });
        }

        let hover = self.tray_menu.as_ref().unwrap().hover;
        let sig = {
            use std::hash::{Hash, Hasher};
            let mut hs = std::collections::hash_map::DefaultHasher::new();
            (w, h, ax, ay).hash(&mut hs);
            open_path.hash(&mut hs);
            hover.hash(&mut hs);
            self.tray_menu.as_ref().unwrap().root.len().hash(&mut hs);
            hs.finish()
        };

        if self.tray_menu.as_ref().unwrap().last_sig != Some(sig) {
            let mut data = vec![0u8; w as usize * h as usize * 4]; // transparent backdrop
            for (ci, col) in columns.iter().enumerate() {
                let nodes = &cols_nodes[ci];
                let has_icon = nodes.iter().any(|n| n.icon.is_some());
                let iconw = if has_icon { ICONW } else { 0 };
                // border + background box
                fill_rect(&mut data, w, h, col.x - 1, col.y - 1, col.w + 2, col.h + 2, BORDER);
                fill_rect(&mut data, w, h, col.x, col.y, col.w, col.h, BG);

                for (ri, node) in nodes.iter().enumerate() {
                    let row = &col.rows[ri];
                    if node.is_separator {
                        let sy = row.y0 + (row.y1 - row.y0) / 2;
                        fill_rect(&mut data, w, h, col.x + PADX, sy, col.w - 2 * PADX, 1, SEP);
                        continue;
                    }
                    let on_path = ci < open_path.len() && open_path[ci] == ri;
                    let highlight = node.enabled && (hover == Some((ci, ri)) || on_path);
                    let fg = if !node.enabled {
                        DIS_FG
                    } else if highlight {
                        SEL_FG
                    } else {
                        FG
                    };
                    if highlight {
                        fill_rect(&mut data, w, h, col.x, row.y0, col.w, ROW, SEL_BG);
                    }
                    // toggle indicator
                    if node.toggle_type != 0 {
                        draw_toggle(
                            &mut data,
                            w,
                            h,
                            col.x + PADX,
                            row.y0 + (ROW - 12) / 2,
                            node.toggle_state == 1,
                            fg,
                        );
                    }
                    // row icon
                    if has_icon {
                        if let Some(icon) = &node.icon {
                            let isz = ROW - 8;
                            draw_icon(
                                &mut data,
                                w,
                                h,
                                col.x + PADX + TOGW,
                                row.y0 + (ROW - isz) / 2,
                                isz,
                                icon,
                            );
                        }
                    }
                    // label
                    draw_text(
                        &mut data,
                        w,
                        h,
                        col.x + PADX + TOGW + iconw,
                        row.y0 + (ROW - 15) / 2,
                        &node.label,
                        FONT,
                        fg,
                        None,
                        &mut fs,
                        &mut sc,
                    );
                    // submenu arrow (a small right-pointing triangle)
                    if node.has_submenu {
                        let tx = col.x + col.w - PADX - 5;
                        let tcy = row.y0 + ROW / 2;
                        for i in 0..5 {
                            let half = 4 - i;
                            fill_rect(&mut data, w, h, tx + i, tcy - half, 1, 2 * half + 1, fg);
                        }
                    }
                }
            }
            premultiply(&mut data);

            let menu = self.tray_menu.as_mut().unwrap();
            if let Some((b, _)) = menu.old.take() {
                b.destroy();
            }
            menu.old = match (menu.buffer.take(), menu.backing.take()) {
                (Some(b), Some(f)) => Some((b, f)),
                _ => None,
            };
            match shm_file(&data) {
                Ok(file) => {
                    let pool = shm.create_pool(file.as_fd(), data.len() as i32, qh, ());
                    let buffer =
                        pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
                    pool.destroy();
                    menu.shell.sync_next_commit();
                    menu.surface.attach(Some(&buffer), 0, 0);
                    menu.surface.damage_buffer(0, 0, w, h);
                    menu.surface.commit();
                    menu.buffer = Some(buffer);
                    menu.backing = Some(file);
                    menu.last_sig = Some(sig);
                }
                Err(e) => eprintln!("sfwm: tray: menu shm buffer failed: {e}"),
            }
        }

        self.font_system = Some(fs);
        self.swash_cache = Some(sc);

        let menu = self.tray_menu.as_mut().unwrap();
        menu.columns = columns;
        menu.node.set_position(mon.x, mon.y);
        menu.node.place_top();
    }

    /// Update an executor module's displayed text (from its worker thread, via
    /// the bar channel) and trigger a redraw.
    fn set_bar_module_text(&mut self, id: u64, text: String) {
        let mut found = false;
        if let Some(bar) = self.bar.as_mut() {
            for m in &mut bar.modules {
                if let BarModule::Executor(e) = m {
                    if e.id == id {
                        e.text = text;
                        found = true;
                        break;
                    }
                }
            }
        }
        if found {
            self.request_manage();
        }
    }

    /// Start a bar executor's worker thread. It runs the command off the main
    /// thread and pushes output over `bar_tx`, so a slow command never blocks the
    /// WM. Returns the child (continuous mode) so teardown can kill it.
    fn spawn_executor(
        &self,
        id: u64,
        cmd: String,
        mode: ExecMode,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<std::process::Child> {
        use std::process::{Command, Stdio};
        use std::sync::atomic::Ordering;
        let tx = self.bar_tx.clone()?;
        match mode {
            ExecMode::Continuous => match Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .spawn()
            {
                Ok(mut child) => {
                    if let Some(out) = child.stdout.take() {
                        std::thread::spawn(move || {
                            use std::io::BufRead;
                            for line in std::io::BufReader::new(out).lines() {
                                match line {
                                    Ok(l) => {
                                        if tx.send((id, l)).is_err() {
                                            break;
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                        });
                    }
                    Some(child)
                }
                Err(e) => {
                    eprintln!("sfwm: bar executor spawn failed: {e}");
                    None
                }
            },
            ExecMode::Interval(secs) => {
                std::thread::spawn(move || loop {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Ok(out) = Command::new("sh")
                        .arg("-c")
                        .arg(&cmd)
                        .stdin(Stdio::null())
                        .output()
                    {
                        let text = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
                        if tx.send((id, text)).is_err() {
                            break;
                        }
                    }
                    if secs == 0 {
                        break; // run-once
                    }
                    for _ in 0..secs {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                });
                None
            }
        }
    }

    /// Stop every bar executor (kill children / signal threads) and drop modules.
    fn teardown_bar_modules(&mut self) {
        if let Some(bar) = self.bar.as_mut() {
            for m in &mut bar.modules {
                if let BarModule::Executor(e) = m {
                    e.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    if let Some(child) = e.child.as_mut() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                }
            }
            bar.modules.clear();
        }
    }

    /// Fully remove the bar: stop its executors AND destroy its Wayland objects
    /// (node, shell surface, surface, buffers). Wayland proxies are NOT freed on
    /// drop, so without this an `sc bar create` over an existing bar would leak
    /// the old shell surface/node — river keeps compositing a ghost of it.
    fn destroy_bar(&mut self) {
        self.teardown_bar_modules();
        if let Some(bar) = self.bar.take() {
            if let Some(b) = bar.buffer {
                b.destroy();
            }
            if let Some((b, _)) = bar.old {
                b.destroy();
            }
            bar.node.destroy();
            bar.shell.destroy();
            bar.surface.destroy();
        }
    }

    /// Draw each monitor's wallpaper and return their nodes (bottom-most). Rebuilds
    /// the shm buffer (decode + scale) only when the monitor size or content
    /// changed; otherwise just re-places the existing surface. Runs inside the
    /// render sequence (from `do_render`, before `render_finish`).
    fn render_wallpaper(&mut self, qh: &QueueHandle<Self>) -> Vec<RiverNodeV1> {
        let mut nodes = Vec::new();
        if self.wallpapers.is_empty() {
            return nodes;
        }
        let Some(shm) = self.shm.clone() else {
            return nodes;
        };
        // Snapshot monitor rects so we don't borrow self.monitors while mutating
        // self.wallpapers.
        let rects: HashMap<usize, Rect> = self
            .monitors
            .list
            .iter()
            .enumerate()
            .map(|(i, m)| (i, m.rect))
            .collect();
        for (idx, wp) in self.wallpapers.iter_mut() {
            let Some(rect) = rects.get(idx).copied() else {
                continue;
            };
            let w = rect.w.max(1);
            let h = rect.h.max(1);
            let sig = (w, h, content_sig(&wp.content));
            if wp.last_sig != Some(sig) {
                let data = render_wallpaper_pixels(&wp.content, w, h);
                if let Some((b, _)) = wp.old.take() {
                    b.destroy();
                }
                wp.old = match (wp.buffer.take(), wp.backing.take()) {
                    (Some(b), Some(f)) => Some((b, f)),
                    _ => None,
                };
                match shm_file(&data) {
                    Ok(file) => {
                        let pool = shm.create_pool(file.as_fd(), data.len() as i32, qh, ());
                        let buffer =
                            pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
                        pool.destroy();
                        wp.shell.sync_next_commit();
                        wp.surface.attach(Some(&buffer), 0, 0);
                        wp.surface.damage_buffer(0, 0, w, h);
                        wp.surface.commit();
                        wp.buffer = Some(buffer);
                        wp.backing = Some(file);
                        wp.last_sig = Some(sig);
                    }
                    Err(e) => {
                        eprintln!("sfwm: wallpaper: shm buffer failed: {e}");
                        continue;
                    }
                }
            }
            wp.node.set_position(rect.x, rect.y);
            nodes.push(wp.node.clone());
        }
        nodes
    }

    /// Set the wallpaper on each of `mons`. If a monitor already has one, REUSE
    /// its surface and just swap the content (same reason as the bar: a freshly
    /// re-created shell surface isn't reliably composited until a full render, so
    /// destroy+recreate made the wallpaper vanish after `sc reload`). The buffer
    /// rebuilds on the next render (`last_sig = None`).
    fn set_wallpaper(&mut self, mons: &[usize], content: WallpaperContent) {
        let (Some(comp), Some(wm)) = (self.compositor.clone(), self.wm.clone()) else {
            return;
        };
        let qh = self.qh.clone();
        for &m in mons {
            if let Some(wp) = self.wallpapers.get_mut(&m) {
                wp.content = content.clone();
                wp.last_sig = None; // force a redraw on the reused surface
            } else {
                let surface = comp.create_surface(&qh, ());
                let shell = wm.get_shell_surface(&surface, &qh, ());
                let node = shell.get_node(&qh, ());
                self.wallpapers.insert(
                    m,
                    Wallpaper {
                        surface,
                        shell,
                        node,
                        content: content.clone(),
                        buffer: None,
                        backing: None,
                        old: None,
                        last_sig: None,
                    },
                );
            }
        }
    }

    /// Tear down the wallpaper on `mon` (or all monitors when `None`), destroying
    /// every Wayland object (proxies aren't freed on Drop — same rule as the bar).
    fn destroy_wallpaper(&mut self, mon: Option<usize>) {
        let targets: Vec<usize> = match mon {
            Some(m) => vec![m],
            None => self.wallpapers.keys().copied().collect(),
        };
        for m in targets {
            if let Some(wp) = self.wallpapers.remove(&m) {
                if let Some(b) = wp.buffer {
                    b.destroy();
                }
                if let Some((b, _)) = wp.old {
                    b.destroy();
                }
                wp.node.destroy();
                wp.shell.destroy();
                wp.surface.destroy();
            }
        }
    }

    /// Handle a notification event from the D-Bus thread (show or close a popup).
    fn handle_notif_event(&mut self, ev: notify::NotifEvent) {
        match ev {
            notify::NotifEvent::Show {
                id,
                summary,
                body,
                urgency,
                expire_timeout,
                ..
            } => {
                // replaces_id reuse: drop any existing popup with this id first.
                self.close_notification(id);
                let (Some(comp), Some(wm)) = (self.compositor.clone(), self.wm.clone()) else {
                    return;
                };
                let qh = self.qh.clone();
                let surface = comp.create_surface(&qh, ());
                let shell = wm.get_shell_surface(&surface, &qh, ());
                let node = shell.get_node(&qh, ());
                self.notifications.push(Notification {
                    id,
                    summary,
                    body,
                    urgency,
                    surface,
                    shell,
                    node,
                    buffer: None,
                    backing: None,
                    height: 0,
                });
                // Keep at most 5 on screen; evict the oldest.
                while self.notifications.len() > 5 {
                    let old = self.notifications.remove(0);
                    Self::destroy_notification_objs(old);
                }
                // Expiry: -1 = the themed default, 0 = sticky, else the given ms.
                let ms = if expire_timeout < 0 {
                    self.notif_theme.timeout_ms.max(0) as u64
                } else {
                    expire_timeout as u64
                };
                if ms > 0 {
                    if let Some(h) = self.loop_handle.clone() {
                        let timer =
                            calloop::timer::Timer::from_duration(std::time::Duration::from_millis(ms));
                        let _ = h.insert_source(timer, move |_, _, state: &mut State| {
                            state.close_notification(id);
                            calloop::timer::TimeoutAction::Drop
                        });
                    }
                }
                self.request_manage();
            }
            notify::NotifEvent::Close(id) => self.close_notification(id),
        }
    }

    /// Remove a popup by id (expiry or explicit CloseNotification) and redraw.
    fn close_notification(&mut self, id: u32) {
        if let Some(pos) = self.notifications.iter().position(|n| n.id == id) {
            let n = self.notifications.remove(pos);
            Self::destroy_notification_objs(n);
            self.request_manage();
        }
    }

    fn destroy_notification_objs(n: Notification) {
        if let Some(b) = n.buffer {
            b.destroy();
        }
        n.node.destroy();
        n.shell.destroy();
        n.surface.destroy();
    }

    /// Draw the notification popups, stacked top-right on the focused monitor,
    /// above the bar. Each popup's buffer is built once (contents are static);
    /// later frames only re-place it as popups above it expire. Runs inside the
    /// render sequence (from `do_render`, before `render_finish`).
    fn render_notifications(&mut self, qh: &QueueHandle<Self>) {
        if self.notifications.is_empty() {
            return;
        }
        let Some(shm) = self.shm.clone() else {
            return;
        };
        let mon = self
            .monitors
            .list
            .get(self.monitors.focus)
            .or_else(|| self.monitors.list.first());
        let Some(rect) = mon.map(|m| m.rect) else {
            return;
        };
        if self.font_system.is_none() {
            self.font_system = Some(cosmic_text::FontSystem::new());
            self.swash_cache = Some(cosmic_text::SwashCache::new());
        }
        let mut fs = self.font_system.take().unwrap();
        let mut sc = self.swash_cache.take().unwrap();
        let theme = self.notif_theme;

        let w_box = theme.width.max(120);
        const PAD: i32 = 12;
        const ACCENT: i32 = 4;
        const MARGIN: i32 = 12;
        const GAP: i32 = 8;
        const SUMMARY: f32 = 15.0;
        const BODY: f32 = 13.0;
        let wrap_w = (w_box - 2 * PAD - ACCENT) as f32;

        // Build any popup that hasn't been rasterized yet (also sets its height).
        for n in self.notifications.iter_mut() {
            if n.buffer.is_some() {
                continue;
            }
            let (_, body_lines) = measure_wrapped(&mut fs, &n.body, BODY, wrap_w);
            let summary_h = (SUMMARY * 1.35).ceil() as i32;
            let body_h = if n.body.is_empty() {
                0
            } else {
                (body_lines as f32 * (BODY * 1.35)).ceil() as i32
            };
            let gap = if n.body.is_empty() { 0 } else { 6 };
            let h = (PAD + summary_h + gap + body_h + PAD).max(40);

            let mut data = vec![0u8; w_box as usize * h as usize * 4];
            fill_rect(&mut data, w_box, h, 0, 0, w_box, h, theme.bg);
            let accent = if n.urgency == 2 {
                theme.accent_critical
            } else {
                theme.accent
            };
            fill_rect(&mut data, w_box, h, 0, 0, ACCENT, h, accent);
            let tx0 = PAD + ACCENT;
            draw_text(
                &mut data, w_box, h, tx0, PAD, &n.summary, SUMMARY, theme.fg, None, &mut fs, &mut sc,
            );
            if !n.body.is_empty() {
                draw_wrapped(
                    &mut data, w_box, h, tx0, PAD + summary_h + gap, &n.body, BODY,
                    theme.body_fg, wrap_w, &mut fs, &mut sc,
                );
            }
            match shm_file(&data) {
                Ok(file) => {
                    let pool = shm.create_pool(file.as_fd(), data.len() as i32, qh, ());
                    let buffer = pool.create_buffer(
                        0, w_box, h, w_box * 4, wl_shm::Format::Argb8888, qh, (),
                    );
                    pool.destroy();
                    n.shell.sync_next_commit();
                    n.surface.attach(Some(&buffer), 0, 0);
                    n.surface.damage_buffer(0, 0, w_box, h);
                    n.surface.commit();
                    n.buffer = Some(buffer);
                    n.backing = Some(file);
                    n.height = h;
                }
                Err(e) => eprintln!("sfwm: notification: shm buffer failed: {e}"),
            }
        }

        // Stack top-right, below a top-anchored bar.
        let mut top = MARGIN;
        if let Some(b) = &self.bar {
            if matches!(b.anchor, DockAnchor::Top) {
                top += b.margin_y * 2 + b.height;
            }
        }
        let x = rect.x + rect.w - w_box - MARGIN;
        let mut y = rect.y + top;
        for n in self.notifications.iter() {
            n.node.set_position(x, y);
            n.node.place_top();
            y += n.height + GAP;
        }

        self.font_system = Some(fs);
        self.swash_cache = Some(sc);
    }

    /// Open the app launcher (`sc launcher`), or toggle it closed if already open.
    fn open_launcher(&mut self) {
        if self.launcher.is_some() {
            self.close_launcher();
            return;
        }
        if self.apps.is_empty() {
            self.apps = launcher::enumerate_apps();
        }
        let entries: Vec<String> = self.apps.iter().map(|a| a.name.clone()).collect();
        let execs: Vec<String> = self.apps.iter().map(|a| a.exec.clone()).collect();
        self.open_launcher_with(entries, LauncherAction::Apps(execs));
    }

    /// Open the launcher in dmenu mode (`sc menu`): the chosen entry is written
    /// back to `stream`; Esc/no-match writes nothing (client sees EOF → exit 1).
    fn open_launcher_menu(&mut self, items: Vec<String>, stream: UnixStream) {
        self.close_launcher(); // a new menu replaces any open launcher
        self.open_launcher_with(items, LauncherAction::Menu(stream));
    }

    fn open_launcher_with(&mut self, entries: Vec<String>, action: LauncherAction) {
        let (Some(comp), Some(wm)) = (self.compositor.clone(), self.wm.clone()) else {
            return;
        };
        let qh = self.qh.clone();
        let surface = comp.create_surface(&qh, ());
        let shell = wm.get_shell_surface(&surface, &qh, ());
        let node = shell.get_node(&qh, ());
        let matches = launcher::filter(&entries, "");
        self.launcher = Some(Launcher {
            surface,
            shell,
            node,
            buffer: None,
            backing: None,
            old: None,
            query: String::new(),
            entries,
            action,
            matches,
            selected: 0,
            scroll: 0,
            last_sig: None,
        });
        self.request_manage();
    }

    /// Close the launcher with no selection. In menu mode the reply stream is
    /// dropped (closed), so the `sc menu` client reads an empty reply.
    fn close_launcher(&mut self) {
        if let Some(l) = self.launcher.take() {
            drop_launcher_surface(l.surface, l.shell, l.node, l.buffer, l.old);
            self.request_manage();
        }
    }

    /// Act on the highlighted row: spawn (apps) or reply to the client (menu),
    /// then close.
    fn launcher_launch(&mut self) {
        use std::io::Write;
        let Some(l) = self.launcher.take() else {
            return;
        };
        let Launcher {
            surface,
            shell,
            node,
            buffer,
            old,
            entries,
            action,
            matches,
            selected,
            ..
        } = l;
        let chosen = matches.get(selected).copied();
        match action {
            LauncherAction::Apps(execs) => {
                if let Some(cmd) = chosen.and_then(|ei| execs.get(ei).cloned()) {
                    let _ = std::process::Command::new("sh").arg("-c").arg(cmd).spawn();
                }
            }
            LauncherAction::Menu(mut stream) => {
                if let Some(ei) = chosen {
                    let _ = writeln!(stream, "{}", entries[ei]);
                    let _ = stream.flush();
                }
                // stream drops here → the client's read completes.
            }
        }
        drop_launcher_surface(surface, shell, node, buffer, old);
        self.request_manage();
    }

    /// Feed a keypress (keysym + its UTF-8 text) into the open launcher.
    fn launcher_key(&mut self, sym: u32, utf8: String) {
        use xkbcommon::xkb::keysyms as ks;
        match sym {
            ks::KEY_Escape => {
                self.close_launcher();
                return;
            }
            ks::KEY_Return | ks::KEY_KP_Enter => {
                self.launcher_launch();
                return;
            }
            _ => {}
        }
        let mut recompute = false;
        {
            let Some(l) = self.launcher.as_mut() else {
                return;
            };
            match sym {
                ks::KEY_BackSpace => {
                    l.query.pop();
                    recompute = true;
                }
                ks::KEY_Up => l.selected = l.selected.saturating_sub(1),
                ks::KEY_Down | ks::KEY_Tab => {
                    if l.selected + 1 < l.matches.len() {
                        l.selected += 1;
                    }
                }
                _ => {
                    for c in utf8.chars() {
                        if !c.is_control() {
                            l.query.push(c);
                            recompute = true;
                        }
                    }
                }
            }
        }
        if recompute {
            if let Some(l) = self.launcher.as_mut() {
                l.matches = launcher::filter(&l.entries, &l.query);
                l.selected = 0;
                l.scroll = 0;
            }
        }
        self.request_manage();
    }

    /// Draw the fullscreen fuzzy launcher on the focused monitor (translucent, on
    /// top of everything). Rebuilds the buffer only when the query/selection/size
    /// changes. Runs inside the render sequence (from `do_render`).
    fn render_launcher(&mut self, qh: &QueueHandle<Self>) {
        if self.launcher.is_none() {
            return;
        }
        let Some(shm) = self.shm.clone() else {
            return;
        };
        let mon = self
            .monitors
            .list
            .get(self.monitors.focus)
            .or_else(|| self.monitors.list.first());
        let Some(rect) = mon.map(|m| m.rect) else {
            return;
        };
        let w = rect.w.max(1);
        let h = rect.h.max(1);

        const QBOX: i32 = 52; // search box height
        const ROW: i32 = 36; // result row height
        let pw = (w - 160).clamp(240, self.launcher_theme.width.max(240));
        let px = (w - pw) / 2;
        let py = h / 6;
        let list_top = py + QBOX + 8;
        let vis = (((h - list_top - 40).max(ROW)) / ROW).max(1) as usize;

        // Keep the selection visible.
        {
            let l = self.launcher.as_mut().unwrap();
            if l.selected < l.scroll {
                l.scroll = l.selected;
            } else if l.selected >= l.scroll + vis {
                l.scroll = l.selected + 1 - vis;
            }
        }

        let sig = {
            use std::hash::{Hash, Hasher};
            let l = self.launcher.as_ref().unwrap();
            let mut hs = std::collections::hash_map::DefaultHasher::new();
            (w, h, l.selected, l.scroll, l.matches.len()).hash(&mut hs);
            l.query.hash(&mut hs);
            hs.finish()
        };

        if self.launcher.as_ref().unwrap().last_sig != Some(sig) {
            if self.font_system.is_none() {
                self.font_system = Some(cosmic_text::FontSystem::new());
                self.swash_cache = Some(cosmic_text::SwashCache::new());
            }
            let mut fs = self.font_system.take().unwrap();
            let mut sc = self.swash_cache.take().unwrap();

            let t = self.launcher_theme;
            let mut data = vec![0u8; w as usize * h as usize * 4];
            fill_rect(&mut data, w, h, 0, 0, w, h, t.dim); // dim backdrop
            fill_rect(&mut data, w, h, px, py, pw, QBOX, t.bg); // search box
            {
                let l = self.launcher.as_ref().unwrap();
                let qy = py + (QBOX - 22) / 2;
                if l.query.is_empty() {
                    let hint = (t.fg.0, t.fg.1, t.fg.2, 0x60); // faded query hint
                    draw_text(
                        &mut data, w, h, px + 16, qy, "Type to search…", 22.0, hint, None,
                        &mut fs, &mut sc,
                    );
                } else {
                    draw_text(
                        &mut data, w, h, px + 16, qy, &l.query, 22.0, t.fg, None, &mut fs, &mut sc,
                    );
                    let cw = measure_text(&mut fs, &l.query, 22.0, None);
                    fill_rect(&mut data, w, h, px + 18 + cw, py + 12, 2, QBOX - 24, t.sel_bg);
                }
                let end = (l.scroll + vis).min(l.matches.len());
                for (row, mi) in (l.scroll..end).enumerate() {
                    let ai = l.matches[mi];
                    let ry = list_top + row as i32 * ROW;
                    let selected = mi == l.selected;
                    let (bg, fg) = if selected {
                        (t.sel_bg, t.sel_fg)
                    } else {
                        (t.bg, t.fg)
                    };
                    fill_rect(&mut data, w, h, px, ry, pw, ROW, bg);
                    draw_text(
                        &mut data, w, h, px + 16, ry + (ROW - 16) / 2, &l.entries[ai], 16.0,
                        fg, None, &mut fs, &mut sc,
                    );
                }
            }
            premultiply(&mut data);

            let l = self.launcher.as_mut().unwrap();
            if let Some((b, _)) = l.old.take() {
                b.destroy();
            }
            l.old = match (l.buffer.take(), l.backing.take()) {
                (Some(b), Some(f)) => Some((b, f)),
                _ => None,
            };
            match shm_file(&data) {
                Ok(file) => {
                    let pool = shm.create_pool(file.as_fd(), data.len() as i32, qh, ());
                    let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
                    pool.destroy();
                    l.shell.sync_next_commit();
                    l.surface.attach(Some(&buffer), 0, 0);
                    l.surface.damage_buffer(0, 0, w, h);
                    l.surface.commit();
                    l.buffer = Some(buffer);
                    l.backing = Some(file);
                    l.last_sig = Some(sig);
                }
                Err(e) => eprintln!("sfwm: launcher: shm buffer failed: {e}"),
            }

            self.font_system = Some(fs);
            self.swash_cache = Some(sc);
        }

        let l = self.launcher.as_mut().unwrap();
        l.node.set_position(rect.x, rect.y);
        l.node.place_top();
    }

    /// Dim inactive windows by attaching a semi-transparent decoration surface
    /// *above* each unfocused, non-fullscreen, visible window (hlwm users do this
    /// with picom; here it's in-WM). Off when `inactive_dim == 0` or the needed
    /// globals are absent. Decoration `set_offset`/`sync_next_commit` are render
    /// state, so this must run inside the render sequence (it does — from
    /// `do_render`, before `render_finish`).
    fn apply_dim(&mut self, qh: &QueueHandle<Self>, layout: &[RenderItem], focused: Option<WinId>) {
        let (Some(comp), Some(vp), Some(spb)) =
            (self.compositor.clone(), self.viewporter.clone(), self.spb.clone())
        else {
            return;
        };
        let dim = self.inactive_dim;

        // Lazily (re)create the shared dim buffer at the current alpha. When it
        // was just rebuilt (e.g. the alpha changed), force every shown overlay to
        // re-attach the new buffer — otherwise an unchanged-size overlay keeps
        // displaying the old one and the new dim level appears to do nothing.
        let rebuilt = dim > 0.0 && self.dim_buffer.is_none();
        if rebuilt {
            let a = (dim.clamp(0.0, 1.0) * u32::MAX as f64) as u32; // premultiplied black
            self.dim_buffer = Some(spb.create_u32_rgba_buffer(0, 0, 0, a, qh, ()));
        }
        let buffer = self.dim_buffer.clone();

        // Per visible window: should it be dimmed, and at what size?
        let mut want: HashMap<WinId, Rect> = HashMap::new();
        if dim > 0.0 {
            for it in layout {
                if it.visible && Some(it.win) != focused && it.layer != Layer::Fullscreen {
                    want.insert(it.win, it.rect);
                }
            }
        }

        let wids: Vec<WinId> = self.windows.keys().copied().collect();
        for wid in wids {
            let target = want.get(&wid).copied();
            let win = match self.windows.get(&wid) {
                Some(w) => w.win.clone(),
                None => continue,
            };
            match target {
                Some(rect) => {
                    let Some(buffer) = buffer.clone() else { continue };
                    // Create the overlay surface/viewport/decoration on first use.
                    if self.windows.get(&wid).and_then(|w| w.dim.as_ref()).is_none() {
                        let surface = comp.create_surface(qh, ());
                        let viewport = vp.get_viewport(&surface, qh, ());
                        let deco = win.get_decoration_above(&surface, qh, ());
                        if let Some(w) = self.windows.get_mut(&wid) {
                            w.dim = Some(DimOverlay { deco, surface, viewport, shown: false, size: (0, 0) });
                        }
                    }
                    if let Some(o) = self.windows.get_mut(&wid).and_then(|w| w.dim.as_mut()) {
                        if !o.shown || o.size != (rect.w, rect.h) || rebuilt {
                            o.surface.attach(Some(&buffer), 0, 0);
                            o.viewport.set_destination(rect.w.max(1), rect.h.max(1));
                            o.surface.damage(0, 0, i32::MAX, i32::MAX);
                            o.deco.set_offset(0, 0);
                            o.deco.sync_next_commit();
                            o.surface.commit();
                            o.shown = true;
                            o.size = (rect.w, rect.h);
                        }
                    }
                }
                None => {
                    // Hide the overlay if it's currently showing.
                    if let Some(o) = self.windows.get_mut(&wid).and_then(|w| w.dim.as_mut()) {
                        if o.shown {
                            o.surface.attach(None, 0, 0);
                            o.deco.sync_next_commit();
                            o.surface.commit();
                            o.shown = false;
                        }
                    }
                }
            }
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
    if let Ok(p) = std::env::var("SOMMERFLUSSWM_SOCKET") {
        return PathBuf::from(p);
    }
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".to_string());
    PathBuf::from(dir).join(format!("sfwm-{display}.sock"))
}

/// Launch the user's `autostart` script, if present, with the socket path in the
/// environment so the `sc` calls inside it connect back to us. Non-fatal if absent.
fn spawn_autostart(sock: &std::path::Path) {
    let path = std::env::var("SOMMERFLUSSWM_CONFIG").map(PathBuf::from).unwrap_or_else(|_| {
        let cfg = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_default();
                PathBuf::from(home).join(".config")
            });
        cfg.join("sommerflusswm").join("autostart")
    });
    // No user config → fall back to the packaged example so a fresh install
    // still comes up with keybinds, bar and wallpaper instead of a black screen.
    let path = if path.exists() {
        path
    } else {
        let fallback = PathBuf::from("/usr/share/sommerflusswm/autostart");
        if !fallback.exists() {
            eprintln!("sfwm: no autostart at {} (skipping)", path.display());
            return;
        }
        eprintln!(
            "sfwm: no autostart at {} — using packaged default {}",
            path.display(),
            fallback.display()
        );
        fallback
    };
    match std::process::Command::new(&path)
        .env("SOMMERFLUSSWM_SOCKET", sock)
        .spawn()
    {
        Ok(_) => eprintln!("sfwm: launched autostart {}", path.display()),
        Err(e) => eprintln!("sfwm: failed to launch autostart {}: {e}", path.display()),
    }
}

/// Fill an axis-aligned rect of a BGRA buffer (`bw`×`bh`) with `(r,g,b,a)`.
fn fill_rect(buf: &mut [u8], bw: i32, bh: i32, x: i32, y: i32, rw: i32, rh: i32, c: (u8, u8, u8, u8)) {
    let (r, g, b, a) = c;
    for yy in y.max(0)..(y + rh).min(bh) {
        for xx in x.max(0)..(x + rw).min(bw) {
            let i = ((yy * bw + xx) * 4) as usize;
            buf[i] = b;
            buf[i + 1] = g;
            buf[i + 2] = r;
            buf[i + 3] = a;
        }
    }
}

/// A stable hash of a wallpaper's content, so the renderer can skip re-decoding
/// when nothing changed (paired with the monitor size in `Wallpaper::last_sig`).
fn content_sig(c: &WallpaperContent) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match c {
        WallpaperContent::Color(col) => {
            0u8.hash(&mut h);
            col.hash(&mut h);
        }
        WallpaperContent::Image { path, mode } => {
            1u8.hash(&mut h);
            path.hash(&mut h);
            (*mode as u8).hash(&mut h);
        }
    }
    h.finish()
}

/// Render a wallpaper's contents into a fresh `w`×`h` BGRA buffer.
fn render_wallpaper_pixels(content: &WallpaperContent, w: i32, h: i32) -> Vec<u8> {
    let mut data = vec![0u8; w as usize * h as usize * 4];
    match content {
        WallpaperContent::Color(c) => fill_rect(&mut data, w, h, 0, 0, w, h, *c),
        WallpaperContent::Image { path, mode } => {
            // Opaque black backdrop behind any letterboxing / transparency.
            fill_rect(&mut data, w, h, 0, 0, w, h, (0, 0, 0, 0xff));
            match image::open(path) {
                Ok(img) => draw_image(&mut data, w, h, &img.to_rgba8(), *mode),
                Err(e) => eprintln!("sfwm: wallpaper: cannot load {}: {e}", path.display()),
            }
        }
    }
    data
}

/// Composite `src` (RGBA) into a `dw`×`dh` BGRA buffer, fitted by `mode`. Output
/// pixels are made opaque (wallpaper is the bottom layer).
fn draw_image(data: &mut [u8], dw: i32, dh: i32, src: &image::RgbaImage, mode: WallMode) {
    use image::imageops::FilterType;
    let (sw, sh) = (src.width() as i32, src.height() as i32);
    if sw == 0 || sh == 0 {
        return;
    }
    // Blit a positioned RGBA image into the BGRA destination at offset (ox, oy).
    fn blit(data: &mut [u8], dw: i32, dh: i32, img: &image::RgbaImage, ox: i32, oy: i32) {
        for (px, py, p) in img.enumerate_pixels() {
            let x = ox + px as i32;
            let y = oy + py as i32;
            if x < 0 || y < 0 || x >= dw || y >= dh {
                continue;
            }
            let i = ((y * dw + x) * 4) as usize;
            data[i] = p[2];
            data[i + 1] = p[1];
            data[i + 2] = p[0];
            data[i + 3] = 0xff;
        }
    }
    match mode {
        WallMode::Stretch => {
            let r = image::imageops::resize(src, dw as u32, dh as u32, FilterType::Triangle);
            blit(data, dw, dh, &r, 0, 0);
        }
        WallMode::Fit | WallMode::Fill => {
            let sx = dw as f32 / sw as f32;
            let sy = dh as f32 / sh as f32;
            let scale = if mode == WallMode::Fit { sx.min(sy) } else { sx.max(sy) };
            let nw = ((sw as f32 * scale).round() as u32).max(1);
            let nh = ((sh as f32 * scale).round() as u32).max(1);
            let r = image::imageops::resize(src, nw, nh, FilterType::Triangle);
            blit(data, dw, dh, &r, (dw - nw as i32) / 2, (dh - nh as i32) / 2);
        }
        WallMode::Center => blit(data, dw, dh, src, (dw - sw) / 2, (dh - sh) / 2),
        WallMode::Tile => {
            let mut oy = 0;
            while oy < dh {
                let mut ox = 0;
                while ox < dw {
                    blit(data, dw, dh, src, ox, oy);
                    ox += sw;
                }
                oy += sh;
            }
        }
    }
}

/// Destroy a launcher's Wayland objects (proxies aren't freed on drop).
fn drop_launcher_surface(
    surface: WlSurface,
    shell: RiverShellSurfaceV1,
    node: RiverNodeV1,
    buffer: Option<WlBuffer>,
    old: Option<(WlBuffer, std::fs::File)>,
) {
    if let Some(b) = buffer {
        b.destroy();
    }
    if let Some((b, _)) = old {
        b.destroy();
    }
    node.destroy();
    shell.destroy();
    surface.destroy();
}

/// Alpha-blend a tray icon (straight-alpha BGRA, `icon.width`×`icon.height`),
/// nearest-neighbor scaled to `size`×`size`, into a BGRA buffer at (`x`,`y`).
/// Nearest-neighbor is fine for the small (~22px) tray icons and keeps it cheap.
fn draw_icon(dst: &mut [u8], dw: i32, dh: i32, x: i32, y: i32, size: i32, icon: &tray::TrayIcon) {
    let (iw, ih) = (icon.width as i32, icon.height as i32);
    if iw <= 0 || ih <= 0 || size <= 0 {
        return;
    }
    for oy in 0..size {
        let sy = oy * ih / size;
        let dy = y + oy;
        if dy < 0 || dy >= dh {
            continue;
        }
        for ox in 0..size {
            let dx = x + ox;
            if dx < 0 || dx >= dw {
                continue;
            }
            let sx = ox * iw / size;
            let si = ((sy * iw + sx) * 4) as usize;
            if si + 3 >= icon.bgra.len() {
                continue;
            }
            let (b, g, r, a) = (icon.bgra[si], icon.bgra[si + 1], icon.bgra[si + 2], icon.bgra[si + 3]);
            let di = ((dy * dw + dx) * 4) as usize;
            blend_px(&mut dst[di..di + 4], (r, g, b), a);
        }
    }
}

/// Which (column, row) of a laid-out tray menu a surface-local point falls on,
/// or None if it's outside every column box (or on inter-row padding).
fn menu_hit(columns: &[MenuColumn], sx: i32, sy: i32) -> Option<(usize, usize)> {
    for (ci, col) in columns.iter().enumerate() {
        if sx >= col.x && sx < col.x + col.w && sy >= col.y && sy < col.y + col.h {
            for (ri, row) in col.rows.iter().enumerate() {
                if sy >= row.y0 && sy < row.y1 {
                    return Some((ci, ri));
                }
            }
            return None;
        }
    }
    None
}

/// Draw a 12×12 checkbox/radio indicator at (`x`,`y`): a box outline, filled in
/// the centre when `on`. Same glyph for check and radio (kept font-free/reliable).
fn draw_toggle(dst: &mut [u8], dw: i32, dh: i32, x: i32, y: i32, on: bool, c: (u8, u8, u8, u8)) {
    const S: i32 = 12;
    fill_rect(dst, dw, dh, x, y, S, 1, c);
    fill_rect(dst, dw, dh, x, y + S - 1, S, 1, c);
    fill_rect(dst, dw, dh, x, y, 1, S, c);
    fill_rect(dst, dw, dh, x + S - 1, y, 1, S, c);
    if on {
        fill_rect(dst, dw, dh, x + 3, y + 3, S - 6, S - 6, c);
    }
}

/// Convert a straight-alpha BGRA buffer to premultiplied alpha (what wl_shm
/// ARGB8888 expects for correct translucency — used by the launcher overlay).
fn premultiply(data: &mut [u8]) {
    for px in data.chunks_exact_mut(4) {
        let a = px[3] as u32;
        px[0] = (px[0] as u32 * a / 255) as u8;
        px[1] = (px[1] as u32 * a / 255) as u8;
        px[2] = (px[2] as u32 * a / 255) as u8;
    }
}

/// Alpha-blend `src` (r,g,b) over one BGRA pixel at coverage `a` (0..=255).
fn blend_px(px: &mut [u8], src: (u8, u8, u8), a: u8) {
    let af = a as f32 / 255.0;
    px[0] = (src.2 as f32 * af + px[0] as f32 * (1.0 - af)) as u8;
    px[1] = (src.1 as f32 * af + px[1] as f32 * (1.0 - af)) as u8;
    px[2] = (src.0 as f32 * af + px[2] as f32 * (1.0 - af)) as u8;
    px[3] = 255;
}

/// Shape one line of text (no wrapping) with system-font fallback.
fn shape_text(
    fs: &mut cosmic_text::FontSystem,
    text: &str,
    font_size: f32,
    family: Option<&str>,
) -> cosmic_text::Buffer {
    use cosmic_text::{Attrs, Buffer, Family, Metrics, Shaping};
    let mut buffer = Buffer::new(fs, Metrics::new(font_size, font_size * 1.3));
    buffer.set_size(fs, Some(100_000.0), Some(font_size * 1.3));
    let mut attrs = Attrs::new();
    if let Some(name) = family {
        attrs = attrs.family(Family::Name(name));
    }
    buffer.set_text(fs, text, attrs, Shaping::Advanced);
    buffer.shape_until_scroll(fs, false);
    buffer
}

/// Shape `text` wrapped to `wrap_w` px wide (multi-line); used by notifications.
fn shape_wrapped(
    fs: &mut cosmic_text::FontSystem,
    text: &str,
    font_size: f32,
    wrap_w: f32,
) -> cosmic_text::Buffer {
    use cosmic_text::{Attrs, Buffer, Metrics, Shaping};
    let mut buffer = Buffer::new(fs, Metrics::new(font_size, font_size * 1.35));
    buffer.set_size(fs, Some(wrap_w), None);
    buffer.set_text(fs, text, Attrs::new(), Shaping::Advanced);
    buffer.shape_until_scroll(fs, false);
    buffer
}

/// (width, line-count) of `text` wrapped to `wrap_w`.
fn measure_wrapped(
    fs: &mut cosmic_text::FontSystem,
    text: &str,
    font_size: f32,
    wrap_w: f32,
) -> (i32, i32) {
    if text.is_empty() {
        return (0, 0);
    }
    let buffer = shape_wrapped(fs, text, font_size, wrap_w);
    let mut w = 0.0f32;
    let mut lines = 0;
    for run in buffer.layout_runs() {
        w = w.max(run.line_w);
        lines += 1;
    }
    (w.ceil() as i32, lines)
}

/// Draw `text` wrapped to `wrap_w`, top-left at `(pen_x, pen_y)`.
#[allow(clippy::too_many_arguments)]
fn draw_wrapped(
    buf: &mut [u8],
    bw: i32,
    bh: i32,
    pen_x: i32,
    pen_y: i32,
    text: &str,
    font_size: f32,
    color: (u8, u8, u8, u8),
    wrap_w: f32,
    fs: &mut cosmic_text::FontSystem,
    sc: &mut cosmic_text::SwashCache,
) {
    if text.is_empty() {
        return;
    }
    let buffer = shape_wrapped(fs, text, font_size, wrap_w);
    let tc = cosmic_text::Color::rgba(color.0, color.1, color.2, color.3);
    buffer.draw(fs, sc, tc, |gx, gy, gw, gh, col| {
        let a = col.a();
        if a == 0 {
            return;
        }
        for dy in 0..gh as i32 {
            for dx in 0..gw as i32 {
                let px = pen_x + gx + dx;
                let py = pen_y + gy + dy;
                if px < 0 || py < 0 || px >= bw || py >= bh {
                    continue;
                }
                let i = ((py * bw + px) * 4) as usize;
                blend_px(&mut buf[i..i + 4], (col.r(), col.g(), col.b()), a);
            }
        }
    });
}

/// Pixel width of `text` at `font_size` (ceil of the widest layout run).
fn measure_text(
    fs: &mut cosmic_text::FontSystem,
    text: &str,
    font_size: f32,
    family: Option<&str>,
) -> i32 {
    if text.is_empty() {
        return 0;
    }
    shape_text(fs, text, font_size, family)
        .layout_runs()
        .map(|r| r.line_w)
        .fold(0.0_f32, f32::max)
        .ceil() as i32
}

/// Draw `text` into a BGRA buffer at pen `(pen_x, pen_y)`, blending glyph
/// coverage over whatever's already there, in `color`.
#[allow(clippy::too_many_arguments)]
fn draw_text(
    buf: &mut [u8],
    bw: i32,
    bh: i32,
    pen_x: i32,
    pen_y: i32,
    text: &str,
    font_size: f32,
    color: (u8, u8, u8, u8),
    family: Option<&str>,
    fs: &mut cosmic_text::FontSystem,
    sc: &mut cosmic_text::SwashCache,
) {
    if text.is_empty() {
        return;
    }
    let buffer = shape_text(fs, text, font_size, family);
    let tc = cosmic_text::Color::rgba(color.0, color.1, color.2, color.3);
    buffer.draw(fs, sc, tc, |gx, gy, gw, gh, col| {
        let a = col.a();
        if a == 0 {
            return;
        }
        for dy in 0..gh as i32 {
            for dx in 0..gw as i32 {
                let px = pen_x + gx + dx;
                let py = pen_y + gy + dy;
                if px < 0 || py < 0 || px >= bw || py >= bh {
                    continue;
                }
                let i = ((py * bw + px) * 4) as usize;
                blend_px(&mut buf[i..i + 4], (col.r(), col.g(), col.b()), a);
            }
        }
    });
}

/// Create an anonymous, sized, filled backing file for a `wl_shm` pool. The file
/// is unlinked immediately; the returned fd keeps it alive while the compositor
/// has it mapped.
fn shm_file(data: &[u8]) -> std::io::Result<std::fs::File> {
    use std::io::Write;
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    let path = format!("{dir}/sfwm-bar-{}", std::process::id());
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)?;
    let _ = std::fs::remove_file(&path); // unlink; the fd stays valid
    file.write_all(data)?;
    file.flush()?;
    Ok(file)
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

    // Optional globals for the "dim inactive window" overlay. If any is missing,
    // dimming is silently disabled (everything else works unchanged).
    let compositor: Option<WlCompositor> = globals.bind(&qh, 1..=6, ()).ok();
    let viewporter: Option<WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
    let spb: Option<WpSinglePixelBufferManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    let shm: Option<WlShm> = globals.bind(&qh, 1..=1, ()).ok();
    // Bind a wl_seat so we can get a wl_keyboard for the launcher's text input
    // (the WM otherwise never needs raw keyboard — river-xkb-bindings covers keys).
    // The keyboard is fetched in the wl_seat Capabilities event.
    let _wl_seat: Option<WlSeat> = globals.bind(&qh, 1..=8, ()).ok();

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
        gestures: None,
        gesturebinds: HashMap::new(),
        floating_tags: HashSet::new(),
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
        compositor,
        viewporter,
        spb,
        dim_buffer: None,
        shm,
        bar: None,
        wallpapers: HashMap::new(),
        loop_handle: None,
        bar_tx: None,
        font_system: None,
        swash_cache: None,
        next_bar_module: 0,
        inactive_dim: 0.0,
        notifications: Vec::new(),
        notif_theme: NotifTheme::default(),
        wl_keyboard: None,
        xkb_state: None,
        launcher: None,
        launcher_theme: LauncherTheme::default(),
        apps: Vec::new(),
        tray_items: Vec::new(),
        tray_cmd: None,
        tray_menu: None,
        pointer_over_menu: false,
        menu_pointer: (0, 0),
        wl_pointer: None,
        pointer_over_bar: false,
        bar_pointer: (0, 0),
        last_pointer_button: 0x110, // BTN_LEFT
    };

    // --- calloop event loop: Wayland + the IPC socket on one thread ---
    let mut event_loop: EventLoop<'static, State> =
        EventLoop::try_new().expect("failed to create the calloop event loop");
    let handle = event_loop.handle();
    state.loop_handle = Some(handle.clone());

    // Channel for bar-executor worker threads to push output back to the main
    // thread, where it's drawn (so a slow command never blocks the WM).
    let (bar_tx, bar_rx) = calloop::channel::channel::<(u64, String)>();
    state.bar_tx = Some(bar_tx);
    handle
        .insert_source(bar_rx, |event, _, state: &mut State| {
            if let calloop::channel::Event::Msg((id, text)) = event {
                state.set_bar_module_text(id, text);
            }
        })
        .expect("failed to insert the bar channel source");

    // Notifications: a D-Bus thread (org.freedesktop.Notifications) hands popups
    // to the main loop over this channel; the main thread draws them.
    let (notif_tx, notif_rx) = calloop::channel::channel::<notify::NotifEvent>();
    handle
        .insert_source(notif_rx, |event, _, state: &mut State| {
            if let calloop::channel::Event::Msg(ev) = event {
                state.handle_notif_event(ev);
            }
        })
        .expect("failed to insert the notification channel source");
    notify::spawn_notification_service(notif_tx);

    // System tray: an SNI Watcher+Host D-Bus thread reports item add/change/remove
    // over this channel; the main thread stores them and the bar draws their icons.
    // Clicks route back to the thread via the returned command sender.
    let (tray_tx, tray_rx) = calloop::channel::channel::<tray::TrayEvent>();
    handle
        .insert_source(tray_rx, |event, _, state: &mut State| {
            if let calloop::channel::Event::Msg(ev) = event {
                state.handle_tray_event(ev);
            }
        })
        .expect("failed to insert the tray channel source");
    state.tray_cmd = Some(tray::spawn_tray(tray_tx));

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

    // Touchpad gestures: read libinput directly (river doesn't forward
    // gestures to the WM). Non-fatal if unavailable — needs the `input` group.
    match gestures::Gestures::new() {
        Ok(g) => {
            let raw = g.raw_fd();
            state.gestures = Some(g);
            handle
                .insert_source(
                    Generic::new(
                        unsafe { calloop::generic::FdWrapper::new(raw) },
                        Interest::READ,
                        Mode::Level,
                    ),
                    |_readiness, _fd, state: &mut State| {
                        let specs = state.gestures.as_mut().map(|g| g.poll()).unwrap_or_default();
                        for spec in specs {
                            if let Some(cmd) = state.gesturebinds.get(&spec).cloned() {
                                let _ = ipc::dispatch(state, &cmd);
                            }
                        }
                        Ok(PostAction::Continue)
                    },
                )
                .expect("failed to insert the gesture source into the event loop");
            eprintln!("sfwm: touchpad gestures enabled");
        }
        Err(e) => eprintln!("sfwm: touchpad gestures disabled: {e}"),
    }

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
                        if let Some(o) = w.dim {
                            o.deco.destroy();
                            o.viewport.destroy();
                            o.surface.destroy();
                        }
                        crate::attr::drop_client_attrs(state, wid);
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
            // Click on one of our own shell surfaces (the bar). river guarantees this
            // event; wl_pointer to the bar may or may not be delivered, so this is the
            // reliable click trigger. If wl_pointer IS working (pointer_over_bar), that
            // path already routed the click — skip here to avoid a double action.
            Event::ShellSurfaceInteraction { shell_surface } => {
                let ssid = shell_surface.id();
                // Menu first: it's the topmost surface when open.
                let menu_geo = state
                    .tray_menu
                    .as_ref()
                    .filter(|m| m.shell.id() == ssid)
                    .map(|m| m.mon);
                if let Some(mon) = menu_geo {
                    if !state.pointer_over_menu {
                        let (mx, my) = (state.pointer_pos.0 - mon.x, state.pointer_pos.1 - mon.y);
                        state.menu_click(mx, my);
                    }
                    return;
                }
                let is_bar = state
                    .bar
                    .as_ref()
                    .is_some_and(|b| b.shell.id() == ssid);
                eprintln!(
                    "sfwm: bar: shell_surface_interaction is_bar={is_bar} over_bar={} pos={:?}",
                    state.pointer_over_bar, state.pointer_pos
                );
                if is_bar && !state.pointer_over_bar {
                    if let Some(origin) = state.bar.as_ref().map(|b| b.origin) {
                        let lx = state.pointer_pos.0 - origin.0;
                        let button = state.last_pointer_button;
                        state.bar_click(lx, button);
                    }
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

// A standard wl_seat, bound only so the launcher can receive keyboard text.
impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Keyboard) && state.wl_keyboard.is_none() {
                state.wl_keyboard = Some(seat.get_keyboard(qh, ()));
            }
            if caps.contains(wl_seat::Capability::Pointer) && state.wl_pointer.is_none() {
                state.wl_pointer = Some(seat.get_pointer(qh, ()));
            }
        }
    }
}

// wl_pointer: sfwm's own shell surfaces (the bar) receive pointer input directly
// (river routes it, unlike windows which only surface as `window_interaction`), so
// this is how bar executors and tray icons get their clicks. We only act on input
// over the bar surface; everything else (window focus, move/resize) goes through
// river_seat_v1 / pointer bindings.
impl Dispatch<WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wl_pointer::Event;
        match event {
            Event::Enter { surface, surface_x, surface_y, .. } => {
                let sid = surface.id();
                state.pointer_over_menu = state
                    .tray_menu
                    .as_ref()
                    .is_some_and(|m| m.surface.id() == sid);
                state.pointer_over_bar = !state.pointer_over_menu
                    && state.bar.as_ref().is_some_and(|b| b.surface.id() == sid);
                if state.pointer_over_menu {
                    state.menu_pointer_moved(surface_x as i32, surface_y as i32);
                } else if state.pointer_over_bar {
                    state.bar_pointer = (surface_x as i32, surface_y as i32);
                }
            }
            Event::Leave { surface, .. } => {
                let sid = surface.id();
                if state.tray_menu.as_ref().is_some_and(|m| m.surface.id() == sid) {
                    state.pointer_over_menu = false;
                }
                if state.bar.as_ref().is_some_and(|b| b.surface.id() == sid) {
                    state.pointer_over_bar = false;
                }
            }
            Event::Motion { surface_x, surface_y, .. } => {
                if state.pointer_over_menu {
                    state.menu_pointer_moved(surface_x as i32, surface_y as i32);
                } else if state.pointer_over_bar {
                    state.bar_pointer = (surface_x as i32, surface_y as i32);
                }
            }
            Event::Button { button, state: WEnum::Value(wl_pointer::ButtonState::Pressed), .. } => {
                // Remember the button so the river `shell_surface_interaction`
                // fallback (below) can route with the right button if wl_pointer
                // enter/motion isn't being delivered to our shell surface.
                state.last_pointer_button = button;
                if state.pointer_over_menu {
                    let (mx, my) = state.menu_pointer;
                    state.menu_click(mx, my);
                } else if state.pointer_over_bar {
                    eprintln!("sfwm: bar: wl_pointer button={button} over_bar=true");
                    let lx = state.bar_pointer.0;
                    state.bar_click(lx, button);
                }
            }
            Event::Axis { axis: WEnum::Value(axis), value, .. } => {
                if state.pointer_over_bar {
                    state.bar_scroll(axis, value);
                }
            }
            _ => {}
        }
    }
}

// wl_keyboard: only consumed while the launcher is open (river-xkb-bindings handle
// all other keys). Builds xkb state from the keymap, resolves each keypress to a
// keysym + UTF-8, and feeds it to the launcher.
impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wl_keyboard::Event;
        match event {
            Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => {
                use std::io::{Read, Seek, SeekFrom};
                let mut file = std::fs::File::from(fd);
                let mut buf = vec![0u8; size as usize];
                if file.seek(SeekFrom::Start(0)).is_ok() && file.read_exact(&mut buf).is_ok() {
                    while buf.last() == Some(&0) {
                        buf.pop();
                    }
                    if let Ok(s) = String::from_utf8(buf) {
                        let ctx = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
                        if let Some(km) = xkbcommon::xkb::Keymap::new_from_string(
                            &ctx,
                            s,
                            xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1,
                            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
                        ) {
                            state.xkb_state = Some(xkbcommon::xkb::State::new(&km));
                        }
                    }
                }
            }
            Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(xs) = state.xkb_state.as_mut() {
                    xs.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            Event::Key {
                key,
                state: WEnum::Value(wl_keyboard::KeyState::Pressed),
                ..
            } => {
                if state.tray_menu.is_some() || state.launcher.is_some() {
                    if let Some((sym, utf8)) = state.xkb_state.as_ref().map(|xs| {
                        let kc: xkbcommon::xkb::Keycode = (key + 8).into();
                        (xs.key_get_one_sym(kc).raw(), xs.key_get_utf8(kc))
                    }) {
                        // The menu (modal, on top) consumes keys before the launcher.
                        if state.tray_menu.is_some() {
                            state.menu_key(sym);
                        } else {
                            state.launcher_key(sym, utf8);
                        }
                    }
                }
            }
            _ => {}
        }
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

// --- dim-overlay objects: all eventless to us (empty dispatch) ------------------

macro_rules! ignore_events {
    ($($t:ty),+ $(,)?) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )+};
}
ignore_events!(
    WlCompositor,
    WlSurface,
    WlBuffer,
    WpViewporter,
    WpViewport,
    WpSinglePixelBufferManagerV1,
    RiverDecorationV1,
    WlShm,
    WlShmPool,
    RiverShellSurfaceV1,
);

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
