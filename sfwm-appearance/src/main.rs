//! sfwm-appearance — the sommerflusswm appearance GUI: a nitrogen-inspired
//! wallpaper browser plus an icon-theme picker (the Icons tab, which writes qt6ct).
//!
//! It is a normal Wayland toplevel app (it runs *under* the sfwm window manager)
//! and does all wallpaper changes by shelling out to the sibling `sc` binary:
//!
//! ```text
//! sc wallpaper <abs-path> mode=<fill|fit|stretch|center|tile> monitor=<all|N>
//! sc wallpaper color <#rrggbb> monitor=<all|N>
//! sc wallpaper off monitor=<all|N>
//! ```
//!
//! Run with `--restore` (for autostart, like `nitrogen --restore &`) to re-apply
//! the saved selection(s) without opening the GUI.

mod config;
mod iconload;
mod icons;
mod thumbs;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use config::{Config, Saved};
use iconload::{IconKey, IconLoader, IconRequest};
use icons::IconTheme;
use thumbs::ThumbLoader;

/// Top-level tabs of the appearance app.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Wallpaper,
    Icons,
    Cursors,
}

/// Image extensions we show in the grid (compared case-insensitively).
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp", "gif"];

/// The five scaling modes understood by `sc wallpaper`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Fill,
    Fit,
    Stretch,
    Center,
    Tile,
}

impl Mode {
    /// All modes, in display order.
    const ALL: [Mode; 5] = [
        Mode::Fill,
        Mode::Fit,
        Mode::Stretch,
        Mode::Center,
        Mode::Tile,
    ];

    /// The token passed to `sc wallpaper ... mode=<token>`.
    fn token(self) -> &'static str {
        match self {
            Mode::Fill => "fill",
            Mode::Fit => "fit",
            Mode::Stretch => "stretch",
            Mode::Center => "center",
            Mode::Tile => "tile",
        }
    }

    /// Human-readable label for the combo box.
    fn label(self) -> &'static str {
        match self {
            Mode::Fill => "Fill",
            Mode::Fit => "Fit",
            Mode::Stretch => "Stretch",
            Mode::Center => "Center",
            Mode::Tile => "Tile",
        }
    }

    fn from_token(s: &str) -> Mode {
        match s {
            "fit" => Mode::Fit,
            "stretch" => Mode::Stretch,
            "center" => Mode::Center,
            "tile" => Mode::Tile,
            _ => Mode::Fill,
        }
    }
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--restore") {
        return restore();
    }
    match run_gui() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// `--restore`: re-apply every saved selection, then exit. No GUI.
fn restore() -> std::process::ExitCode {
    let cfg = Config::load();
    let mut had_error = false;
    // Apply "all" first so per-monitor entries act as overrides on top of it
    // (BTreeMap order would otherwise run "all" last and clobber them).
    let ordered = cfg
        .saved
        .iter()
        .filter(|(m, _)| m.as_str() == "all")
        .chain(cfg.saved.iter().filter(|(m, _)| m.as_str() != "all"));
    for (mon, saved) in ordered {
        let result = match saved {
            Saved::Image { mode, path } => run_sc(&[path, &format!("mode={mode}"), &format!("monitor={mon}")]),
            Saved::Color { color } => run_sc(&["color", color, &format!("monitor={mon}")]),
        };
        if let Err(e) = result {
            eprintln!("error: restoring monitor {mon}: {e}");
            had_error = true;
        }
    }
    if let Some(theme) = &cfg.cursor_theme {
        let size = cfg.cursor_size.unwrap_or(24).to_string();
        if let Err(e) = run_sc_raw(&["cursor_theme", theme, &size]) {
            eprintln!("error: restoring cursor theme: {e}");
            had_error = true;
        }
    }
    if had_error {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}

/// Run `sc wallpaper <args...>`, capturing output.
fn run_sc(args: &[&str]) -> Result<(), String> {
    let mut full = vec!["wallpaper"];
    full.extend_from_slice(args);
    run_sc_raw(&full)
}

/// Run `sc <args...>` verbatim, capturing output.
///
/// On success `sc` prints nothing and exits 0 → returns `Ok(())`.
/// On error `sc` prints `error: ...` to stdout and exits non-zero → returns the
/// captured message (so the GUI can surface it / `--restore` can log it).
fn run_sc_raw(args: &[&str]) -> Result<(), String> {
    let mut cmd = Command::new("sc");
    cmd.args(args);
    let out = cmd
        .output()
        .map_err(|e| format!("failed to run `sc`: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let mut msg = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if msg.is_empty() {
        msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
    }
    if msg.is_empty() {
        msg = format!("sc exited with status {}", out.status);
    }
    Err(msg)
}

/// Query `sc list_monitors` and return the available monitor selectors:
/// `"all"` plus one entry per monitor index. Falls back to `["all", "0"]` if
/// the command fails or reports no monitors.
fn query_monitors() -> Vec<String> {
    let fallback = || vec!["all".to_string(), "0".to_string()];
    let out = match Command::new("sc").arg("list_monitors").output() {
        Ok(o) if o.status.success() => o,
        _ => return fallback(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    // Don't depend on the exact line format — just count non-empty lines.
    let count = text.lines().filter(|l| !l.trim().is_empty()).count();
    if count == 0 {
        return fallback();
    }
    let mut v = vec!["all".to_string()];
    v.extend((0..count).map(|i| i.to_string()));
    v
}

/// List image files in `dir`, sorted by file name (case-insensitive ext match).
fn list_images(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file() && has_image_ext(p))
            .collect(),
        Err(_) => Vec::new(),
    };
    v.sort_by(|a, b| {
        a.file_name()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .cmp(&b.file_name().unwrap_or_default().to_ascii_lowercase())
    });
    v
}

fn has_image_ext(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            IMAGE_EXTS.contains(&e.as_str())
        })
        .unwrap_or(false)
}

fn run_gui() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 720.0])
            .with_min_inner_size([640.0, 420.0])
            .with_title("sfwm appearance"),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "sfwm-appearance",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

/// The egui application state.
struct App {
    /// Active top-level tab.
    tab: Tab,

    cfg: Config,
    /// Currently selected browse directory (index into `cfg.dirs`).
    selected_dir: Option<usize>,
    /// Images in the selected directory.
    images: Vec<PathBuf>,
    /// Currently selected image (a path within `images`).
    selected_image: Option<PathBuf>,

    /// Text field for adding a new browse directory.
    new_dir_input: String,

    mode: Mode,
    monitors: Vec<String>,
    monitor: String,
    /// RGB for the solid-colour picker.
    color: [u8; 3],

    /// Last action / error shown in the status bar.
    status: String,

    /// Thumbnail loader (background thread) + caches.
    loader: ThumbLoader,
    /// Decoded thumbnails keyed by path.
    thumbs: HashMap<PathBuf, egui::TextureHandle>,
    /// Paths already requested or known-failed (avoid re-decoding).
    requested: HashSet<PathBuf>,
    /// Paths that failed to decode (shown as a placeholder).
    failed: HashSet<PathBuf>,

    // ----- Icons tab -----
    /// Icon base dirs, cached for resolution (shared with the worker).
    icon_base_dirs: Vec<PathBuf>,
    /// Discovered installable icon themes, sorted by display name.
    themes: Vec<IconTheme>,
    /// Currently selected theme (index into `themes`).
    selected_theme: Option<usize>,
    /// Independent status line for the Icons tab.
    icon_status: String,
    /// Icon preview loader (background thread) + caches.
    icon_loader: IconLoader,
    /// Rendered preview textures keyed by (theme-dir, first-slot-name).
    icon_texs: HashMap<IconKey, egui::TextureHandle>,
    /// Slots already requested (avoid re-rendering).
    icon_requested: HashSet<IconKey>,
    /// Slots that resolved to nothing / failed to render.
    icon_failed: HashSet<IconKey>,

    // ----- Cursors tab -----
    /// Installed cursor themes (directory names with a `cursors/` subdir).
    cursor_themes: Vec<String>,
    /// Currently selected cursor theme (index into `cursor_themes`).
    selected_cursor: Option<usize>,
    /// Cursor size applied with the theme.
    cursor_size: u32,
    /// Independent status line for the Cursors tab.
    cursor_status: String,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> App {
        let mut cfg = Config::load();

        // If no dirs configured, default to the usual wallpaper folders that
        // actually exist; otherwise start empty (user adds one).
        if cfg.dirs.is_empty() {
            let home = std::env::var("HOME").unwrap_or_default();
            for cand in [
                format!("{home}/Pictures/wallpapers"),
                format!("{home}/wallpapers"),
            ] {
                if Path::new(&cand).is_dir() {
                    cfg.dirs.push(cand);
                }
            }
        }

        let monitors = query_monitors();
        let monitor = monitors.first().cloned().unwrap_or_else(|| "all".into());

        // Scan installed icon themes up front (cheap: a few directory reads; no
        // icon decoding happens here — that's lazy, per selected theme).
        let icon_base_dirs = icons::icon_base_dirs();
        let themes = icons::scan_themes(&icon_base_dirs);
        // Pre-select the theme saved in our config, if it's still installed.
        let selected_theme = cfg
            .icon_theme
            .as_deref()
            .and_then(|saved| themes.iter().position(|t| t.dir_name == saved));

        let cursor_themes = find_cursor_themes();
        let selected_cursor = cfg
            .cursor_theme
            .as_deref()
            .and_then(|saved| cursor_themes.iter().position(|t| t == saved));
        let cursor_size = cfg.cursor_size.unwrap_or(24);

        let mut app = App {
            tab: Tab::Wallpaper,
            cfg,
            selected_dir: None,
            images: Vec::new(),
            selected_image: None,
            new_dir_input: String::new(),
            mode: Mode::Fill,
            monitors,
            monitor,
            color: [30, 30, 46],
            status: "Ready.".to_string(),
            loader: ThumbLoader::spawn(cc.egui_ctx.clone()),
            thumbs: HashMap::new(),
            requested: HashSet::new(),
            failed: HashSet::new(),

            icon_base_dirs,
            themes,
            selected_theme,
            icon_status: "Ready.".to_string(),
            icon_loader: IconLoader::spawn(cc.egui_ctx.clone()),
            icon_texs: HashMap::new(),
            icon_requested: HashSet::new(),
            icon_failed: HashSet::new(),

            cursor_themes,
            selected_cursor,
            cursor_size,
            cursor_status: "Ready.".to_string(),
        };

        // Pre-select the first existing directory.
        if let Some(idx) = app.cfg.dirs.iter().position(|d| Path::new(d).is_dir()) {
            app.select_dir(idx);
        }
        app
    }

    /// Switch the active browse directory and rescan its images.
    fn select_dir(&mut self, idx: usize) {
        self.selected_dir = Some(idx);
        self.selected_image = None;
        if let Some(dir) = self.cfg.dirs.get(idx) {
            self.images = list_images(Path::new(dir));
        } else {
            self.images.clear();
        }
    }

    /// Ensure a thumbnail for `path` is requested (once).
    fn ensure_requested(&mut self, path: &Path) {
        if self.thumbs.contains_key(path) || self.requested.contains(path) {
            return;
        }
        self.requested.insert(path.to_path_buf());
        self.loader.request(path.to_path_buf());
    }

    /// Upload any thumbnails that finished decoding.
    fn drain_thumbs(&mut self, ctx: &egui::Context) {
        for res in self.loader.drain() {
            match res.image {
                Some((w, h, rgba)) => {
                    let color = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
                    let tex = ctx.load_texture(
                        res.path.to_string_lossy(),
                        color,
                        egui::TextureOptions::LINEAR,
                    );
                    self.thumbs.insert(res.path, tex);
                }
                None => {
                    self.failed.insert(res.path);
                }
            }
        }
    }

    /// Current selectors for the active monitor, as owned strings.
    fn monitor_arg(&self) -> String {
        format!("monitor={}", self.monitor)
    }

    /// Apply the selected image (without saving).
    fn apply_image(&mut self) {
        let Some(path) = self.selected_image.clone() else {
            self.status = "No image selected.".to_string();
            return;
        };
        let path_str = path.to_string_lossy().to_string();
        let mode_arg = format!("mode={}", self.mode.token());
        let mon_arg = self.monitor_arg();
        match run_sc(&[&path_str, &mode_arg, &mon_arg]) {
            Ok(()) => {
                self.status = format!(
                    "Applied {} ({}, monitor {}).",
                    file_label(&path),
                    self.mode.label(),
                    self.monitor
                );
            }
            Err(e) => self.status = format!("error: {e}"),
        }
    }

    /// Apply the selected image and persist it to the config.
    fn apply_and_save_image(&mut self) {
        let Some(path) = self.selected_image.clone() else {
            self.status = "No image selected.".to_string();
            return;
        };
        self.apply_image();
        // Only persist if the status didn't report an error.
        if self.status.starts_with("error:") {
            return;
        }
        self.cfg.set_saved(
            &self.monitor.clone(),
            Saved::Image {
                mode: self.mode.token().to_string(),
                path: path.to_string_lossy().to_string(),
            },
        );
        match self.cfg.save() {
            Ok(()) => {
                self.status = format!("{} Saved.", self.status);
            }
            Err(e) => self.status = format!("error: saving config: {e}"),
        }
    }

    /// Apply the solid colour from the picker.
    fn apply_color(&mut self, save: bool) {
        let hex = format!("#{:02x}{:02x}{:02x}", self.color[0], self.color[1], self.color[2]);
        let mon_arg = self.monitor_arg();
        match run_sc(&["color", &hex, &mon_arg]) {
            Ok(()) => {
                self.status = format!("Applied colour {hex} (monitor {}).", self.monitor);
                if save {
                    self.cfg
                        .set_saved(&self.monitor.clone(), Saved::Color { color: hex });
                    if let Err(e) = self.cfg.save() {
                        self.status = format!("error: saving config: {e}");
                    } else {
                        self.status = format!("{} Saved.", self.status);
                    }
                }
            }
            Err(e) => self.status = format!("error: {e}"),
        }
    }
}

/// XCursor themes: directories under the icon base paths that contain a
/// `cursors/` subdirectory. Returned as sorted, deduplicated directory names.
fn find_cursor_themes() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let bases = [
        format!("{home}/.icons"),
        format!("{home}/.local/share/icons"),
        "/usr/share/icons".to_string(),
    ];
    let mut out: Vec<String> = Vec::new();
    for base in bases {
        let Ok(entries) = std::fs::read_dir(&base) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.join("cursors").is_dir() {
                if let Some(name) = p.file_name().map(|n| n.to_string_lossy().to_string()) {
                    if !out.contains(&name) {
                        out.push(name);
                    }
                }
            }
        }
    }
    out.sort_by_key(|a| a.to_ascii_lowercase());
    out
}

impl App {
    /// Apply the selected cursor theme via `sc cursor_theme` and persist it.
    fn apply_cursor_theme(&mut self) {
        let Some(theme) = self.selected_cursor.and_then(|i| self.cursor_themes.get(i)).cloned()
        else {
            self.cursor_status = "No cursor theme selected.".to_string();
            return;
        };
        let size = self.cursor_size.max(1);
        match run_sc_raw(&["cursor_theme", &theme, &size.to_string()]) {
            Ok(()) => {
                self.cfg.set_cursor_theme(&theme, size);
                match self.cfg.save() {
                    Ok(()) => {
                        self.cursor_status = format!("Applied cursor theme {theme} @ {size}px. Saved.")
                    }
                    Err(e) => self.cursor_status = format!("error: saving config: {e}"),
                }
            }
            Err(e) => self.cursor_status = format!("error: {e}"),
        }
    }

    fn cursors_left_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("cursor-themes")
            .resizable(false)
            .exact_width(240.0)
            .show(ctx, |ui| {
                ui.heading("Cursor themes");
                ui.separator();
                if self.cursor_themes.is_empty() {
                    ui.label("No cursor themes found.");
                    return;
                }
                let mut select: Option<usize> = None;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .drag_to_scroll(false)
                    .show(ui, |ui| {
                        for (i, name) in self.cursor_themes.iter().enumerate() {
                            let selected = self.selected_cursor == Some(i);
                            if ui.selectable_label(selected, name).clicked() {
                                select = Some(i);
                            }
                        }
                    });
                if let Some(i) = select {
                    self.selected_cursor = Some(i);
                }
            });
    }

    fn cursors_bottom_panel(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("cursor-controls").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                egui::ComboBox::from_label("Size")
                    .selected_text(self.cursor_size.to_string())
                    .show_ui(ui, |ui| {
                        for s in [16u32, 24, 32, 48, 64] {
                            ui.selectable_value(&mut self.cursor_size, s, s.to_string());
                        }
                    });
                let can_apply = self.selected_cursor.is_some();
                if ui.add_enabled(can_apply, egui::Button::new("Apply")).clicked() {
                    self.apply_cursor_theme();
                }
                if let Some(t) = self.selected_cursor.and_then(|i| self.cursor_themes.get(i)) {
                    ui.label(egui::RichText::new(format!("→ {t}")).weak());
                }
            });
            ui.add_space(2.0);
            ui.separator();
            let is_err = self.cursor_status.starts_with("error:");
            let color = if is_err {
                egui::Color32::from_rgb(0xff, 0x6b, 0x6b)
            } else {
                ui.visuals().text_color()
            };
            ui.label(egui::RichText::new(&self.cursor_status).color(color));
            ui.add_space(2.0);
        });
    }

    fn cursors_central_panel(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(theme) = self.selected_cursor.and_then(|i| self.cursor_themes.get(i)) else {
                ui.centered_and_justified(|ui| {
                    ui.label("Select a cursor theme on the left.");
                });
                return;
            };
            ui.heading(theme);
            ui.add_space(6.0);
            ui.label("Applies to the compositor cursor immediately (sc cursor_theme).");
            ui.label("Running apps pick the theme up when restarted; new apps inherit it.");
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "Saved to wallpaper.conf and re-applied at login by sfwm-appearance --restore.",
                )
                .weak(),
            );
        });
    }
}

/// Short label for a path (file name, or the full path if it has none).
fn file_label(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| p.to_string_lossy().to_string())
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_thumbs(ctx);
        self.drain_icons(ctx);

        // Tab bar (added before any side/bottom/central panels).
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Wallpaper, "Wallpaper");
                ui.selectable_value(&mut self.tab, Tab::Icons, "Icons");
                ui.selectable_value(&mut self.tab, Tab::Cursors, "Cursors");
            });
            ui.add_space(2.0);
        });

        match self.tab {
            Tab::Wallpaper => {
                self.left_panel(ctx);
                self.bottom_panel(ctx);
                self.central_panel(ctx);
            }
            Tab::Icons => {
                self.icons_left_panel(ctx);
                self.icons_bottom_panel(ctx);
                self.icons_central_panel(ctx);
            }
            Tab::Cursors => {
                self.cursors_left_panel(ctx);
                self.cursors_bottom_panel(ctx);
                self.cursors_central_panel(ctx);
            }
        }
    }
}

impl App {
    fn left_panel(&mut self, ctx: &egui::Context) {
        // Fixed width (NOT resizable): a draggable separator can be grabbed by a
        // phantom pointer-drag under the compositor and yanked across the window,
        // shoving the wallpaper grid off-screen. Pin it so that can't happen.
        egui::SidePanel::left("dirs")
            .resizable(false)
            .exact_width(240.0)
            .show(ctx, |ui| {
                ui.heading("Directories");
                ui.separator();

                // Directory list with per-row remove control.
                let mut remove: Option<usize> = None;
                let mut select: Option<usize> = None;
                let dirs = self.cfg.dirs.clone();
                for (i, dir) in dirs.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.small_button("✕").on_hover_text("Remove").clicked() {
                            remove = Some(i);
                        }
                        let selected = self.selected_dir == Some(i);
                        // Show just the last path component as the label, full
                        // path on hover.
                        let label = Path::new(dir)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| dir.clone());
                        if ui
                            .selectable_label(selected, label)
                            .on_hover_text(dir)
                            .clicked()
                        {
                            select = Some(i);
                        }
                    });
                }
                if let Some(i) = select {
                    self.select_dir(i);
                }
                if let Some(i) = remove {
                    self.cfg.dirs.remove(i);
                    // Fix up selection.
                    match self.selected_dir {
                        Some(s) if s == i => {
                            self.selected_dir = None;
                            self.images.clear();
                            self.selected_image = None;
                        }
                        Some(s) if s > i => self.selected_dir = Some(s - 1),
                        _ => {}
                    }
                    let _ = self.cfg.save();
                }

                ui.separator();
                ui.label("Add directory (absolute path):");
                ui.horizontal(|ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.new_dir_input)
                            .desired_width(f32::INFINITY)
                            .hint_text("/home/you/wallpapers"),
                    );
                    let submit = resp.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.button("Add").clicked() || submit {
                        self.add_dir();
                    }
                });
            });
    }

    fn add_dir(&mut self) {
        let path = self.new_dir_input.trim().to_string();
        if path.is_empty() {
            return;
        }
        if !Path::new(&path).is_absolute() {
            self.status = format!("error: not an absolute path: {path}");
            return;
        }
        if !Path::new(&path).is_dir() {
            self.status = format!("error: not a directory: {path}");
            return;
        }
        if self.cfg.dirs.iter().any(|d| d == &path) {
            self.status = "Directory already in list.".to_string();
            self.new_dir_input.clear();
            return;
        }
        self.cfg.dirs.push(path.clone());
        let idx = self.cfg.dirs.len() - 1;
        self.select_dir(idx);
        self.new_dir_input.clear();
        let _ = self.cfg.save();
        self.status = format!("Added {path}.");
    }

    fn bottom_panel(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("controls").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                // Mode combo.
                egui::ComboBox::from_label("Mode")
                    .selected_text(self.mode.label())
                    .show_ui(ui, |ui| {
                        for m in Mode::ALL {
                            ui.selectable_value(&mut self.mode, m, m.label());
                        }
                    });

                ui.separator();

                // Monitor combo.
                egui::ComboBox::from_label("Monitor")
                    .selected_text(self.monitor.clone())
                    .show_ui(ui, |ui| {
                        let opts = self.monitors.clone();
                        for m in opts {
                            ui.selectable_value(&mut self.monitor, m.clone(), m);
                        }
                    });

                ui.separator();

                if ui.button("Apply").clicked() {
                    self.apply_image();
                }
                if ui.button("Apply & Save").clicked() {
                    self.apply_and_save_image();
                }
            });

            ui.add_space(2.0);
            ui.horizontal_wrapped(|ui| {
                ui.label("Solid colour:");
                ui.color_edit_button_srgb(&mut self.color);
                if ui.button("Apply colour").clicked() {
                    self.apply_color(false);
                }
                if ui.button("Apply & Save colour").clicked() {
                    self.apply_color(true);
                }
                if ui.button("Clear (off)").clicked() {
                    let mon_arg = self.monitor_arg();
                    match run_sc(&["off", &mon_arg]) {
                        Ok(()) => self.status = format!("Cleared monitor {}.", self.monitor),
                        Err(e) => self.status = format!("error: {e}"),
                    }
                }
            });

            ui.add_space(2.0);
            ui.separator();
            // Status line (coloured red on error).
            let is_err = self.status.starts_with("error:");
            let color = if is_err {
                egui::Color32::from_rgb(0xff, 0x6b, 0x6b)
            } else {
                ui.visuals().text_color()
            };
            ui.label(egui::RichText::new(&self.status).color(color));
            ui.add_space(2.0);
        });
    }

    fn central_panel(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.selected_dir.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.label("No directory selected. Add or pick one on the left.");
                });
                return;
            }
            if self.images.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("No images in this directory.");
                });
                return;
            }

            // Request thumbnails for everything in the dir (the worker decodes
            // them lazily off-thread; cheap to over-request since it's once).
            let paths = self.images.clone();
            for p in &paths {
                self.ensure_requested(p);
            }

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                // Don't let a press-drag on the grid background pan the view (which
                // a stray/stuck pointer-drag would otherwise turn into a slide).
                .drag_to_scroll(false)
                .show(ui, |ui| {
                    // horizontal_wrapped can't wrap Frame containers (their size
                    // isn't known until after layout), so chunk into explicit
                    // rows: cell = min size + frame inner margin, plus spacing.
                    let cell_w = thumbs::THUMB_MAX as f32 + 12.0 + 8.0
                        + ui.spacing().item_spacing.x;
                    let cols = (ui.available_width() / cell_w).floor().max(1.0) as usize;
                    let mut clicked: Option<PathBuf> = None;
                    for row in paths.chunks(cols) {
                        ui.horizontal(|ui| {
                            for path in row {
                                if self.thumb_widget(ui, path) {
                                    clicked = Some(path.clone());
                                }
                            }
                        });
                    }
                    if let Some(p) = clicked {
                        self.selected_image = Some(p);
                    }
                });
        });
    }

    /// Draw a single thumbnail cell. Returns true if it was clicked.
    fn thumb_widget(&self, ui: &mut egui::Ui, path: &Path) -> bool {
        // Fixed cell so the grid stays tidy regardless of thumbnail aspect.
        let cell = egui::vec2(thumbs::THUMB_MAX as f32 + 12.0, thumbs::THUMB_MAX as f32 + 28.0);
        let selected = self.selected_image.as_deref() == Some(path);

        let mut clicked = false;
        let frame = egui::Frame::group(ui.style())
            .stroke(if selected {
                egui::Stroke::new(3.0, egui::Color32::from_rgb(0x89, 0xb4, 0xfa))
            } else {
                ui.visuals().widgets.noninteractive.bg_stroke
            })
            .inner_margin(4.0);

        frame.show(ui, |ui| {
            ui.set_min_size(cell);
            ui.set_max_size(cell);
            ui.vertical_centered(|ui| {
                let img_area = egui::vec2(thumbs::THUMB_MAX as f32, thumbs::THUMB_MAX as f32);
                if let Some(tex) = self.thumbs.get(path) {
                    let sized = egui::load::SizedTexture::new(tex.id(), tex.size_vec2());
                    let resp = ui.add(
                        egui::Image::new(sized)
                            .max_size(img_area)
                            .fit_to_original_size(1.0)
                            .sense(egui::Sense::click()),
                    );
                    if resp.clicked() {
                        clicked = true;
                    }
                } else if self.failed.contains(path) {
                    let (rect, resp) = ui.allocate_exact_size(img_area, egui::Sense::click());
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "⚠ decode failed",
                        egui::FontId::proportional(12.0),
                        ui.visuals().weak_text_color(),
                    );
                    if resp.clicked() {
                        clicked = true;
                    }
                } else {
                    // Still decoding — show a spinner placeholder.
                    let (rect, resp) = ui.allocate_exact_size(img_area, egui::Sense::click());
                    ui.put(rect, egui::Spinner::new());
                    if resp.clicked() {
                        clicked = true;
                    }
                }
                ui.label(
                    egui::RichText::new(elide(&file_label(path), 22)).small(),
                );
            });
        });

        clicked
    }
}

impl App {
    /// Upload any icon previews that finished rendering.
    fn drain_icons(&mut self, ctx: &egui::Context) {
        for res in self.icon_loader.drain() {
            match res.image {
                Some((w, h, rgba)) => {
                    let color = if res.premultiplied {
                        egui::ColorImage::from_rgba_premultiplied([w, h], &rgba)
                    } else {
                        egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba)
                    };
                    let name = format!("icon:{}:{}", res.key.0, res.key.1);
                    let tex = ctx.load_texture(name, color, egui::TextureOptions::LINEAR);
                    self.icon_texs.insert(res.key, tex);
                }
                None => {
                    self.icon_failed.insert(res.key);
                }
            }
        }
    }

    /// Request preview rendering for a slot of the given theme (once).
    fn ensure_icon_requested(&mut self, theme_dir: &str, names: &[&str]) {
        let key: IconKey = (
            theme_dir.to_string(),
            names.first().copied().unwrap_or("").to_string(),
        );
        if self.icon_texs.contains_key(&key)
            || self.icon_requested.contains(&key)
            || self.icon_failed.contains(&key)
        {
            return;
        }
        self.icon_requested.insert(key);
        self.icon_loader.request(IconRequest {
            theme_dir: theme_dir.to_string(),
            names: names.iter().map(|s| s.to_string()).collect(),
            base_dirs: self.icon_base_dirs.clone(),
        });
    }

    /// Request all sample-slot previews for the selected theme (lazy: only ever
    /// the visible/selected theme, never every theme up front).
    fn request_selected_previews(&mut self) {
        let Some(theme) = self.selected_theme.and_then(|i| self.themes.get(i)) else {
            return;
        };
        let dir = theme.dir_name.clone();
        for slot in icons::SAMPLE_ICONS {
            self.ensure_icon_requested(&dir, slot);
        }
    }

    /// Apply the selected theme: write qt6ct.conf and persist to our config.
    fn apply_icon_theme(&mut self) {
        let Some(theme) = self.selected_theme.and_then(|i| self.themes.get(i)) else {
            self.icon_status = "No icon theme selected.".to_string();
            return;
        };
        let dir_name = theme.dir_name.clone();
        let display = theme.display_name.clone();
        match icons::apply_qt6ct_icon_theme(&dir_name) {
            Ok(()) => {
                self.cfg.set_icon_theme(&dir_name);
                match self.cfg.save() {
                    Ok(()) => {
                        self.icon_status =
                            format!("Applied icon theme {display} ({dir_name}). Saved.")
                    }
                    Err(e) => self.icon_status = format!("error: saving config: {e}"),
                }
            }
            Err(e) => self.icon_status = format!("error: {e}"),
        }
    }

    fn icons_left_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("themes")
            .resizable(false)
            .exact_width(240.0)
            .show(ctx, |ui| {
                ui.heading("Icon themes");
                ui.separator();
                if self.themes.is_empty() {
                    ui.label("No icon themes found.");
                    return;
                }
                let mut select: Option<usize> = None;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .drag_to_scroll(false)
                    .show(ui, |ui| {
                        for (i, theme) in self.themes.iter().enumerate() {
                            let selected = self.selected_theme == Some(i);
                            let text = egui::RichText::new(&theme.display_name);
                            let resp = ui
                                .selectable_label(selected, text)
                                .on_hover_text(&theme.dir_name);
                            // Small dir-name subtitle under each row.
                            ui.add_space(-4.0);
                            ui.label(
                                egui::RichText::new(elide(&theme.dir_name, 30))
                                    .small()
                                    .weak(),
                            );
                            ui.add_space(2.0);
                            if resp.clicked() {
                                select = Some(i);
                            }
                        }
                    });
                if let Some(i) = select {
                    self.selected_theme = Some(i);
                }
            });
    }

    fn icons_bottom_panel(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("icon-controls").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let can_apply = self.selected_theme.is_some();
                if ui
                    .add_enabled(can_apply, egui::Button::new("Apply"))
                    .clicked()
                {
                    self.apply_icon_theme();
                }
                if let Some(theme) = self.selected_theme.and_then(|i| self.themes.get(i)) {
                    ui.label(
                        egui::RichText::new(format!("→ {}", theme.dir_name)).weak(),
                    );
                }
            });
            ui.add_space(2.0);
            ui.separator();
            let is_err = self.icon_status.starts_with("error:");
            let color = if is_err {
                egui::Color32::from_rgb(0xff, 0x6b, 0x6b)
            } else {
                ui.visuals().text_color()
            };
            ui.label(egui::RichText::new(&self.icon_status).color(color));
            ui.add_space(2.0);
        });
    }

    fn icons_central_panel(&mut self, ctx: &egui::Context) {
        // Kick off preview rendering for the selected theme (lazy + cached).
        self.request_selected_previews();

        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(theme) = self.selected_theme.and_then(|i| self.themes.get(i)) else {
                ui.centered_and_justified(|ui| {
                    ui.label("Select an icon theme on the left to preview it.");
                });
                return;
            };
            let dir = theme.dir_name.clone();
            let display = theme.display_name.clone();

            ui.heading(&display);
            ui.label(egui::RichText::new(&dir).weak());
            ui.separator();

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .drag_to_scroll(false)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for slot in icons::SAMPLE_ICONS {
                            self.icon_preview_cell(ui, &dir, slot);
                        }
                    });
                });
        });
    }

    /// Draw one sample-icon preview cell (image + name label).
    fn icon_preview_cell(&self, ui: &mut egui::Ui, theme_dir: &str, names: &[&str]) {
        let px = iconload::ICON_PX as f32;
        let cell = egui::vec2(px + 24.0, px + 28.0);
        let key: IconKey = (
            theme_dir.to_string(),
            names.first().copied().unwrap_or("").to_string(),
        );
        let frame = egui::Frame::group(ui.style()).inner_margin(4.0);
        frame.show(ui, |ui| {
            ui.set_min_size(cell);
            ui.set_max_size(cell);
            ui.vertical_centered(|ui| {
                let area = egui::vec2(px, px);
                if let Some(tex) = self.icon_texs.get(&key) {
                    let sized = egui::load::SizedTexture::new(tex.id(), tex.size_vec2());
                    ui.add(
                        egui::Image::new(sized)
                            .max_size(area)
                            .fit_to_original_size(1.0),
                    );
                } else if self.icon_failed.contains(&key) {
                    let (rect, _) = ui.allocate_exact_size(area, egui::Sense::hover());
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "—",
                        egui::FontId::proportional(18.0),
                        ui.visuals().weak_text_color(),
                    );
                } else {
                    let (rect, _) = ui.allocate_exact_size(area, egui::Sense::hover());
                    ui.put(rect, egui::Spinner::new());
                }
                ui.label(egui::RichText::new(names[0]).small());
            });
        });
    }
}

/// Truncate a label to `max` chars with an ellipsis (keeps the grid tidy).
fn elide(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

// Suppress dead-code warning: `Mode::from_token` is used by `--restore` paths
// indirectly via config strings, but kept public-ish for symmetry/testing.
#[allow(dead_code)]
fn _mode_roundtrip(s: &str) -> &'static str {
    Mode::from_token(s).token()
}
