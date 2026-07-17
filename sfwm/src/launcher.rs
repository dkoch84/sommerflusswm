//! App-launcher logic with no Wayland dependencies: enumerate `.desktop`
//! applications and fuzzy-match a query against them. The overlay rendering and
//! keyboard input live in `main.rs`; this module is the pure, testable core.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A launchable application parsed from a `.desktop` entry.
pub struct DesktopApp {
    pub name: String,
    pub exec: String,
}

/// Scan the XDG applications directories (in precedence order) and return the
/// launchable entries, de-duplicated by filename and sorted by name.
pub fn enumerate_apps() -> Vec<DesktopApp> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        let data_home =
            std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{home}/.local/share"));
        dirs.push(PathBuf::from(format!("{data_home}/applications")));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    for d in data_dirs.split(':').filter(|d| !d.is_empty()) {
        dirs.push(PathBuf::from(format!("{d}/applications")));
    }

    let mut seen = HashSet::new();
    let mut apps = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            // XDG precedence: earlier dirs win, so skip a basename already seen.
            let Some(base) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
                continue;
            };
            if !seen.insert(base) {
                continue;
            }
            if let Some(app) = parse_desktop(&path) {
                apps.push(app);
            }
        }
    }
    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    apps
}

/// Parse the `[Desktop Entry]` group; return None for hidden/non-application/
/// no-exec entries.
fn parse_desktop(path: &Path) -> Option<DesktopApp> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_entry = false;
    let (mut name, mut exec, mut typ) = (None, None, None);
    let mut hidden = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        if let Some(v) = line.strip_prefix("Name=") {
            name.get_or_insert_with(|| v.to_string());
        } else if let Some(v) = line.strip_prefix("Exec=") {
            exec.get_or_insert_with(|| v.to_string());
        } else if let Some(v) = line.strip_prefix("Type=") {
            typ = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("NoDisplay=") {
            hidden |= v.eq_ignore_ascii_case("true");
        } else if let Some(v) = line.strip_prefix("Hidden=") {
            hidden |= v.eq_ignore_ascii_case("true");
        }
    }
    if hidden {
        return None;
    }
    if matches!(&typ, Some(t) if t != "Application") {
        return None;
    }
    let name = name?;
    let exec = clean_exec(&exec?);
    if exec.is_empty() {
        return None;
    }
    Some(DesktopApp { name, exec })
}

/// Strip `.desktop` Exec field codes (`%f %u %U %i %c %k` …) and collapse spaces.
fn clean_exec(exec: &str) -> String {
    let mut out = String::new();
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            chars.next(); // drop the code letter
        } else {
            out.push(c);
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Fuzzy subsequence score of `query` within `text` (case-insensitive). `None`
/// means no match; higher is better (consecutive + word-start matches score more).
pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let mut qi = 0;
    let mut score = 0i32;
    let mut prev: Option<usize> = None;
    for (ti, &tc) in t.iter().enumerate() {
        if qi >= q.len() {
            break;
        }
        if tc == q[qi] {
            score += 1;
            if prev == Some(ti.wrapping_sub(1)) {
                score += 5; // consecutive run
            }
            if ti == 0 || !t[ti - 1].is_alphanumeric() {
                score += 3; // start of a word
            }
            prev = Some(ti);
            qi += 1;
        }
    }
    (qi == q.len()).then(|| score - t.len() as i32 / 20)
}

/// Indices into `entries` that match `query`, best match first. Works for both
/// app names (drun) and arbitrary dmenu lines.
pub fn filter(entries: &[String], query: &str) -> Vec<usize> {
    let mut scored: Vec<(i32, usize)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| fuzzy_score(query, e).map(|s| (s, i)))
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| entries[a.1].len().cmp(&entries[b.1].len()))
    });
    scored.into_iter().map(|(_, i)| i).collect()
}
