//! Icon-theme discovery, lookup and application.
//!
//! We scan the freedesktop icon-theme base directories, parse each theme's
//! `index.theme`, resolve representative icon files (PNG or SVG) for previews,
//! and — on Apply — write the chosen theme's *directory name* into
//! `~/.config/qt6ct/qt6ct.conf` under `[Appearance] icon_theme=`.
//!
//! Absolutely no GTK / gsettings / portal involvement: qt6ct.conf is a plain
//! INI file we edit line-by-line, preserving everything else.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Sample icon "slots" shown in the preview panel. Each slot lists alternative
/// names tried in order — the first that resolves in the theme is used. The
/// cache key for a slot is its first name.
pub const SAMPLE_ICONS: &[&[&str]] = &[
    &["folder"],
    &["user-home"],
    &["text-editor", "accessories-text-editor"],
    &["web-browser", "firefox"],
    &["utilities-terminal", "terminal"],
    &["system-file-manager"],
    &["applications-system"],
];

/// A discovered installable icon theme.
#[derive(Clone, Debug)]
pub struct IconTheme {
    /// Directory name — the stable identifier written to config (e.g.
    /// `Papirus-Dark`).
    pub dir_name: String,
    /// Human-readable `Name=` from `index.theme` (falls back to `dir_name`).
    pub display_name: String,
}

/// The freedesktop icon base directories, in search-precedence order, skipping
/// any that don't exist.
pub fn icon_base_dirs() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        v.push(PathBuf::from(&home).join(".icons"));
        v.push(PathBuf::from(&home).join(".local/share/icons"));
    }
    v.push(PathBuf::from("/usr/local/share/icons"));
    v.push(PathBuf::from("/usr/share/icons"));
    if let Ok(dirs) = std::env::var("XDG_DATA_DIRS") {
        for d in dirs.split(':') {
            if !d.is_empty() {
                v.push(PathBuf::from(d).join("icons"));
            }
        }
    }
    // De-duplicate (XDG_DATA_DIRS commonly already includes /usr/share).
    let mut seen = std::collections::HashSet::new();
    v.retain(|p| seen.insert(p.clone()));
    v.into_iter().filter(|p| p.is_dir()).collect()
}

/// A parsed `index.theme` (only the fields we care about).
#[derive(Clone, Debug, Default)]
struct ThemeIndex {
    name: Option<String>,
    directories: Vec<String>,
    inherits: Vec<String>,
    /// Per-subdirectory nominal `Size=` (from each directory's own group).
    sizes: HashMap<String, i32>,
    has_directories_key: bool,
}

/// Parse an `index.theme` file. Returns `None` if it has no `[Icon Theme]`
/// group (not a valid theme).
fn parse_index_theme(path: &Path) -> Option<ThemeIndex> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut idx = ThemeIndex::default();
    let mut cur_group = String::new();
    let mut saw_icon_theme = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            cur_group = rest.strip_suffix(']').unwrap_or(rest).to_string();
            if cur_group == "Icon Theme" {
                saw_icon_theme = true;
            }
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim();
        if cur_group == "Icon Theme" {
            match key {
                "Name" => idx.name = Some(val.to_string()),
                "Directories" => {
                    idx.has_directories_key = true;
                    idx.directories = val
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                "Inherits" => {
                    idx.inherits = val
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                _ => {}
            }
        } else if key == "Size" {
            if let Ok(sz) = val.parse::<i32>() {
                idx.sizes.insert(cur_group.clone(), sz);
            }
        }
    }
    if saw_icon_theme {
        Some(idx)
    } else {
        None
    }
}

/// Scan all base dirs and return installable icon themes, sorted by display
/// name. Skips `hicolor`/`default`, cursor-only themes, and de-duplicates by
/// directory name (first occurrence wins).
pub fn scan_themes(base_dirs: &[PathBuf]) -> Vec<IconTheme> {
    let mut by_dir: HashMap<String, IconTheme> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for base in base_dirs {
        let Ok(rd) = std::fs::read_dir(base) else {
            continue;
        };
        for entry in rd.filter_map(|e| e.ok()) {
            let theme_dir = entry.path();
            if !theme_dir.is_dir() {
                continue;
            }
            let dir_name = match theme_dir.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if dir_name == "hicolor" || dir_name == "default" {
                continue;
            }
            if by_dir.contains_key(&dir_name) {
                continue; // earlier base dir wins
            }
            let index_path = theme_dir.join("index.theme");
            let Some(idx) = parse_index_theme(&index_path) else {
                continue;
            };
            // Skip cursor-only themes: no Directories and a cursors/ subdir.
            if idx.directories.is_empty() && theme_dir.join("cursors").is_dir() {
                continue;
            }
            let display_name = idx.name.clone().unwrap_or_else(|| dir_name.clone());
            order.push(dir_name.clone());
            by_dir.insert(
                dir_name.clone(),
                IconTheme {
                    dir_name,
                    display_name,
                },
            );
        }
    }
    let mut themes: Vec<IconTheme> = order
        .into_iter()
        .filter_map(|d| by_dir.remove(&d))
        .collect();
    themes.sort_by(|a, b| {
        a.display_name
            .to_ascii_lowercase()
            .cmp(&b.display_name.to_ascii_lowercase())
            .then_with(|| a.dir_name.cmp(&b.dir_name))
    });
    themes
}

const ICON_EXTS: &[&str] = &["png", "svg"];

/// Resolve an icon file for `icon_name` within `theme_dir_name`, preferring a
/// directory whose `Size=` is closest to `target_size`. Falls back through the
/// theme's `Inherits=` parents and finally `hicolor`.
pub fn resolve_icon(
    theme_dir_name: &str,
    icon_name: &str,
    base_dirs: &[PathBuf],
    target_size: i32,
) -> Option<PathBuf> {
    let mut visited = std::collections::HashSet::new();
    resolve_in_theme(theme_dir_name, icon_name, base_dirs, target_size, &mut visited)
}

fn resolve_in_theme(
    theme_dir_name: &str,
    icon_name: &str,
    base_dirs: &[PathBuf],
    target_size: i32,
    visited: &mut std::collections::HashSet<String>,
) -> Option<PathBuf> {
    if !visited.insert(theme_dir_name.to_string()) {
        return None;
    }

    // Locate + parse this theme's index.theme (first base dir that has it).
    let mut idx: Option<ThemeIndex> = None;
    for base in base_dirs {
        let p = base.join(theme_dir_name).join("index.theme");
        if p.is_file() {
            if let Some(parsed) = parse_index_theme(&p) {
                idx = Some(parsed);
                break;
            }
        }
    }
    let idx = idx?;

    // Order this theme's directories by |Size - target| (unknown sizes last).
    let mut dirs: Vec<(&String, i32)> = idx
        .directories
        .iter()
        .map(|d| (d, *idx.sizes.get(d).unwrap_or(&0)))
        .collect();
    dirs.sort_by_key(|(_, sz)| (sz - target_size).abs());

    for (subdir, _) in &dirs {
        for base in base_dirs {
            let dir = base.join(theme_dir_name).join(subdir);
            for ext in ICON_EXTS {
                let cand = dir.join(format!("{icon_name}.{ext}"));
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }

    // Inherited parents.
    for parent in &idx.inherits {
        if let Some(found) =
            resolve_in_theme(parent, icon_name, base_dirs, target_size, visited)
        {
            return Some(found);
        }
    }

    // Final fallback to hicolor (unless we already are it).
    if theme_dir_name != "hicolor" && !visited.contains("hicolor") {
        if let Some(found) =
            resolve_in_theme("hicolor", icon_name, base_dirs, target_size, visited)
        {
            return Some(found);
        }
    }

    None
}

/// Resolve the first name in a sample slot that exists for the theme.
pub fn resolve_slot(
    theme_dir_name: &str,
    names: &[&str],
    base_dirs: &[PathBuf],
    target_size: i32,
) -> Option<PathBuf> {
    for name in names {
        if let Some(p) = resolve_icon(theme_dir_name, name, base_dirs, target_size) {
            return Some(p);
        }
    }
    None
}

/// Path to `~/.config/qt6ct/qt6ct.conf`.
pub fn qt6ct_conf_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    Path::new(&home)
        .join(".config")
        .join("qt6ct")
        .join("qt6ct.conf")
}

/// Write `icon_theme=<dir_name>` into `[Appearance]` in qt6ct.conf, preserving
/// every other line/group. Creates the file and directory if absent.
pub fn apply_qt6ct_icon_theme(dir_name: &str) -> Result<(), String> {
    let path = qt6ct_conf_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }

    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let new_contents = set_ini_key(&existing, "Appearance", "icon_theme", dir_name);

    std::fs::write(&path, new_contents).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(())
}

/// Line-based INI edit: set `key=value` inside `[group]`, preserving all other
/// content. If the key exists in the group its line is replaced; if the group
/// exists but not the key, the key is inserted at the end of that group; if the
/// group is absent it is appended.
fn set_ini_key(input: &str, group: &str, key: &str, value: &str) -> String {
    let target_header = format!("[{group}]");
    let mut lines: Vec<String> = input.lines().map(|l| l.to_string()).collect();

    // Find the target group's header line.
    let mut group_start: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == target_header {
            group_start = Some(i);
            break;
        }
    }

    match group_start {
        Some(start) => {
            // Scan the group body until the next header; replace or insert.
            let mut insert_at = lines.len();
            let mut replaced = false;
            for i in (start + 1)..lines.len() {
                let t = lines[i].trim();
                if t.starts_with('[') && t.ends_with(']') {
                    insert_at = i;
                    break;
                }
                if let Some((k, _)) = t.split_once('=') {
                    if k.trim() == key {
                        lines[i] = format!("{key}={value}");
                        replaced = true;
                        break;
                    }
                }
            }
            if !replaced {
                // Insert just before the next header (or at EOF), trimming a
                // trailing blank line inside the group for tidiness.
                let mut at = insert_at;
                while at > start + 1 && lines[at - 1].trim().is_empty() {
                    at -= 1;
                }
                lines.insert(at, format!("{key}={value}"));
            }
        }
        None => {
            if !lines.is_empty() && !lines.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
                lines.push(String::new());
            }
            lines.push(target_header);
            lines.push(format!("{key}={value}"));
        }
    }

    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::set_ini_key;

    #[test]
    fn replaces_existing_key() {
        let input = "[Appearance]\nicon_theme=Old\nstyle=Fusion\n";
        let out = set_ini_key(input, "Appearance", "icon_theme", "New");
        assert!(out.contains("icon_theme=New"));
        assert!(out.contains("style=Fusion"));
        assert!(!out.contains("Old"));
    }

    #[test]
    fn inserts_key_in_existing_group() {
        let input = "[Appearance]\nstyle=Fusion\n\n[Fonts]\nfixed=x\n";
        let out = set_ini_key(input, "Appearance", "icon_theme", "New");
        assert!(out.contains("icon_theme=New"));
        assert!(out.contains("[Fonts]"));
        assert!(out.contains("style=Fusion"));
    }

    #[test]
    fn appends_missing_group() {
        let input = "[Fonts]\nfixed=x\n";
        let out = set_ini_key(input, "Appearance", "icon_theme", "New");
        assert!(out.contains("[Appearance]"));
        assert!(out.contains("icon_theme=New"));
        assert!(out.contains("[Fonts]"));
    }

    #[test]
    fn from_empty() {
        let out = set_ini_key("", "Appearance", "icon_theme", "New");
        assert_eq!(out, "[Appearance]\nicon_theme=New\n");
    }
}
