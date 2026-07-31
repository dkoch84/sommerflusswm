//! Config file handling for the wallpaper browser.
//!
//! Lives at `~/.config/sommerflusswm/wallpaper.conf` and uses a simple,
//! human-readable, line-based format (whitespace-separated). It mirrors what
//! `nitrogen` persists, so that `sfwm-appearance --restore` can re-apply the
//! last saved wallpaper(s) at login.
//!
//! Line grammar (blank lines and `#` comments are ignored):
//!
//! ```text
//! dir <abs-path>                  # a directory shown in the browse list
//! saved <monitor> <mode> <path>   # a saved image selection
//! saved-color <monitor> <#rrggbb> # a saved solid colour
//! icon-theme <dir-name>           # the last chosen icon theme (dir name)
//! ```
//!
//! `<monitor>` is `all` or a numeric index. For `saved`/`saved-color`, last
//! write wins per monitor (we de-duplicate on save).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// A persisted wallpaper selection for a given monitor.
#[derive(Clone, Debug)]
pub enum Saved {
    /// An image at `path` rendered with `mode` (fill/fit/stretch/center/tile).
    Image { mode: String, path: String },
    /// A solid colour, e.g. `#1e1e2e`.
    Color { color: String },
}

/// The full parsed config.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// Directories shown in the left-hand browse list (order preserved).
    pub dirs: Vec<String>,
    /// Saved selections keyed by monitor (`all` or an index). Sorted for
    /// stable output; last write wins per monitor.
    pub saved: BTreeMap<String, Saved>,
    /// The last chosen icon theme, stored as its *directory name* (e.g.
    /// `Papirus-Dark`). Only used to pre-select in the Icons tab; the actual
    /// applied value lives in `qt6ct.conf`.
    pub icon_theme: Option<String>,
    /// The last chosen cursor theme (directory name) — applied via
    /// `sc cursor_theme` on save and on `--restore`.
    pub cursor_theme: Option<String>,
    /// Cursor size for the theme above (defaults to 24).
    pub cursor_size: Option<u32>,
}

/// Resolve `~/.config/sommerflusswm/wallpaper.conf` via `$HOME`.
pub fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    Path::new(&home)
        .join(".config")
        .join("sommerflusswm")
        .join("wallpaper.conf")
}

impl Config {
    /// Load the config, returning an empty config if the file is missing or
    /// unreadable (a fresh install simply has nothing saved yet).
    pub fn load() -> Config {
        let path = config_path();
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => return Config::default(),
        };
        let mut cfg = Config::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Split into at most 4 fields; the path/value is the remainder so it
            // may itself contain spaces.
            let mut it = line.splitn(2, char::is_whitespace);
            let kind = it.next().unwrap_or("");
            let rest = it.next().unwrap_or("").trim();
            match kind {
                "dir" => {
                    if !rest.is_empty() && !cfg.dirs.iter().any(|d| d == rest) {
                        cfg.dirs.push(rest.to_string());
                    }
                }
                "saved" => {
                    // saved <monitor> <mode> <path>
                    let mut p = rest.splitn(3, char::is_whitespace);
                    let (mon, mode, path) = (p.next(), p.next(), p.next());
                    if let (Some(mon), Some(mode), Some(path)) = (mon, mode, path) {
                        let path = path.trim();
                        if !path.is_empty() {
                            cfg.saved.insert(
                                mon.to_string(),
                                Saved::Image {
                                    mode: mode.to_string(),
                                    path: path.to_string(),
                                },
                            );
                        }
                    }
                }
                "saved-color" => {
                    // saved-color <monitor> <#rrggbb>
                    let mut p = rest.splitn(2, char::is_whitespace);
                    let (mon, color) = (p.next(), p.next());
                    if let (Some(mon), Some(color)) = (mon, color) {
                        let color = color.trim();
                        if !color.is_empty() {
                            cfg.saved.insert(
                                mon.to_string(),
                                Saved::Color {
                                    color: color.to_string(),
                                },
                            );
                        }
                    }
                }
                "icon-theme" => {
                    if !rest.is_empty() {
                        cfg.icon_theme = Some(rest.to_string());
                    }
                }
                "cursor-theme" => {
                    if !rest.is_empty() {
                        cfg.cursor_theme = Some(rest.to_string());
                    }
                }
                "cursor-size" => {
                    if let Ok(n) = rest.parse::<u32>() {
                        cfg.cursor_size = Some(n);
                    }
                }
                _ => { /* ignore unknown directives for forward-compat */ }
            }
        }
        cfg
    }

    /// Write the config to disk, creating parent directories as needed.
    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = String::new();
        out.push_str("# sommerflusswm wallpaper browser config\n");
        out.push_str("# managed by sfwm-appearance; edits are preserved on the\n");
        out.push_str("# next save only for recognised directives.\n");
        for d in &self.dirs {
            out.push_str(&format!("dir {d}\n"));
        }
        for (mon, saved) in &self.saved {
            match saved {
                Saved::Image { mode, path } => {
                    out.push_str(&format!("saved {mon} {mode} {path}\n"));
                }
                Saved::Color { color } => {
                    out.push_str(&format!("saved-color {mon} {color}\n"));
                }
            }
        }
        if let Some(theme) = &self.icon_theme {
            out.push_str(&format!("icon-theme {theme}\n"));
        }
        if let Some(theme) = &self.cursor_theme {
            out.push_str(&format!("cursor-theme {theme}\n"));
        }
        if let Some(size) = self.cursor_size {
            out.push_str(&format!("cursor-size {size}\n"));
        }
        let mut f = fs::File::create(&path)?;
        f.write_all(out.as_bytes())?;
        Ok(())
    }

    /// Record/replace the saved selection for `monitor`.
    pub fn set_saved(&mut self, monitor: &str, saved: Saved) {
        self.saved.insert(monitor.to_string(), saved);
    }

    /// Record the last chosen icon theme (its directory name).
    pub fn set_cursor_theme(&mut self, dir_name: &str, size: u32) {
        self.cursor_theme = Some(dir_name.to_string());
        self.cursor_size = Some(size);
    }

    pub fn set_icon_theme(&mut self, dir_name: &str) {
        self.icon_theme = Some(dir_name.to_string());
    }
}
