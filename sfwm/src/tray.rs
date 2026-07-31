//! The built-in StatusNotifierItem (SNI) system-tray Watcher + Host — sfwm's
//! answer to having no external tray. Real tray daemons (stalonetray, the panels'
//! own trays) either need X11/XEmbed or draw with wlr-layer-shell, neither of
//! which can composite under sfwm. So the WM speaks the SNI D-Bus protocol itself:
//! it owns `org.kde.StatusNotifierWatcher`, acts as a host, tracks every item that
//! registers, decodes its icon to BGRA, and hands snapshots to the WM's calloop
//! thread — which draws them into the WM-rendered bar with the same engine as the
//! notifications/wallpaper. Clicks/scrolls flow back the other way as `TrayCmd`s.
//!
//! Runs on a dedicated thread driving zbus' async API via `zbus::block_on`. A
//! second small thread owns the command channel and issues item method calls, so
//! the two directions never block each other. Tolerant throughout: no session bus,
//! or the watcher name already taken (another tray is running) → log and bail; the
//! WM keeps running fine without a tray.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use calloop::channel::Sender;
use zbus::export::futures_util::stream::{select_all, StreamExt};
use zbus::fdo::DBusProxy;
use zbus::message::Header;
use zbus::names::BusName;
use zbus::object_server::SignalContext;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, Proxy};

/// Raw dbusmenu layout node as it appears on the wire: (id, properties,
/// children-as-variants). Signature "(ia{sv}av)".
#[derive(serde::Deserialize, zbus::zvariant::Type)]
struct RawLayout(
    #[allow(dead_code)] i32,
    #[allow(dead_code)] HashMap<String, OwnedValue>,
    Vec<OwnedValue>,
);

/// Message from the tray thread to the WM main thread.
pub enum TrayEvent {
    /// An item appeared or changed. Full snapshot each time (KISS — main replaces).
    Upsert(TrayItem),
    /// The item with this key went away.
    Remove(String),
    /// The parsed dbusmenu for an item, ready for the WM to draw as a context
    /// menu overlay. `(x, y)` is the anchor the WM asked to open at.
    Menu {
        key: String,
        x: i32,
        y: i32,
        items: Vec<MenuNode>,
    },
}

/// One entry in a tray item's com.canonical.dbusmenu tree. The WM renders these
/// as cascading columns in a WM-drawn overlay (Stage-2 tray menus).
#[derive(Clone)]
pub struct MenuNode {
    /// dbusmenu item id, echoed back in the `clicked` Event when activated.
    pub id: i32,
    /// Display label, mnemonic underscores stripped.
    pub label: String,
    /// Greyed-out and unclickable when false.
    pub enabled: bool,
    /// Hidden entirely when false.
    pub visible: bool,
    /// A horizontal divider (no label / not clickable).
    pub is_separator: bool,
    /// Has a submenu — the WM opens a child column and never sends `clicked`.
    pub has_submenu: bool,
    /// 0 = none, 1 = checkmark, 2 = radio.
    pub toggle_type: u8,
    /// 1 = on, 0 = off, -1 = indeterminate (only meaningful with a toggle_type).
    pub toggle_state: i32,
    /// Optional row icon (resolved from `icon-name`).
    pub icon: Option<TrayIcon>,
    /// Child entries (populated for submenu rows).
    pub children: Vec<MenuNode>,
}

#[derive(Clone)]
pub struct TrayItem {
    /// Stable unique key = the item's D-Bus unique bus name (e.g. ":1.57").
    pub key: String,
    /// Item title/tooltip. Not surfaced yet (no tray tooltips); kept for later.
    #[allow(dead_code)]
    pub title: String,
    /// SNI Status: "Active" | "Passive" | "NeedsAttention". We render all items
    /// regardless (Passive-hiding made appindicator items invisible); kept for a
    /// possible future NeedsAttention highlight.
    #[allow(dead_code)]
    pub status: String,
    /// Decoded icon, or None if none could be resolved.
    pub icon: Option<TrayIcon>,
    /// ItemIsMenu hint: left-click should show the menu, not Activate.
    pub is_menu: bool,
    /// dbusmenu object path — consumed by the Stage-2 in-WM context-menu overlay.
    #[allow(dead_code)]
    pub menu: Option<String>,
}

#[derive(Clone)]
pub struct TrayIcon {
    pub width: u32,
    pub height: u32,
    /// Row-major BGRA, non-premultiplied, exactly width*height*4 bytes.
    pub bgra: Vec<u8>,
}

/// Command from the WM main thread to the tray thread (in response to clicks).
pub enum TrayCmd {
    Activate { key: String, x: i32, y: i32 },
    SecondaryActivate { key: String, x: i32, y: i32 },
    /// Fetch the item's dbusmenu and send it back as `TrayEvent::Menu` for the WM
    /// to draw. Falls back to the SNI `ContextMenu` method if the item has no menu.
    OpenMenu { key: String, x: i32, y: i32 },
    /// The user activated a leaf menu row — send dbusmenu `Event(id, "clicked")`.
    MenuClicked { key: String, id: i32 },
    Scroll { key: String, delta: i32, horizontal: bool },
}

const SNI_IFACE: &str = "org.kde.StatusNotifierItem";
const SNI_PATH_DEFAULT: &str = "/StatusNotifierItem";
const DBUSMENU_IFACE: &str = "com.canonical.dbusmenu";

/// key -> (unique bus name, object path). Shared between the watcher (writes on
/// register), the per-item tracking tasks (removes on death) and the command
/// thread (reads to find the item to poke).
type Registry = Arc<Mutex<HashMap<String, (String, String)>>>;

/// Start the tray Watcher+Host on a background thread. Returns a sender the main
/// thread uses to dispatch click/scroll commands to items. `events` is how the
/// thread reports item add/change/remove to the main thread.
pub fn spawn_tray(events: Sender<TrayEvent>) -> std::sync::mpsc::Sender<TrayCmd> {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<TrayCmd>();
    let _ = std::thread::Builder::new()
        .name("sfwm-tray".to_string())
        .spawn(move || {
            if let Err(e) = zbus::block_on(run(events, cmd_rx)) {
                eprintln!("sfwm: tray: D-Bus unavailable ({e}); system tray disabled");
            }
        });
    cmd_tx
}

/// The watcher interface state. All shared bits are `Arc`s so per-item tracking
/// tasks (spawned on the connection's executor) can clean up after themselves.
struct Watcher {
    events: Sender<TrayEvent>,
    registry: Registry,
    /// The raw service strings registered (for the RegisteredStatusNotifierItems
    /// property). Parallel to `registry` but keyed differently, so kept separate.
    services: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    /// An item registers itself. Per spec the argument is EITHER a bus name
    /// ("org.kde.StatusNotifierItem-1234-1") or just an object path
    /// ("/StatusNotifierItem"), in which case the item lives at that path on the
    /// *caller's* unique connection. We read the caller's unique name from the
    /// message header to disambiguate, then resolve any well-known bus name down
    /// to its unique owner so we have one stable key for the item's whole life.
    async fn register_status_notifier_item(
        &self,
        service: String,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(signal_context)] ctxt: SignalContext<'_>,
    ) {
        let sender = hdr.sender().map(|s| s.to_string());
        let (mut bus_name, obj_path) = if service.starts_with('/') {
            // Argument is an object path → the item is on the caller's connection.
            match sender {
                Some(s) => (s, service.clone()),
                None => {
                    eprintln!("sfwm: tray: register with path but no sender; ignoring");
                    return;
                }
            }
        } else {
            // Argument is a bus name; the item sits at the well-known SNI path.
            (service.clone(), SNI_PATH_DEFAULT.to_string())
        };

        // Resolve a well-known name to its unique owner (":1.x"). Unique names
        // already start with ':' and need no lookup.
        if !bus_name.starts_with(':') {
            if let Ok(p) = DBusProxy::new(conn).await {
                if let Ok(bn) = BusName::try_from(bus_name.as_str()) {
                    if let Ok(owner) = p.get_name_owner(bn).await {
                        bus_name = owner.to_string();
                    }
                }
            }
        }
        let key = bus_name; // the unique name is our key

        // First time we see this key: record it, announce it, start tracking.
        let is_new = {
            let mut r = self.registry.lock().unwrap();
            r.insert(key.clone(), (key.clone(), obj_path.clone())).is_none()
        };
        if !is_new {
            return;
        }
        {
            let mut s = self.services.lock().unwrap();
            if !s.contains(&service) {
                s.push(service.clone());
            }
        }
        let _ = Self::status_notifier_item_registered(&ctxt, &service).await;
        eprintln!("sfwm: tray: item registered: {service} (owner {key}, path {obj_path})");

        // Send an initial snapshot INLINE (this method definitely runs — the
        // spawned tracking task below might be delayed), so the icon shows at once.
        if let Ok(item) = Proxy::new(conn, key.clone(), obj_path.clone(), SNI_IFACE).await {
            let snap = fetch_item(&item, &key).await;
            eprintln!(
                "sfwm: tray: {key} status={} icon={} (title {:?})",
                snap.status,
                snap.icon.as_ref().map_or("none", |_| "yes"),
                snap.title
            );
            let _ = self.events.send(TrayEvent::Upsert(snap));
        }

        // Per-item signal loop runs concurrently on the connection's executor.
        conn.executor()
            .spawn(
                track_item(
                    conn.clone(),
                    self.events.clone(),
                    self.registry.clone(),
                    self.services.clone(),
                    key,
                    obj_path,
                    service,
                ),
                "sfwm-tray-item",
            )
            .detach();
    }

    /// A host announces itself. We're the only host, so just re-broadcast.
    async fn register_status_notifier_host(
        &self,
        _service: String,
        #[zbus(signal_context)] ctxt: SignalContext<'_>,
    ) {
        let _ = Self::status_notifier_host_registered(&ctxt).await;
    }

    #[zbus(property)]
    async fn registered_status_notifier_items(&self) -> Vec<String> {
        self.services.lock().unwrap().clone()
    }

    #[zbus(property)]
    async fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    async fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(
        ctxt: &SignalContext<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(
        ctxt: &SignalContext<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_registered(ctxt: &SignalContext<'_>) -> zbus::Result<()>;
}

async fn run(events: Sender<TrayEvent>, cmd_rx: Receiver<TrayCmd>) -> zbus::Result<()> {
    let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
    let services = Arc::new(Mutex::new(Vec::new()));

    // The command loop needs its own handle to report menus back to the WM.
    let cmd_events = events.clone();

    let watcher = Watcher {
        events,
        registry: registry.clone(),
        services,
    };

    // Own the watcher name and serve the interface. If another tray already owns
    // the name, build() fails and we bail (the caller logs).
    let conn = zbus::connection::Builder::session()?
        .name("org.kde.StatusNotifierWatcher")?
        .serve_at("/StatusNotifierWatcher", watcher)?
        .build()
        .await?;

    // Host handshake: apps wait until a host exists before showing their icons.
    // Own a unique host name and advertise ourselves (both the property, always
    // true, and the signal below satisfy every client we've seen).
    let host_name = format!("org.kde.StatusNotifierHost-{}", std::process::id());
    let _ = conn.request_name(host_name.as_str()).await;
    if let Ok(ctxt) = SignalContext::new(&conn, "/StatusNotifierWatcher") {
        let _ = Watcher::status_notifier_host_registered(&ctxt).await;
    }
    eprintln!(
        "sfwm: tray: StatusNotifierWatcher + Host ready (host {host_name}); \
         waiting for items. If your bar has no `sc bar add tray`, icons won't show."
    );

    // Drive TrayCmd → item method calls from a dedicated thread with its own
    // block_on, so a slow/blocked item call never stalls the watcher's I/O.
    {
        let conn = conn.clone();
        let registry = registry.clone();
        let _ = std::thread::Builder::new()
            .name("sfwm-tray-cmd".to_string())
            .spawn(move || command_loop(conn, registry, cmd_events, cmd_rx));
    }

    // Keep the connection alive for the process lifetime; zbus' internal executor
    // services the bus (and our spawned per-item tasks) on the async-io reactor.
    std::future::pending::<()>().await;
    Ok(())
}

/// What a per-item stream event means to the tracking loop.
enum Ev {
    /// A NewIcon/NewTitle/NewStatus/NewToolTip signal — re-fetch and re-send.
    Changed,
    /// The item's unique name lost its owner — the app died.
    Removed,
}

/// Track one item: emit an initial snapshot, then re-snapshot on any change
/// signal, and tear down when the owning connection disappears.
async fn track_item(
    conn: Connection,
    events: Sender<TrayEvent>,
    registry: Registry,
    services: Arc<Mutex<Vec<String>>>,
    key: String,
    obj_path: String,
    service: String,
) {
    let item = match Proxy::new(&conn, key.clone(), obj_path.clone(), SNI_IFACE).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("sfwm: tray: can't proxy item {key}: {e}");
            registry.lock().unwrap().remove(&key);
            return;
        }
    };

    // Initial snapshot.
    let snap = fetch_item(&item, &key).await;
    eprintln!(
        "sfwm: tray: {key} status={} icon={} (title {:?})",
        snap.status,
        snap.icon.as_ref().map_or("none", |_| "yes"),
        snap.title
    );
    let _ = events.send(TrayEvent::Upsert(snap));

    // Merge every stream we care about into one, each mapped to an `Ev`.
    let mut streams = Vec::new();
    for sig in ["NewIcon", "NewTitle", "NewStatus", "NewToolTip"] {
        if let Ok(s) = item.receive_signal(sig).await {
            streams.push(s.map(|_| Ev::Changed).boxed());
        }
    }
    // Removal: watch NameOwnerChanged for our unique name (server-side arg0
    // filter) — an empty new-owner string means the name was released.
    if let Ok(dbus) = Proxy::new(
        &conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await
    {
        if let Ok(s) = dbus
            .receive_signal_with_args("NameOwnerChanged", &[(0, key.as_str())])
            .await
        {
            streams.push(
                s.filter_map(|msg| async move {
                    match msg.body().deserialize::<(String, String, String)>() {
                        Ok((_name, _old, new_owner)) if new_owner.is_empty() => Some(Ev::Removed),
                        _ => None,
                    }
                })
                .boxed(),
            );
        }
    }

    let mut merged = select_all(streams);
    while let Some(ev) = merged.next().await {
        match ev {
            Ev::Changed => {
                let _ = events.send(TrayEvent::Upsert(fetch_item(&item, &key).await));
            }
            Ev::Removed => break,
        }
    }

    // Teardown: drop from the registry + service list, tell the WM, announce it.
    registry.lock().unwrap().remove(&key);
    services.lock().unwrap().retain(|s| s != &service);
    let _ = events.send(TrayEvent::Remove(key.clone()));
    if let Ok(ctxt) = SignalContext::new(&conn, "/StatusNotifierWatcher") {
        let _ = Watcher::status_notifier_item_unregistered(&ctxt, &service).await;
    }
}

/// Read every property we render, tolerating individual failures with defaults.
async fn fetch_item(item: &Proxy<'_>, key: &str) -> TrayItem {
    let title = item
        .get_property::<String>("Title")
        .await
        .unwrap_or_default();
    let status = item
        .get_property::<String>("Status")
        .await
        .unwrap_or_else(|_| "Active".to_string());
    let icon_name = item
        .get_property::<String>("IconName")
        .await
        .unwrap_or_default();
    let pixmap = item
        .get_property::<Vec<(i32, i32, Vec<u8>)>>("IconPixmap")
        .await
        .unwrap_or_default();
    // App-provided icon dir (appindicator apps ship icons outside the system theme).
    let icon_theme_path = item
        .get_property::<String>("IconThemePath")
        .await
        .unwrap_or_default();
    let is_menu = item
        .get_property::<bool>("ItemIsMenu")
        .await
        .unwrap_or(false);
    let menu = item
        .get_property::<OwnedObjectPath>("Menu")
        .await
        .ok()
        .map(|p| p.as_str().to_string())
        .filter(|p| p != "/"); // "/" is the "no menu" sentinel

    TrayItem {
        key: key.to_string(),
        title,
        status,
        icon: resolve_icon(&pixmap, &icon_name, &icon_theme_path),
        is_menu,
        menu,
    }
}

/// Produce a BGRA icon: embedded pixmap wins (crisp, no theme lookup), else a
/// themed/absolute IconName resolved to a PNG or SVG on disk. `icon_theme_path`
/// is the app's own icon dir (SNI `IconThemePath`), searched before the themes.
fn resolve_icon(
    pixmap: &[(i32, i32, Vec<u8>)],
    icon_name: &str,
    icon_theme_path: &str,
) -> Option<TrayIcon> {
    if let Some(icon) = pick_pixmap(pixmap) {
        return Some(icon);
    }
    if !icon_name.is_empty() {
        if icon_name.starts_with('/') {
            return decode_file(Path::new(icon_name));
        }
        if let Some(path) = find_icon_file(icon_name, icon_theme_path) {
            return decode_file(&path);
        }
    }
    None
}

/// Pick the best embedded pixmap and convert it. IconPixmap entries are
/// ARGB32 in *network* (big-endian) byte order, i.e. memory bytes [A,R,G,B].
/// Tray icons render ~22px, so prefer the largest entry that's still ≤32px
/// (avoids downscaling a 256px icon); fall back to the smallest otherwise.
fn pick_pixmap(entries: &[(i32, i32, Vec<u8>)]) -> Option<TrayIcon> {
    let mut under: Option<&(i32, i32, Vec<u8>)> = None; // largest width ≤ 32
    let mut smallest: Option<&(i32, i32, Vec<u8>)> = None;
    for e in entries {
        let (w, h, bytes) = e;
        if *w <= 0 || *h <= 0 {
            continue;
        }
        if bytes.len() < (*w as usize) * (*h as usize) * 4 {
            continue;
        }
        if *w <= 32 && under.map_or(true, |u| *w > u.0) {
            under = Some(e);
        }
        if smallest.map_or(true, |s| *w < s.0) {
            smallest = Some(e);
        }
    }
    let (w, h, src) = under.or(smallest)?;
    let (w, h) = (*w as u32, *h as u32);
    let mut bgra = Vec::with_capacity((w * h * 4) as usize);
    for px in src.chunks_exact(4).take((w * h) as usize) {
        // [A,R,G,B] (ARGB big-endian) -> [B,G,R,A]
        bgra.extend_from_slice(&[px[3], px[2], px[1], px[0]]);
    }
    Some(TrayIcon {
        width: w,
        height: h,
        bgra,
    })
}

/// Decode an icon file to straight-alpha BGRA. SVG is rasterized with resvg
/// (most themes ship SVG-only status icons); everything else via `image`.
fn decode_file(path: &Path) -> Option<TrayIcon> {
    let is_svg = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("svg"))
        .unwrap_or(false);
    if is_svg {
        decode_svg(path)
    } else {
        decode_raster(path)
    }
}

/// Decode a PNG/JPEG/etc file to BGRA (image gives us RGBA; swap R and B).
fn decode_raster(path: &Path) -> Option<TrayIcon> {
    let img = image::open(path).ok()?.to_rgba8();
    let (width, height) = (img.width(), img.height());
    let mut bgra = img.into_raw();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Some(TrayIcon {
        width,
        height,
        bgra,
    })
}

/// Rasterize an SVG to straight-alpha BGRA at ~48px (main scales to bar height).
/// resvg outputs premultiplied RGBA, so un-premultiply back to straight alpha
/// (our bar blend expects straight-alpha source colours).
fn decode_svg(path: &Path) -> Option<TrayIcon> {
    const PX: f32 = 48.0;
    let data = fs::read(path).ok()?;
    let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).ok()?;
    let size = tree.size();
    let (sw, sh) = (size.width(), size.height());
    if sw <= 0.0 || sh <= 0.0 {
        return None;
    }
    let scale = PX / sw.max(sh);
    let pw = (sw * scale).round().max(1.0) as u32;
    let ph = (sh * scale).round().max(1.0) as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(pw, ph)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    // tiny-skia pixmap is premultiplied RGBA → straight BGRA.
    let mut bgra = pixmap.take();
    for px in bgra.chunks_exact_mut(4) {
        let a = px[3];
        if a > 0 {
            let un = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
            let (r, g, b) = (un(px[0]), un(px[1]), un(px[2]));
            px[0] = b;
            px[1] = g;
            px[2] = r;
        }
    }
    Some(TrayIcon {
        width: pw,
        height: ph,
        bgra,
    })
}

// --- icon-theme lookup (PNG only; no SVG rasterizer here) --------------------

/// The user's icon theme from qt6ct, if configured.
fn qt_icon_theme() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let content = fs::read_to_string(format!("{home}/.config/qt6ct/qt6ct.conf")).ok()?;
    for line in content.lines() {
        if let Some(v) = line.trim().strip_prefix("icon_theme=") {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Standard icon base directories, de-duplicated, in search order.
fn icon_base_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(format!("{home}/.icons")));
        dirs.push(PathBuf::from(format!("{home}/.local/share/icons")));
    }
    dirs.push(PathBuf::from("/usr/local/share/icons"));
    dirs.push(PathBuf::from("/usr/share/icons"));
    let xdg =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    for d in xdg.split(':') {
        if !d.is_empty() {
            dirs.push(PathBuf::from(d).join("icons"));
        }
    }
    let mut seen = HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    dirs
}

/// Themes to search, in priority order: the configured theme, its `Inherits`,
/// then Adwaita and hicolor as universal fallbacks.
fn theme_list(bases: &[PathBuf]) -> Vec<String> {
    let mut themes = Vec::new();
    if let Some(t) = qt_icon_theme() {
        for parent in theme_inherits(bases, &t) {
            if !themes.contains(&parent) {
                themes.push(parent);
            }
        }
        if !themes.contains(&t) {
            themes.insert(0, t);
        }
    }
    for fallback in ["Adwaita", "hicolor"] {
        let f = fallback.to_string();
        if !themes.contains(&f) {
            themes.push(f);
        }
    }
    themes
}

/// Parse `Inherits=` from a theme's index.theme (first base dir that has it).
fn theme_inherits(bases: &[PathBuf], theme: &str) -> Vec<String> {
    for base in bases {
        let index = base.join(theme).join("index.theme");
        if let Ok(content) = fs::read_to_string(&index) {
            for line in content.lines() {
                if let Some(v) = line.trim().strip_prefix("Inherits=") {
                    return v
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
            }
        }
    }
    Vec::new()
}

/// Find `<name>.png` or `<name>.svg`, searching the app's own `IconThemePath`
/// first, then the icon themes, preferring 22/24px variants.
fn find_icon_file(name: &str, icon_theme_path: &str) -> Option<PathBuf> {
    let targets = [format!("{name}.png"), format!("{name}.svg")];

    // 1. The app's private icon dir (appindicator IconThemePath), if any.
    if !icon_theme_path.is_empty() {
        let dir = Path::new(icon_theme_path);
        if dir.is_dir() {
            let mut candidates = Vec::new();
            walk_collect(dir, &targets, 0, &mut candidates);
            if let Some(best) = best_candidate(candidates) {
                return Some(best);
            }
        }
    }

    // 2. The configured icon themes + fallbacks.
    let bases = icon_base_dirs();
    for theme in theme_list(&bases) {
        let mut candidates = Vec::new();
        for base in &bases {
            let dir = base.join(&theme);
            if dir.is_dir() {
                walk_collect(&dir, &targets, 0, &mut candidates);
            }
        }
        if let Some(best) = best_candidate(candidates) {
            return Some(best);
        }
    }
    None
}

/// Bounded recursive walk (depth-capped, and stops once we have plenty) that
/// collects every file whose name matches one of `targets`.
fn walk_collect(dir: &Path, targets: &[String], depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 5 || out.len() > 64 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_collect(&path, targets, depth + 1, out);
        } else if path
            .file_name()
            .and_then(|f| f.to_str())
            .map_or(false, |f| targets.iter().any(|t| t == f))
        {
            out.push(path);
        }
    }
}

/// Pick the candidate whose path most looks like a tray-sized raster icon.
fn best_candidate(candidates: Vec<PathBuf>) -> Option<PathBuf> {
    let mut best: Option<(i32, PathBuf)> = None;
    for path in candidates {
        let sc = icon_size_score(&path);
        if best.as_ref().map_or(true, |(bs, _)| sc > *bs) {
            best = Some((sc, path));
        }
    }
    best.map(|(_, p)| p)
}

fn icon_size_score(path: &Path) -> i32 {
    let s = path.to_string_lossy();
    if s.contains("/22") || s.contains("22x22") {
        100
    } else if s.contains("/24") || s.contains("24x24") {
        90
    } else if s.contains("/32") || s.contains("32x32") {
        80
    } else if s.contains("symbolic") {
        10
    } else {
        50
    }
}

// --- command dispatch --------------------------------------------------------

/// Blocking loop: for each click/scroll, look up the item's current
/// destination/path and best-effort call the matching SNI method. Items commonly
/// implement only a subset, so all errors are ignored. `OpenMenu`/`MenuClicked`
/// drive the com.canonical.dbusmenu side and report menus back via `events`.
fn command_loop(conn: Connection, registry: Registry, events: Sender<TrayEvent>, rx: Receiver<TrayCmd>) {
    while let Ok(cmd) = rx.recv() {
        let key = match &cmd {
            TrayCmd::Activate { key, .. }
            | TrayCmd::SecondaryActivate { key, .. }
            | TrayCmd::OpenMenu { key, .. }
            | TrayCmd::MenuClicked { key, .. }
            | TrayCmd::Scroll { key, .. } => key.clone(),
        };
        let Some((bus, path)) = registry.lock().unwrap().get(&key).cloned() else {
            eprintln!("sfwm: tray: command for unknown item '{key}' (not in registry)");
            continue;
        };
        let conn = conn.clone();
        let events = events.clone();
        let _ = zbus::block_on(async move {
            match cmd {
                TrayCmd::Activate { x, y, .. } => {
                    let item = Proxy::new(&conn, bus, path, SNI_IFACE).await?;
                    item.call::<_, _, ()>("Activate", &(x, y)).await
                }
                TrayCmd::SecondaryActivate { x, y, .. } => {
                    let item = Proxy::new(&conn, bus, path, SNI_IFACE).await?;
                    item.call::<_, _, ()>("SecondaryActivate", &(x, y)).await
                }
                TrayCmd::Scroll { delta, horizontal, .. } => {
                    let item = Proxy::new(&conn, bus, path, SNI_IFACE).await?;
                    let orientation = if horizontal { "horizontal" } else { "vertical" };
                    item.call::<_, _, ()>("Scroll", &(delta, orientation)).await
                }
                TrayCmd::OpenMenu { x, y, .. } => {
                    open_menu(&conn, &events, &bus, &path, &key, x, y).await;
                    Ok(())
                }
                TrayCmd::MenuClicked { id, .. } => {
                    menu_clicked(&conn, &bus, &path, id).await;
                    Ok(())
                }
            }
        });
    }
}

/// Fetch an item's dbusmenu layout and send it to the WM as `TrayEvent::Menu`.
/// If the item exposes no menu, fall back to the SNI `ContextMenu` method so a
/// non-appindicator item can still respond to a right-click.
async fn open_menu(
    conn: &Connection,
    events: &Sender<TrayEvent>,
    bus: &str,
    sni_path: &str,
    key: &str,
    x: i32,
    y: i32,
) {
    // The dbusmenu object path lives on the SNI item's `Menu` property.
    let menu_path = match Proxy::new(conn, bus.to_string(), sni_path.to_string(), SNI_IFACE).await {
        Ok(item) => item
            .get_property::<OwnedObjectPath>("Menu")
            .await
            .ok()
            .map(|p| p.as_str().to_string()),
        Err(_) => None,
    };
    let menu_path = match menu_path {
        Some(p) if p != "/" => p,
        _ => {
            // No dbusmenu → let the app pop its own menu (best effort; most
            // appindicator apps ignore this, but native SNI items honor it).
            if let Ok(item) = Proxy::new(conn, bus.to_string(), sni_path.to_string(), SNI_IFACE).await
            {
                let _ = item.call::<_, _, ()>("ContextMenu", &(x, y)).await;
            }
            return;
        }
    };

    let menu = match Proxy::new(conn, bus.to_string(), menu_path, DBUSMENU_IFACE).await {
        Ok(m) => m,
        Err(e) => {
            eprintln!("sfwm: tray: can't proxy dbusmenu for {key}: {e}");
            return;
        }
    };
    // Give the app a chance to (re)populate the menu before we read it.
    let _ = menu.call::<_, _, bool>("AboutToShow", &(0i32,)).await;
    // GetLayout returns (u revision, (ia{sv}av) layout) — the layout is a BARE
    // struct, not a variant, so it needs a typed target ("uv" fails to match).
    let layout: (u32, RawLayout) = match menu
        .call("GetLayout", &(0i32, -1i32, Vec::<&str>::new()))
        .await
    {
        Ok(l) => l,
        Err(e) => {
            eprintln!("sfwm: tray: GetLayout failed for {key}: {e}");
            return;
        }
    };
    // The root node is just a container; its `av` children are the menu entries
    // (each a variant wrapping the same struct shape, which the parser handles).
    let items: Vec<MenuNode> = layout
        .1
         .2
        .iter()
        .filter_map(|v| parse_menu_value(v))
        .collect();
    eprintln!("sfwm: tray: menu for {key}: {} top-level items", items.len());
    let _ = events.send(TrayEvent::Menu { key: key.to_string(), x, y, items });
}

/// Tell the app a leaf menu row was activated (dbusmenu `Event(id,"clicked")`).
async fn menu_clicked(conn: &Connection, bus: &str, sni_path: &str, id: i32) {
    let menu_path = match Proxy::new(conn, bus.to_string(), sni_path.to_string(), SNI_IFACE).await {
        Ok(item) => item
            .get_property::<OwnedObjectPath>("Menu")
            .await
            .ok()
            .map(|p| p.as_str().to_string()),
        Err(_) => None,
    };
    let Some(menu_path) = menu_path.filter(|p| p != "/") else {
        return;
    };
    if let Ok(menu) = Proxy::new(conn, bus.to_string(), menu_path, DBUSMENU_IFACE).await {
        // data is an (ignored) empty variant; timestamp 0 is universally accepted.
        let _ = menu
            .call::<_, _, ()>("Event", &(id, "clicked", Value::from(0i32), 0u32))
            .await;
    }
}

// --- dbusmenu layout parsing -------------------------------------------------

fn menu_as_str(v: &Value) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.as_str().to_string()),
        _ => None,
    }
}
fn menu_as_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}
fn menu_as_i32(v: &Value) -> Option<i32> {
    match v {
        Value::I32(i) => Some(*i),
        Value::I16(i) => Some(*i as i32),
        Value::U32(i) => Some(*i as i32),
        Value::U8(i) => Some(*i as i32),
        _ => None,
    }
}

/// Strip GNOME/GTK mnemonic underscores from a label: a lone `_` is dropped, a
/// doubled `__` becomes a literal `_`.
fn strip_mnemonics(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '_' {
            if chars.peek() == Some(&'_') {
                out.push('_');
                chars.next();
            }
            // else: a mnemonic marker — drop it.
        } else {
            out.push(c);
        }
    }
    out
}

/// Resolve a menu row's `icon-name` to a decoded icon (reusing the tray icon
/// theme search). `icon-data` blobs aren't supported — appindicator menus that
/// carry icons use `icon-name` in practice.
fn menu_icon(name: Option<String>) -> Option<TrayIcon> {
    let name = name.filter(|s| !s.is_empty())?;
    if name.starts_with('/') {
        return decode_file(Path::new(&name));
    }
    find_icon_file(&name, "").and_then(|p| decode_file(&p))
}

/// Parse one dbusmenu layout item `(i32 id, a{sv} props, av children)` into a
/// `MenuNode`, recursively. `v` may be the structure directly or a one-layer
/// variant wrapping it (children come wrapped). Returns None for a non-structure.
fn parse_menu_value(v: &Value) -> Option<MenuNode> {
    let v = match v {
        Value::Value(inner) => inner.as_ref(),
        other => other,
    };
    let s = match v {
        Value::Structure(s) => s,
        _ => return None,
    };
    let fields = s.fields();
    if fields.len() < 3 {
        return None;
    }
    let id = menu_as_i32(&fields[0]).unwrap_or(0);

    // Properties: a{sv}. Unwrap each value's variant layer as we collect it.
    let mut props: HashMap<String, &Value> = HashMap::new();
    if let Value::Dict(d) = &fields[1] {
        for (k, val) in d.iter() {
            if let Value::Str(k) = k {
                let val = match val {
                    Value::Value(inner) => inner.as_ref(),
                    other => other,
                };
                props.insert(k.as_str().to_string(), val);
            }
        }
    }
    let get = |k: &str| props.get(k).copied();

    let typ = get("type").and_then(menu_as_str).unwrap_or_default();
    let toggle_type = match get("toggle-type").and_then(menu_as_str).as_deref() {
        Some("checkmark") => 1u8,
        Some("radio") => 2u8,
        _ => 0u8,
    };

    // Children: av (array of variant-wrapped items).
    let mut children = Vec::new();
    if let Value::Array(a) = &fields[2] {
        for c in a.inner() {
            if let Some(n) = parse_menu_value(c) {
                children.push(n);
            }
        }
    }

    Some(MenuNode {
        id,
        label: strip_mnemonics(&get("label").and_then(menu_as_str).unwrap_or_default()),
        enabled: get("enabled").and_then(menu_as_bool).unwrap_or(true),
        visible: get("visible").and_then(menu_as_bool).unwrap_or(true),
        is_separator: typ == "separator",
        has_submenu: get("children-display").and_then(menu_as_str).as_deref() == Some("submenu"),
        toggle_type,
        toggle_state: get("toggle-state").and_then(menu_as_i32).unwrap_or(-1),
        icon: menu_icon(get("icon-name").and_then(menu_as_str)),
        children,
    })
}
