//! The object/attribute tree — herbstluftwm's introspectable, scriptable model.
//!
//! hlwm exposes a filesystem-like tree of objects (`settings.`, `tags.`,
//! `clients.`, `monitors.`, plus user `my_*` attributes) addressed by
//! dot-separated paths, e.g. `settings.window_gap` or `clients.focus.title`.
//! This module resolves those paths against the live [`State`] for the
//! `attr` / `get_attr` / `set_attr` / `new_attr` / `remove_attr` commands and
//! the `compare`/`substitute` combinators built on top of them.
//!
//! It is a pragmatic subset: the most useful, panel- and script-relevant
//! attributes are present and the common ones are writable. Setting a settings
//! attribute delegates to the same code path as the `set` command.

use crate::frame::{Frame, LayoutMode, WinId};
use crate::State;

fn color(c: (u8, u8, u8, u8)) -> String {
    format!("#{:02x}{:02x}{:02x}{:02x}", c.0, c.1, c.2, c.3)
}

fn boolstr(b: bool) -> String {
    if b { "true".into() } else { "false".into() }
}

fn layout_name(l: LayoutMode) -> String {
    match l {
        LayoutMode::Max => "max",
        LayoutMode::Vertical => "vertical",
        LayoutMode::Horizontal => "horizontal",
        LayoutMode::Grid => "grid",
    }
    .into()
}

fn leaf_count(f: &Frame) -> usize {
    match f {
        Frame::Leaf(_) => 1,
        Frame::Split(s) => leaf_count(&s.children[0]) + leaf_count(&s.children[1]),
    }
}

/// The id of the focused window, if any.
fn focused(state: &State) -> Option<WinId> {
    state.focused_window()
}

// --- get -----------------------------------------------------------------------

/// Resolve a dot-path to a single attribute value (hlwm `get_attr`).
pub fn get(state: &State, path: &str) -> Result<String, String> {
    let p: Vec<&str> = path.split('.').collect();
    match p.as_slice() {
        ["settings", name] => settings_get(state, name),

        ["tags", "count"] => Ok(tag_ids(state).len().to_string()),
        ["tags", "focus"] => Ok(state.focused_tag().to_string()),
        ["tags", idx, attr] => {
            let tag = idx.parse().map_err(|_| format!("bad tag '{idx}'"))?;
            tag_get(state, tag, attr)
        }

        ["clients", "count"] => Ok(state.windows.len().to_string()),
        ["clients", "focus", attr] => match focused(state) {
            Some(w) => client_get(state, w, attr),
            None => Err("clients.focus: no focused window".into()),
        },
        ["clients", id, attr] => {
            let w = id.parse().map_err(|_| format!("bad window id '{id}'"))?;
            client_get(state, w, attr)
        }

        ["monitors", "count"] => Ok(state.monitors.list.len().to_string()),
        ["monitors", "focus"] => Ok(state.monitors.focus.to_string()),
        ["monitors", idx, attr] => {
            let i: usize = idx.parse().map_err(|_| format!("bad monitor '{idx}'"))?;
            monitor_get(state, i, attr)
        }

        [user] if user.starts_with("my_") => state
            .user_attrs
            .get(*user)
            .cloned()
            .ok_or_else(|| format!("no such attribute: {user}")),

        _ => Err(format!("no such attribute: {path}")),
    }
}

fn settings_get(state: &State, name: &str) -> Result<String, String> {
    Ok(match name {
        "window_gap" | "frame_gap" => state.window_gap.to_string(),
        "border_width" => state.border_width.to_string(),
        "border_color_active" => color(state.border_active),
        "border_color_normal" => color(state.border_normal),
        "border_color_urgent" => color(state.border_urgent),
        "focus_follows_mouse" => boolstr(state.focus_follows_mouse),
        "raise_on_focus" => boolstr(state.raise_on_focus),
        "smart_frame_surroundings" => boolstr(state.smart_frame_surroundings),
        "smart_window_surroundings" => boolstr(state.smart_window_surroundings),
        "default_frame_layout" => layout_name(state.default_frame_layout),
        "inactive_dim" => format!("{:.2}", state.inactive_dim),
        _ => return Err(format!("no such setting: {name}")),
    })
}

fn tag_get(state: &State, tag: u32, attr: &str) -> Result<String, String> {
    let tree = state.tags.get(&tag);
    Ok(match attr {
        "name" | "index" => tag.to_string(),
        "client_count" => tree.map_or(0, |t| t.all_windows().len()).to_string(),
        "frame_count" => tree.map_or(1, leaf_count).to_string(),
        "urgent_count" => state
            .windows
            .values()
            .filter(|w| w.tag == tag && w.urgent)
            .count()
            .to_string(),
        "visible" => boolstr(state.monitors.tag_visible(tag)),
        _ => return Err(format!("tags.{tag}: no such attribute: {attr}")),
    })
}

fn client_get(state: &State, wid: WinId, attr: &str) -> Result<String, String> {
    let w = state.windows.get(&wid).ok_or_else(|| format!("no such window: {wid}"))?;
    Ok(match attr {
        "winid" => wid.to_string(),
        "app_id" | "class" | "instance" => w.app_id.clone().unwrap_or_default(),
        "title" => w.title.clone().unwrap_or_default(),
        "tag" => w.tag.to_string(),
        "floating" => boolstr(w.floating),
        "fullscreen" => boolstr(w.fullscreen),
        "pseudotile" => boolstr(w.pseudotile),
        "urgent" => boolstr(w.urgent),
        "focused" => boolstr(focused(state) == Some(wid)),
        _ => return Err(format!("clients.{wid}: no such attribute: {attr}")),
    })
}

fn monitor_get(state: &State, i: usize, attr: &str) -> Result<String, String> {
    let m = state.monitors.list.get(i).ok_or_else(|| format!("no such monitor: {i}"))?;
    Ok(match attr {
        "name" => m.name.clone().unwrap_or_default(),
        "index" => i.to_string(),
        "tag" => m.tag.to_string(),
        "lock_tag" => m.locked_tag.map(|t| t.to_string()).unwrap_or_default(),
        "x" => m.rect.x.to_string(),
        "y" => m.rect.y.to_string(),
        "width" => m.rect.w.to_string(),
        "height" => m.rect.h.to_string(),
        "focused" => boolstr(i == state.monitors.focus),
        _ => return Err(format!("monitors.{i}: no such attribute: {attr}")),
    })
}

// --- set -----------------------------------------------------------------------

/// Set a writable attribute (hlwm `set_attr`).
pub fn set(state: &mut State, path: &str, val: &str) -> Result<(), String> {
    let p: Vec<&str> = path.split('.').collect();
    match p.as_slice() {
        ["settings", name] => {
            // Reuse the `set` command's parsing/validation and side effects.
            let reply = crate::ipc::dispatch(
                state,
                &["set".to_string(), name.to_string(), val.to_string()],
            );
            if let Some(rest) = reply.strip_prefix("error:") {
                Err(rest.trim().to_string())
            } else {
                Ok(())
            }
        }
        ["clients", "focus", attr] => match focused(state) {
            Some(w) => client_set(state, w, attr, val),
            None => Err("clients.focus: no focused window".into()),
        },
        ["clients", id, attr] => {
            let w = id.parse().map_err(|_| format!("bad window id '{id}'"))?;
            client_set(state, w, attr, val)
        }
        ["monitors", idx, attr] => {
            let i: usize = idx.parse().map_err(|_| format!("bad monitor '{idx}'"))?;
            monitor_set(state, i, attr, val)
        }
        [user] if user.starts_with("my_") => {
            if state.user_attrs.contains_key(*user) {
                state.user_attrs.insert(user.to_string(), val.to_string());
                Ok(())
            } else {
                Err(format!("no such attribute: {user} (create it with new_attr)"))
            }
        }
        _ => Err(format!("attribute not writable: {path}")),
    }
}

fn client_set(state: &mut State, wid: WinId, attr: &str, val: &str) -> Result<(), String> {
    let on = matches!(val, "on" | "true" | "1");
    if !state.windows.contains_key(&wid) {
        return Err(format!("no such window: {wid}"));
    }
    match attr {
        "fullscreen" => {
            if let Some(w) = state.windows.get_mut(&wid) {
                w.fullscreen = on;
            }
        }
        "pseudotile" => {
            if let Some(w) = state.windows.get_mut(&wid) {
                w.pseudotile = on;
            }
        }
        "urgent" => {
            if let Some(w) = state.windows.get_mut(&wid) {
                w.urgent = on;
            }
        }
        "floating" => {
            if on {
                let geo = state
                    .last_rects
                    .get(&wid)
                    .copied()
                    .unwrap_or_else(|| state.default_float_geo());
                state.make_floating(wid, geo);
            } else if let Some(w) = state.windows.get_mut(&wid) {
                w.floating = false;
            }
        }
        _ => return Err(format!("clients.{wid}: attribute not writable: {attr}")),
    }
    state.request_manage();
    Ok(())
}

fn monitor_set(state: &mut State, i: usize, attr: &str, val: &str) -> Result<(), String> {
    match attr {
        "tag" => {
            let tag = val.parse().map_err(|_| format!("bad tag '{val}'"))?;
            let m = state.monitors.list.get_mut(i).ok_or_else(|| format!("no such monitor: {i}"))?;
            if m.locked_tag.map_or(true, |lt| lt == tag) {
                m.tag = tag;
            }
        }
        "name" => {
            let m = state.monitors.list.get_mut(i).ok_or_else(|| format!("no such monitor: {i}"))?;
            m.name = if val.is_empty() { None } else { Some(val.to_string()) };
        }
        _ => return Err(format!("monitors.{i}: attribute not writable: {attr}")),
    }
    state.request_manage();
    Ok(())
}

// --- user attributes -----------------------------------------------------------

/// Create a user attribute (hlwm `new_attr <type> my_name`). The type is
/// validated against a default value but all values are stored as strings.
pub fn new_attr(state: &mut State, ty: &str, name: &str) -> Result<(), String> {
    if !name.starts_with("my_") {
        return Err("new_attr: user attribute names must start with 'my_'".into());
    }
    if state.user_attrs.contains_key(name) {
        return Err(format!("new_attr: {name} already exists"));
    }
    let default = match ty {
        "bool" => "false",
        "int" | "uint" => "0",
        "string" => "",
        "color" => "#000000ff",
        _ => return Err(format!("new_attr: unknown type '{ty}'")),
    };
    state.user_attrs.insert(name.to_string(), default.to_string());
    Ok(())
}

/// Remove a user attribute (hlwm `remove_attr`).
pub fn remove_attr(state: &mut State, name: &str) -> Result<(), String> {
    if state.user_attrs.remove(name).is_some() {
        Ok(())
    } else {
        Err(format!("remove_attr: no such attribute: {name}"))
    }
}

// --- listing -------------------------------------------------------------------

/// List an object's child objects and attributes (hlwm `attr [path]`). With no
/// path, lists the top-level objects. With an attribute path, prints its value.
pub fn list(state: &State, path: Option<&str>) -> String {
    let Some(path) = path else {
        let mut out = String::from("objects:\n");
        for o in ["clients.", "monitors.", "settings.", "tags."] {
            out.push_str(&format!("  {o}\n"));
        }
        out.push_str("attributes:\n");
        let mut us: Vec<&String> = state.user_attrs.keys().collect();
        us.sort();
        for u in us {
            out.push_str(&format!("  {u} = \"{}\"\n", state.user_attrs[u]));
        }
        return out;
    };
    // A concrete attribute path → just its value.
    if let Ok(v) = get(state, path) {
        return format!("{v}\n");
    }
    // Otherwise enumerate the named container's attributes.
    let names: &[&str] = match path {
        "settings" => &[
            "window_gap",
            "border_width",
            "border_color_active",
            "border_color_normal",
            "border_color_urgent",
            "focus_follows_mouse",
            "raise_on_focus",
            "smart_frame_surroundings",
            "smart_window_surroundings",
            "default_frame_layout",
            "inactive_dim",
        ],
        "clients.focus" => &["winid", "app_id", "title", "tag", "floating", "fullscreen", "pseudotile", "urgent"],
        _ => return format!("error: no such object: {path}\n"),
    };
    let mut out = String::new();
    for n in names {
        let v = get(state, &format!("{path}.{n}")).unwrap_or_default();
        out.push_str(&format!("{n} = \"{v}\"\n"));
    }
    out
}

/// Tag ids that currently have a frame tree (used for `tags.count`).
fn tag_ids(state: &State) -> Vec<u32> {
    let mut v: Vec<u32> = state.tags.keys().copied().collect();
    v.sort_unstable();
    v
}
