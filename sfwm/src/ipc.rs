//! The IPC command dispatcher — sommerflusswm's `herbstclient`-style control surface.
//!
//! `sc` connects to the Unix socket, sends a command as NUL-separated argument
//! bytes, half-closes its write side, and reads the reply text back. This module
//! parses one such request and mutates [`State`] accordingly. It runs on the same
//! thread as the Wayland dispatch (via calloop), so it can touch `State` directly
//! with no locking; after any change it calls `request_manage()` so river runs a
//! manage/render pass.
//!
//! Milestone 2 implements the monitor surface the autostart leans on
//! (set_monitors / add_monitor / raise_monitor / lock_tag / pad / focus_monitor /
//! cycle_monitor), plus enough (`use`, `move`, `spawn`, introspection) to actually
//! drive and demonstrate overlapping monitors. The full attribute tree, keybinds,
//! and the frame tree come in later milestones.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::frame;
use crate::monitor::{Insets, MonitorSel, Rect, TagId};
use crate::{Rule, State};

/// Read one request from `stream`, dispatch it, and write the reply back.
pub fn handle_connection(mut stream: UnixStream, state: &mut State) {
    // Guard against a client that connects but never closes its write side.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));

    let mut buf = Vec::new();
    // The client half-closes after sending, so read_to_end returns at EOF.
    let _ = stream.read_to_end(&mut buf);

    let args: Vec<String> = buf
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();

    // `sc --idle`: keep the connection open and stream hooks to it. The client
    // half-closed its write side (as every client does), so we only ever write.
    if matches!(args.first().map(String::as_str), Some("--idle") | Some("-i")) {
        let _ = stream.set_read_timeout(None);
        state.idle_clients.push(stream);
        return;
    }

    let reply = dispatch(state, &args);
    let _ = stream.write_all(reply.as_bytes());
    let _ = stream.flush();
}

/// Dispatch a parsed command. Returns the reply text (errors are plain text
/// prefixed with `error:`). Called both from the socket and from key bindings.
pub(crate) fn dispatch(state: &mut State, args: &[String]) -> String {
    let Some(cmd) = args.first() else {
        return err("empty command");
    };
    let rest = &args[1..];

    match cmd.as_str() {
        "set_monitors" => cmd_set_monitors(state, rest),
        "add_monitor" => cmd_add_monitor(state, rest),
        "raise_monitor" => cmd_raise_monitor(state, rest),
        "lock_tag" => cmd_lock_tag(state, rest),
        "pad" => cmd_pad(state, rest),
        "focus_monitor" => cmd_focus_monitor(state, rest),
        "cycle_monitor" => cmd_cycle_monitor(state, rest),
        "detect_monitors" => {
            state.auto_monitors = true;
            state.monitors.list.clear();
            state.maybe_detect_monitors();
            state.request_manage();
            ok()
        }
        "remove_monitor" => cmd_remove_monitor(state, rest),
        "unlock_tag" => cmd_unlock_tag(state, rest),
        "move_monitor" => cmd_move_monitor(state, rest),
        "rename_monitor" => cmd_rename_monitor(state, rest),
        "shift_to_monitor" => cmd_shift_to_monitor(state, rest),
        "use" => cmd_use(state, rest),
        "use_index" => cmd_use_index(state, rest),
        "use_previous" => cmd_use_previous(state),
        "merge_tag" => cmd_merge_tag(state, rest),
        "move" => cmd_move(state, rest),
        "bring" => cmd_bring(state, rest),
        "jumpto" => cmd_jumpto(state, rest),
        "raise" => {
            state.restack_focused(true);
            ok()
        }
        "lower" => {
            state.restack_focused(false);
            ok()
        }
        "spawn" => cmd_spawn(rest),
        "keybind" => cmd_keybind(state, rest),
        "keyunbind" => cmd_keyunbind(state, rest),
        // frame tree
        "split" => cmd_split(state, rest),
        "focus" => cmd_focus(state, rest),
        "shift" => cmd_shift(state, rest),
        "resize" => cmd_resize(state, rest),
        "remove" => cmd_remove(state),
        "cycle" => cmd_cycle(state, rest),
        "cycle_all" => cmd_cycle_all(state, rest),
        "cycle_frame" => {
            let d = rest.first().and_then(|s| s.parse::<i32>().ok()).unwrap_or(1);
            state.focused_tree_mut().cycle_frame(d);
            state.request_manage();
            ok()
        }
        "focus_frame" => match rest.first().and_then(|s| s.parse::<usize>().ok()) {
            Some(n) => {
                state.focused_tree_mut().focus_nth_leaf(n);
                state.request_manage();
                ok()
            }
            None => err("focus_frame: expected a frame index"),
        },
        "set_layout" => cmd_set_layout(state, rest),
        "cycle_layout" => cmd_cycle_layout(state),
        "set" => cmd_set(state, rest),
        // per-window modes
        "close" => cmd_close(state),
        "close_or_remove" => {
            if state.focused_window().is_some() {
                cmd_close(state)
            } else {
                cmd_remove(state)
            }
        }
        "fullscreen" => cmd_window_mode(state, rest, WinMode::Fullscreen),
        "pseudotile" => cmd_window_mode(state, rest, WinMode::Pseudotile),
        "floating" => cmd_floating(state, rest),
        "floating_geometry" => cmd_floating_geometry(state, rest),
        // mouse, rules, affinity
        "mousebind" => cmd_mousebind(state, rest),
        "mouseunbind" => cmd_mouseunbind(state, rest),
        "rule" => cmd_rule(state, rest),
        "unrule" => cmd_unrule(state),
        "set_tag_monitor" => cmd_set_tag_monitor(state, rest),
        "list_monitors" => list_monitors(state),
        "list_clients" => list_clients(state),
        "list_outputs" => list_outputs(state),
        "list_keybinds" => list_keybinds(state),
        "list_rules" => list_rules(state),
        "tag_status" => tag_status(state, rest),
        "emit_hook" => {
            let parts: Vec<&str> = rest.iter().map(|s| s.as_str()).collect();
            state.emit_hook(&parts);
            ok()
        }
        "version" => format!("sommerflusswm {}\n", env!("CARGO_PKG_VERSION")),
        // --- object/attribute tree ---
        "attr" => crate::attr::list(state, rest.first().map(|s| s.as_str())),
        "get_attr" => match rest.first() {
            Some(p) => match crate::attr::get(state, p) {
                Ok(v) => format!("{v}\n"),
                Err(e) => err(&e),
            },
            None => err("get_attr: expected an attribute path"),
        },
        "set_attr" => {
            if rest.len() < 2 {
                err("set_attr: expected <path> <value>")
            } else {
                match crate::attr::set(state, &rest[0], &rest[1]) {
                    Ok(()) => ok(),
                    Err(e) => err(&e),
                }
            }
        }
        "new_attr" => {
            if rest.len() < 2 {
                err("new_attr: expected <type> <my_name>")
            } else {
                match crate::attr::new_attr(state, &rest[0], &rest[1]) {
                    Ok(()) => ok(),
                    Err(e) => err(&e),
                }
            }
        }
        "remove_attr" => match rest.first() {
            Some(p) => match crate::attr::remove_attr(state, p) {
                Ok(()) => ok(),
                Err(e) => err(&e),
            },
            None => err("remove_attr: expected an attribute path"),
        },
        // --- command combinators ---
        "chain" => cmd_chain(state, rest, ChainMode::Chain),
        "and" => cmd_chain(state, rest, ChainMode::And),
        "or" => cmd_chain(state, rest, ChainMode::Or),
        "!" | "negate" => cmd_negate(state, rest),
        "try" => {
            let reply = dispatch(state, rest);
            if reply.starts_with("error:") { ok() } else { reply }
        }
        "silent" => {
            let reply = dispatch(state, rest);
            if reply.starts_with("error:") { err("(silenced)") } else { String::new() }
        }
        "compare" => cmd_compare(state, rest),
        "substitute" => cmd_substitute(state, rest),
        "sprintf" => cmd_sprintf(state, rest),
        "echo" => format!("{}\n", rest.join(" ")),
        "true" => ok(),
        "false" => err("false"),
        "dump" => cmd_dump_layout(state, rest),
        "load" => cmd_load(state, rest),
        "dump_tree" => cmd_dump_tree(state),
        "quit" => {
            std::process::exit(0);
        }
        "reload" => {
            let sock = crate::socket_path();
            crate::spawn_autostart(&sock);
            ok()
        }
        other => err(&format!("unknown command: {other}")),
    }
}

// --- monitor commands ----------------------------------------------------------

fn cmd_set_monitors(state: &mut State, rest: &[String]) -> String {
    if rest.is_empty() {
        return err("set_monitors: expected at least one WxH+X+Y rect");
    }
    let mut rects = Vec::with_capacity(rest.len());
    for r in rest {
        match Rect::parse(r) {
            Some(rect) => rects.push(rect),
            None => return err(&format!("set_monitors: bad rect '{r}'")),
        }
    }
    state.monitors.set_monitors(&rects);
    state.auto_monitors = false; // explicit topology — don't auto-clobber on hotplug
    state.request_manage();
    ok()
}

fn cmd_add_monitor(state: &mut State, rest: &[String]) -> String {
    // add_monitor <rect> <tag> [name]
    if rest.len() < 2 {
        return err("add_monitor: expected <rect> <tag> [name]");
    }
    let Some(rect) = Rect::parse(&rest[0]) else {
        return err(&format!("add_monitor: bad rect '{}'", rest[0]));
    };
    let Some(tag) = parse_tag(&rest[1]) else {
        return err(&format!("add_monitor: bad tag '{}'", rest[1]));
    };
    let name = rest.get(2).cloned();
    let id = state.monitors.add_monitor(rect, tag, name);
    state.request_manage();
    format!("monitor {}\n", id.0)
}

fn cmd_raise_monitor(state: &mut State, rest: &[String]) -> String {
    let Some(sel) = rest.first() else {
        return err("raise_monitor: expected a monitor selector");
    };
    if state.monitors.raise_monitor(&MonitorSel::parse(sel)) {
        state.request_manage();
        ok()
    } else {
        err(&format!("raise_monitor: no such monitor '{sel}'"))
    }
}

fn cmd_lock_tag(state: &mut State, rest: &[String]) -> String {
    // lock_tag <tag> <monitor>
    if rest.len() < 2 {
        return err("lock_tag: expected <tag> <monitor>");
    }
    let Some(tag) = parse_tag(&rest[0]) else {
        return err(&format!("lock_tag: bad tag '{}'", rest[0]));
    };
    if state.monitors.lock_tag(tag, &MonitorSel::parse(&rest[1])) {
        state.request_manage();
        ok()
    } else {
        err(&format!("lock_tag: no such monitor '{}'", rest[1]))
    }
}

fn cmd_pad(state: &mut State, rest: &[String]) -> String {
    // pad <monitor> <top> [right] [bottom] [left]  (hlwm edge order)
    if rest.len() < 2 {
        return err("pad: expected <monitor> <top> [right] [bottom] [left]");
    }
    let sel = MonitorSel::parse(&rest[0]);
    let nums: Result<Vec<i32>, _> = rest[1..].iter().map(|s| s.parse::<i32>()).collect();
    let Ok(n) = nums else {
        return err("pad: edge values must be integers");
    };
    // hlwm fills missing edges by repeating the last given value's CSS-like rules;
    // we keep it simple: missing edges default to 0.
    let pad = Insets {
        top: n.first().copied().unwrap_or(0),
        right: n.get(1).copied().unwrap_or(0),
        bottom: n.get(2).copied().unwrap_or(0),
        left: n.get(3).copied().unwrap_or(0),
    };
    if state.monitors.set_pad(&sel, pad) {
        state.request_manage();
        ok()
    } else {
        err(&format!("pad: no such monitor '{}'", rest[0]))
    }
}

fn cmd_focus_monitor(state: &mut State, rest: &[String]) -> String {
    let Some(sel) = rest.first() else {
        return err("focus_monitor: expected a monitor selector");
    };
    if state.monitors.focus_monitor(&MonitorSel::parse(sel)) {
        state.request_manage();
        ok()
    } else {
        err(&format!("focus_monitor: no such monitor '{sel}'"))
    }
}

fn cmd_cycle_monitor(state: &mut State, rest: &[String]) -> String {
    let delta = rest.first().and_then(|s| s.parse::<i32>().ok()).unwrap_or(1);
    state.monitors.cycle_monitor(delta);
    state.request_manage();
    ok()
}

fn cmd_remove_monitor(state: &mut State, rest: &[String]) -> String {
    let Some(sel) = rest.first() else {
        return err("remove_monitor: expected a monitor selector");
    };
    if state.monitors.remove_monitor(&MonitorSel::parse(sel)) {
        state.request_manage();
        ok()
    } else {
        err(&format!("remove_monitor: no such monitor '{sel}'"))
    }
}

fn cmd_unlock_tag(state: &mut State, rest: &[String]) -> String {
    let Some(sel) = rest.first() else {
        return err("unlock_tag: expected a monitor selector");
    };
    if state.monitors.unlock_tag(&MonitorSel::parse(sel)) {
        state.request_manage();
        ok()
    } else {
        err(&format!("unlock_tag: no such monitor '{sel}'"))
    }
}

fn cmd_move_monitor(state: &mut State, rest: &[String]) -> String {
    if rest.len() < 2 {
        return err("move_monitor: expected <monitor> <rect>");
    }
    let Some(rect) = Rect::parse(&rest[1]) else {
        return err(&format!("move_monitor: bad rect '{}'", rest[1]));
    };
    if state.monitors.move_monitor(&MonitorSel::parse(&rest[0]), rect) {
        state.request_manage();
        ok()
    } else {
        err(&format!("move_monitor: no such monitor '{}'", rest[0]))
    }
}

fn cmd_rename_monitor(state: &mut State, rest: &[String]) -> String {
    if rest.len() < 2 {
        return err("rename_monitor: expected <monitor> <name>");
    }
    let name = if rest[1].is_empty() { None } else { Some(rest[1].clone()) };
    if state.monitors.rename_monitor(&MonitorSel::parse(&rest[0]), name) {
        ok()
    } else {
        err(&format!("rename_monitor: no such monitor '{}'", rest[0]))
    }
}

/// `shift_to_monitor <monitor>` — move the focused window to the tag currently
/// shown on that monitor.
fn cmd_shift_to_monitor(state: &mut State, rest: &[String]) -> String {
    let Some(sel) = rest.first() else {
        return err("shift_to_monitor: expected a monitor selector");
    };
    let Some(tag) = state.monitors.tag_of(&MonitorSel::parse(sel)) else {
        return err(&format!("shift_to_monitor: no such monitor '{sel}'"));
    };
    move_focused_to_tag(state, tag)
}

// --- tag / window commands -----------------------------------------------------

fn cmd_use(state: &mut State, rest: &[String]) -> String {
    let Some(tag) = rest.first().and_then(|s| parse_tag(s)) else {
        return err("use: expected a tag");
    };
    // hlwm `my_monitor`: if the tag has a home monitor, focus it first so the
    // tag is shown there rather than on whatever monitor is currently focused.
    if let Some(sel) = state.tag_monitor.get(&tag).cloned() {
        state.monitors.focus_monitor(&sel);
    }
    state.remember_prev_tag();
    state.monitors.show_on_focused(tag);
    emit_tag_changed(state);
    state.request_manage();
    ok()
}

/// Emit the `tag_changed <tag> <monitor>` hook for the focused monitor.
fn emit_tag_changed(state: &mut State) {
    let tag = state.focused_tag().to_string();
    let mon = state.monitors.focus.to_string();
    state.emit_hook(&["tag_changed", &tag, &mon]);
}

/// `use_previous` — toggle the focused monitor back to the tag it last showed.
fn cmd_use_previous(state: &mut State) -> String {
    let Some(prev) = state.prev_tag.get(&state.monitors.focus).copied() else {
        return ok(); // nothing shown before yet
    };
    state.remember_prev_tag();
    state.monitors.show_on_focused(prev);
    emit_tag_changed(state);
    state.request_manage();
    ok()
}

/// `merge_tag <src> [target]` — move every window from tag `src` into `target`
/// (default: the focused tag), then leave `src` empty.
fn cmd_merge_tag(state: &mut State, rest: &[String]) -> String {
    let Some(src) = rest.first().and_then(|s| parse_tag(s)) else {
        return err("merge_tag: expected <src> [target]");
    };
    let target = rest.get(1).and_then(|s| parse_tag(s)).unwrap_or_else(|| state.focused_tag());
    if src == target {
        return ok();
    }
    let windows = state.tags.get(&src).map(|t| t.all_windows()).unwrap_or_default();
    if let Some(t) = state.tags.get_mut(&src) {
        *t = frame::Frame::new();
    }
    for wid in windows {
        if let Some(w) = state.windows.get_mut(&wid) {
            w.tag = target;
        }
        state.tag_tree_mut(target).insert_window(wid);
    }
    state.request_manage();
    ok()
}

/// `use_index <i> [--skip-visible]`. Absolute index is 0-based over tags 1..=9
/// (matching hlwm). A leading `+`/`-` makes it relative to the focused monitor's
/// current tag, optionally skipping tags already visible on another monitor.
fn cmd_use_index(state: &mut State, rest: &[String]) -> String {
    let Some(arg) = rest.first() else {
        return err("use_index: expected an index");
    };
    let skip_visible = rest.iter().any(|a| a == "--skip-visible");

    let tags: Vec<TagId> = (1..=9).collect();
    let cur = state.monitors.focused().map(|m| m.tag).unwrap_or(1);
    let cur_pos = tags.iter().position(|&t| t == cur).unwrap_or(0);

    let target_tag = if arg.starts_with('+') || arg.starts_with('-') {
        let Ok(delta) = arg.parse::<i32>() else {
            return err(&format!("use_index: bad relative index '{arg}'"));
        };
        let n = tags.len() as i32;
        let mut pos = cur_pos as i32;
        // Step at least once, then keep stepping over visible tags if requested.
        let step = if delta >= 0 { 1 } else { -1 };
        let mut remaining = delta.abs().max(1);
        let mut guard = 0;
        while remaining > 0 && guard < n {
            pos = (pos + step).rem_euclid(n);
            let t = tags[pos as usize];
            if skip_visible && state.monitors.tag_visible(t) {
                guard += 1;
                continue;
            }
            remaining -= 1;
            guard = 0;
        }
        tags[pos as usize]
    } else {
        let Ok(i) = arg.parse::<usize>() else {
            return err(&format!("use_index: bad index '{arg}'"));
        };
        match tags.get(i) {
            Some(&t) => t,
            None => return err(&format!("use_index: index {i} out of range")),
        }
    };

    state.remember_prev_tag();
    state.monitors.show_on_focused(target_tag);
    emit_tag_changed(state);
    state.request_manage();
    ok()
}

fn cmd_move(state: &mut State, rest: &[String]) -> String {
    let Some(tag) = rest.first().and_then(|s| parse_tag(s)) else {
        return err("move: expected a tag");
    };
    move_focused_to_tag(state, tag)
}

/// Move the focused window from its current tag's tree into `tag`'s tree.
fn move_focused_to_tag(state: &mut State, tag: TagId) -> String {
    let Some(wid) = state.focused_window() else {
        return err("move: no focused window");
    };
    let from = match state.windows.get(&wid) {
        Some(w) => w.tag,
        None => return err("move: focused window missing"),
    };
    if from == tag {
        return ok();
    }
    if let Some(tree) = state.tags.get_mut(&from) {
        tree.remove_window(wid);
    }
    if let Some(w) = state.windows.get_mut(&wid) {
        w.tag = tag;
    }
    state.tag_tree_mut(tag).insert_window(wid);
    state.request_manage();
    ok()
}

/// `bring <winid>` — pull a window (by its `win=` id from `list_clients`) onto
/// the focused tag's focused frame.
fn cmd_bring(state: &mut State, rest: &[String]) -> String {
    let Some(wid) = rest.first().and_then(|s| s.parse::<frame::WinId>().ok()) else {
        return err("bring: expected a window id");
    };
    let Some(from) = state.windows.get(&wid).map(|w| w.tag) else {
        return err(&format!("bring: no such window {wid}"));
    };
    let to = state.focused_tag();
    if from != to {
        if let Some(tree) = state.tags.get_mut(&from) {
            tree.remove_window(wid);
        }
        if let Some(w) = state.windows.get_mut(&wid) {
            w.tag = to;
        }
        state.focused_tree_mut().insert_window(wid);
    } else {
        state.focused_tree_mut().focus_window(wid);
    }
    state.request_manage();
    ok()
}

/// `jumpto <winid>` — focus a window by its `win=` id (and the monitor showing it).
fn cmd_jumpto(state: &mut State, rest: &[String]) -> String {
    let Some(wid) = rest.first().and_then(|s| s.parse::<frame::WinId>().ok()) else {
        return err("jumpto: expected a window id");
    };
    if !state.windows.contains_key(&wid) {
        return err(&format!("jumpto: no such window {wid}"));
    }
    state.focus_window_by_id(wid);
    state.request_manage();
    ok()
}

// --- frame tree ----------------------------------------------------------------

fn cmd_split(state: &mut State, rest: &[String]) -> String {
    let Some(dir) = rest.first().and_then(|s| frame::SplitDir::parse(s)) else {
        return err("split: expected top|bottom|left|right|horizontal|vertical|explode");
    };
    let frac = rest.get(1).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.5);
    state.focused_tree_mut().split(dir, frac);
    state.request_manage();
    ok()
}

fn cmd_focus(state: &mut State, rest: &[String]) -> String {
    let Some(dir) = rest.first().and_then(|s| frame::Dir::parse(s)) else {
        return err("focus: expected left|down|up|right");
    };
    let area = state.focused_area();
    let gap = state.window_gap;
    state.focused_tree_mut().focus_dir(dir, area, gap);
    state.request_manage();
    ok()
}

fn cmd_shift(state: &mut State, rest: &[String]) -> String {
    let Some(dir) = rest.first().and_then(|s| frame::Dir::parse(s)) else {
        return err("shift: expected left|down|up|right");
    };
    let area = state.focused_area();
    let gap = state.window_gap;
    state.focused_tree_mut().shift_dir(dir, area, gap);
    state.request_manage();
    ok()
}

fn cmd_resize(state: &mut State, rest: &[String]) -> String {
    let Some(dir) = rest.first().and_then(|s| frame::Dir::parse(s)) else {
        return err("resize: expected left|down|up|right <delta>");
    };
    // hlwm passes a fraction like +0.02; accept a bare or signed float.
    let delta = rest
        .get(1)
        .and_then(|s| s.trim_start_matches('+').parse::<f64>().ok())
        .unwrap_or(0.02);
    state.focused_tree_mut().resize(dir, delta);
    state.request_manage();
    ok()
}

fn cmd_remove(state: &mut State) -> String {
    state.focused_tree_mut().remove_frame();
    state.request_manage();
    ok()
}

fn cmd_cycle(state: &mut State, rest: &[String]) -> String {
    let delta = rest.first().and_then(|s| s.parse::<i32>().ok()).unwrap_or(1);
    state.focused_tree_mut().cycle(delta);
    state.request_manage();
    ok()
}

fn cmd_cycle_all(state: &mut State, rest: &[String]) -> String {
    let delta = rest.first().and_then(|s| s.parse::<i32>().ok()).unwrap_or(1);
    let tree = state.focused_tree_mut();
    let all = tree.all_windows();
    if all.is_empty() {
        return ok();
    }
    let cur = tree.focused_window();
    let idx = cur.and_then(|w| all.iter().position(|&x| x == w)).unwrap_or(0) as i32;
    let n = all.len() as i32;
    let next = all[(((idx + delta) % n + n) % n) as usize];
    tree.focus_window(next);
    state.request_manage();
    ok()
}

fn cmd_set_layout(state: &mut State, rest: &[String]) -> String {
    let Some(layout) = rest.first().and_then(|s| frame::LayoutMode::parse(s)) else {
        return err("set_layout: expected max|vertical|horizontal|grid");
    };
    state.focused_tree_mut().set_layout(layout);
    state.request_manage();
    ok()
}

fn cmd_cycle_layout(state: &mut State) -> String {
    state.focused_tree_mut().cycle_layout();
    state.request_manage();
    ok()
}

/// `set <name> <value>` — layout and theme settings.
fn cmd_set(state: &mut State, rest: &[String]) -> String {
    let (Some(name), Some(v)) = (rest.first().map(|s| s.as_str()), rest.get(1)) else {
        return err("set: expected <name> <value>");
    };
    match name {
        "window_gap" | "frame_gap" => match v.parse::<i32>() {
            Ok(n) => state.window_gap = n.max(0),
            Err(_) => return err("set window_gap: expected an integer"),
        },
        "border_width" => match v.parse::<i32>() {
            Ok(n) => state.border_width = n.max(0),
            Err(_) => return err("set border_width: expected an integer"),
        },
        "border_color_active" | "frame_border_active_color" => match parse_color(v) {
            Some(c) => state.border_active = c,
            None => return err("set: bad colour (use #rrggbb)"),
        },
        "border_color_normal" | "frame_border_normal_color" => match parse_color(v) {
            Some(c) => state.border_normal = c,
            None => return err("set: bad colour (use #rrggbb)"),
        },
        "border_color_urgent" => match parse_color(v) {
            Some(c) => state.border_urgent = c,
            None => return err("set: bad colour (use #rrggbb)"),
        },
        "focus_follows_mouse" => state.focus_follows_mouse = parse_bool(v),
        "inactive_dim" => match v.parse::<f64>() {
            Ok(n) => {
                state.inactive_dim = n.clamp(0.0, 1.0);
                // Drop the cached buffer so the next render rebuilds it at the
                // new alpha.
                if let Some(b) = state.dim_buffer.take() {
                    b.destroy();
                }
            }
            Err(_) => return err("set inactive_dim: expected a number 0.0..=1.0"),
        },
        "raise_on_focus" => state.raise_on_focus = parse_bool(v),
        "smart_frame_surroundings" => state.smart_frame_surroundings = parse_bool(v),
        "smart_window_surroundings" => state.smart_window_surroundings = parse_bool(v),
        "default_frame_layout" => match frame::LayoutMode::parse(v) {
            Some(l) => state.default_frame_layout = l,
            None => return err("set default_frame_layout: expected max|vertical|horizontal|grid"),
        },
        // Unknown settings are accepted-and-ignored so autostart ports don't fail.
        _ => return ok(),
    }
    state.request_manage();
    ok()
}

// --- per-window modes ----------------------------------------------------------

enum WinMode {
    Fullscreen,
    Pseudotile,
}

fn cmd_close(state: &mut State) -> String {
    match state.focused_window() {
        Some(wid) => {
            state.pending_close.push(wid);
            state.request_manage();
            ok()
        }
        None => err("close: no focused window"),
    }
}

fn cmd_window_mode(state: &mut State, rest: &[String], mode: WinMode) -> String {
    let Some(wid) = state.focused_window() else {
        return err("no focused window");
    };
    let arg = rest.first().map(|s| s.as_str()).unwrap_or("toggle");
    if let Some(w) = state.windows.get_mut(&wid) {
        let cur = match mode {
            WinMode::Fullscreen => w.fullscreen,
            WinMode::Pseudotile => w.pseudotile,
        };
        let v = match arg {
            "on" | "true" => true,
            "off" | "false" => false,
            _ => !cur,
        };
        match mode {
            WinMode::Fullscreen => w.fullscreen = v,
            WinMode::Pseudotile => w.pseudotile = v,
        }
    }
    state.request_manage();
    ok()
}

fn cmd_floating(state: &mut State, rest: &[String]) -> String {
    let Some(wid) = state.focused_window() else {
        return err("floating: no focused window");
    };
    let arg = rest.first().map(|s| s.as_str()).unwrap_or("toggle");
    let cur = state.windows.get(&wid).map_or(false, |w| w.floating);
    let want = match arg {
        "on" | "true" => true,
        "off" | "false" => false,
        _ => !cur,
    };
    if want && !cur {
        let geo = state
            .last_rects
            .get(&wid)
            .copied()
            .unwrap_or_else(|| state.default_float_geo());
        state.make_floating(wid, geo);
    } else if !want && cur {
        if let Some(w) = state.windows.get_mut(&wid) {
            w.floating = false; // returns to the tiling (still a tree leaf)
        }
    }
    state.request_manage();
    ok()
}

/// `floating_geometry WxH+X+Y` — set the focused window's floating rect (and
/// float it). Used by the resize-viewport.sh port.
fn cmd_floating_geometry(state: &mut State, rest: &[String]) -> String {
    let Some(g) = rest.first().and_then(|s| Rect::parse(s)) else {
        return err("floating_geometry: expected WxH+X+Y");
    };
    let Some(wid) = state.focused_window() else {
        return err("floating_geometry: no focused window");
    };
    state.make_floating(wid, g);
    if let Some(w) = state.windows.get_mut(&wid) {
        w.floating = true;
        w.float_geo = g;
    }
    state.request_manage();
    ok()
}

// --- mouse, rules, tag affinity ------------------------------------------------

fn cmd_mousebind(state: &mut State, rest: &[String]) -> String {
    if rest.len() < 2 {
        return err("mousebind: expected <mods+ButtonN> <move|resize|zoom>");
    }
    let (mods, button) = match parse_mousebutton(&rest[0]) {
        Ok(v) => v,
        Err(e) => return err(&format!("mousebind: {e}")),
    };
    let resize = match rest[1].as_str() {
        "move" => false,
        "resize" | "zoom" => true,
        other => return err(&format!("mousebind: unknown action '{other}'")),
    };
    match state.add_mousebind(mods, button, resize) {
        Ok(()) => ok(),
        Err(e) => err(&format!("mousebind: {e}")),
    }
}

fn cmd_mouseunbind(state: &mut State, _rest: &[String]) -> String {
    state.clear_mousebinds();
    ok()
}

/// `rule [app_id=foo|app_id~re] [title~re] [tag=N] [floating=on] [pseudotile=on]`.
/// `class` is accepted as an alias for `app_id`; `focus`/`manage`/`windowtype`
/// are accepted and ignored.
fn cmd_rule(state: &mut State, rest: &[String]) -> String {
    let mut rule = Rule {
        app_id: None,
        title: None,
        tag: None,
        monitor: None,
        floating: None,
        pseudotile: None,
        focus: None,
        switchtag: None,
    };
    for tok in rest {
        let (key, exact, val) = if let Some(i) = tok.find('~') {
            (&tok[..i], false, &tok[i + 1..])
        } else if let Some(i) = tok.find('=') {
            (&tok[..i], true, &tok[i + 1..])
        } else {
            continue;
        };
        let on = parse_bool(val);
        match key {
            "app_id" | "class" | "instance" => rule.app_id = Some((exact, val.to_string())),
            "title" => rule.title = Some((exact, val.to_string())),
            "tag" => rule.tag = parse_tag(val),
            "monitor" => rule.monitor = Some(MonitorSel::parse(val)),
            "floating" => rule.floating = Some(on),
            "pseudotile" => rule.pseudotile = Some(on),
            "focus" => rule.focus = Some(on),
            "switchtag" => rule.switchtag = Some(on),
            // accepted but not yet acted on
            "manage" | "windowtype" => {}
            _ => {}
        }
    }
    state.rules.push(rule);
    ok()
}

fn cmd_unrule(state: &mut State) -> String {
    state.rules.clear();
    ok()
}

fn cmd_set_tag_monitor(state: &mut State, rest: &[String]) -> String {
    if rest.len() < 2 {
        return err("set_tag_monitor: expected <tag> <monitor>");
    }
    let Some(tag) = parse_tag(&rest[0]) else {
        return err(&format!("set_tag_monitor: bad tag '{}'", rest[0]));
    };
    state.tag_monitor.insert(tag, MonitorSel::parse(&rest[1]));
    ok()
}

fn cmd_spawn(rest: &[String]) -> String {
    let Some((cmd, args)) = rest.split_first() else {
        return err("spawn: expected a command");
    };
    match std::process::Command::new(cmd).args(args).spawn() {
        Ok(_) => ok(),
        Err(e) => err(&format!("spawn: {e}")),
    }
}

// --- key bindings --------------------------------------------------------------

/// `keybind <mods+key> <command...>` — e.g. `keybind Mod4+Return spawn alacritty`.
/// Modifiers and key are separated by `+` or `-` (so hlwm's `Mod4-Return` works);
/// the last token is the key. The command runs through this same dispatcher when
/// the binding fires.
fn cmd_keybind(state: &mut State, rest: &[String]) -> String {
    if rest.len() < 2 {
        return err("keybind: expected <mods+key> <command...>");
    }
    let (mods, keysym) = match parse_binding(&rest[0]) {
        Ok(v) => v,
        Err(e) => return err(&format!("keybind: {e}")),
    };
    match state.add_keybind(rest[0].clone(), mods, keysym, rest[1..].to_vec()) {
        Ok(()) => ok(),
        Err(e) => err(&format!("keybind: {e}")),
    }
}

/// `keyunbind [--all]` — currently only clears every binding.
fn cmd_keyunbind(state: &mut State, _rest: &[String]) -> String {
    state.clear_keybinds();
    state.request_manage();
    ok()
}

/// Parse a binding spec into `(modifier bits, xkbcommon keysym)`. Modifier bits
/// use the `river_seat_v1.modifiers` values (shift=1, ctrl=4, mod1/alt=8,
/// mod3=32, mod4/super=64, mod5=128). The key name is resolved by libxkbcommon,
/// so anything from `Return` to `XF86AudioRaiseVolume` works. We resolve the
/// *base* (unshifted) keysym — river's base-layer match path compares modifiers
/// exactly and the level-0 keysym, so `Mod4+Shift+s` is keysym `s` + {mod4,shift}.
fn parse_binding(spec: &str) -> Result<(u32, u32), String> {
    let parts: Vec<&str> = spec.split(['+', '-']).filter(|s| !s.is_empty()).collect();
    let Some((key, mod_toks)) = parts.split_last() else {
        return Err(format!("empty binding '{spec}'"));
    };
    let mods = parse_mods(mod_toks)?;
    let keysym = xkbcommon::xkb::keysym_from_name(key, xkbcommon::xkb::KEYSYM_NO_FLAGS);
    let raw = keysym.raw();
    if raw == 0 {
        return Err(format!("unknown key '{key}'"));
    }
    Ok((mods, raw))
}

/// Parse modifier tokens into `river_seat_v1.modifiers` bits.
fn parse_mods(toks: &[&str]) -> Result<u32, String> {
    let mut mods = 0u32;
    for m in toks {
        mods |= match m.to_ascii_lowercase().as_str() {
            "mod4" | "super" | "logo" | "win" | "mod" => 64,
            "mod1" | "alt" => 8,
            "control" | "ctrl" => 4,
            "shift" => 1,
            "mod3" => 32,
            "mod5" => 128,
            other => return Err(format!("unknown modifier '{other}'")),
        };
    }
    Ok(mods)
}

/// Parse a `<mods+ButtonN>` spec into (modifier bits, Linux button code).
fn parse_mousebutton(spec: &str) -> Result<(u32, u32), String> {
    let parts: Vec<&str> = spec.split(['+', '-']).filter(|s| !s.is_empty()).collect();
    let Some((btn, mod_toks)) = parts.split_last() else {
        return Err(format!("empty mousebind '{spec}'"));
    };
    let mods = parse_mods(mod_toks)?;
    // hlwm Button1/2/3 = left/middle/right → BTN_LEFT/MIDDLE/RIGHT.
    let button = match *btn {
        "Button1" => 0x110,
        "Button2" => 0x112,
        "Button3" => 0x111,
        "Button4" => 0x113,
        "Button5" => 0x114,
        other => return Err(format!("unknown button '{other}'")),
    };
    Ok((mods, button))
}

/// Parse `#rrggbb` or `#rrggbbaa` into an RGBA byte tuple.
fn parse_color(s: &str) -> Option<(u8, u8, u8, u8)> {
    let s = s.trim_start_matches('#');
    let byte = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
    match s.len() {
        6 => Some((byte(0)?, byte(2)?, byte(4)?, 0xff)),
        8 => Some((byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
        _ => None,
    }
}

// --- introspection -------------------------------------------------------------

fn list_monitors(state: &State) -> String {
    let mut out = String::new();
    for (i, m) in state.monitors.list.iter().enumerate() {
        let focus = if i == state.monitors.focus { " [focused]" } else { "" };
        let name = m.name.as_deref().unwrap_or("-");
        let lock = m.locked_tag.map(|t| t.to_string()).unwrap_or_else(|| "-".into());
        out.push_str(&format!(
            "{i}: id={} name={name} {}x{}{:+}{:+} z={} tag={} lock={lock}{focus}\n",
            m.id.0, m.rect.w, m.rect.h, m.rect.x, m.rect.y, m.z, m.tag,
        ));
    }
    if out.is_empty() {
        out.push_str("(no monitors)\n");
    }
    out
}

fn list_clients(state: &State) -> String {
    let focused = state.focused_window();
    let mut lines: Vec<(u64, String)> = state
        .windows
        .iter()
        .map(|(wid, w)| {
            let app = w.app_id.as_deref().unwrap_or("-");
            let title = w.title.as_deref().unwrap_or("-");
            let mark = if Some(*wid) == focused { " [focused]" } else { "" };
            (*wid, format!("win={wid} tag={} app_id={app} title={title}{mark}\n", w.tag))
        })
        .collect();
    lines.sort_by_key(|(wid, _)| *wid);
    let mut out: String = lines.into_iter().map(|(_, l)| l).collect();
    if out.is_empty() {
        out.push_str("(no clients)\n");
    }
    out
}

/// `dump [tag]` — serialize a tag's frame tree to a loadable layout string
/// (hlwm `dump`). Default: the focused tag.
fn cmd_dump_layout(state: &State, rest: &[String]) -> String {
    let tag = rest.first().and_then(|s| parse_tag(s)).unwrap_or_else(|| state.focused_tag());
    match state.tags.get(&tag) {
        Some(t) => format!("{}\n", t.serialize()),
        None => "(clients vertical:0)\n".to_string(),
    }
}

/// `load [tag] <layout>` — restore a tag's frame tree from a layout string
/// (hlwm `load`). If the first token is a bare tag number it selects the tag;
/// otherwise the whole argument list is the layout for the focused tag.
fn cmd_load(state: &mut State, rest: &[String]) -> String {
    if rest.is_empty() {
        return err("load: expected [tag] <layout>");
    }
    let first_is_tag = !rest[0].starts_with('(') && parse_tag(&rest[0]).is_some() && rest.len() > 1;
    let (tag, layout) = if first_is_tag {
        (parse_tag(&rest[0]).unwrap(), rest[1..].join(" "))
    } else {
        (state.focused_tag(), rest.join(" "))
    };
    match state.load_layout(tag, &layout) {
        Ok(()) => ok(),
        Err(e) => err(&e),
    }
}

/// Inspect the focused tag's frame tree and the rects it computes — lets the
/// frame tree be verified headlessly (no display needed).
fn cmd_dump_tree(state: &State) -> String {
    let tag = state.focused_tag();
    let area = state.focused_area();
    let gap = state.window_gap;
    let Some(tree) = state.tags.get(&tag) else {
        return format!("tag {tag}: empty\n");
    };
    let mut out = format!(
        "tag {tag} area={}x{}{:+}{:+} gap={gap}\n",
        area.w, area.h, area.x, area.y
    );
    out.push_str(&tree.describe());
    out.push_str("placements:\n");
    for p in tree.placements(area, gap) {
        out.push_str(&format!(
            "  win={} {}x{}{:+}{:+} visible={}\n",
            p.win, p.rect.w, p.rect.h, p.rect.x, p.rect.y, p.visible
        ));
    }
    out
}

fn list_outputs(state: &State) -> String {
    let mut out = String::new();
    for o in state.outputs.values() {
        out.push_str(&format!(
            "{}x{}{:+}{:+} wl_output={}\n",
            o.geo.w,
            o.geo.h,
            o.geo.x,
            o.geo.y,
            o.wl_output_name.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
        ));
    }
    if out.is_empty() {
        out.push_str("(no outputs)\n");
    }
    out
}

fn list_keybinds(state: &State) -> String {
    let mut lines: Vec<String> = state
        .keybinds
        .values()
        .map(|kb| format!("{}\t{}\n", kb.spec, kb.command.join(" ")))
        .collect();
    lines.sort();
    let out: String = lines.into_iter().collect();
    if out.is_empty() {
        "(no keybinds)\n".to_string()
    } else {
        out
    }
}

/// `tag_status [monitor]` — a panel-friendly view of tags 1..9 for the given
/// monitor (default: focused). Each entry is `<prefix><tag>`, tab-separated:
///   `#` focused here · `%` visible on another monitor · `!` urgent ·
///   `:` occupied (has windows) · `.` empty.
fn tag_status(state: &State, rest: &[String]) -> String {
    let mon_idx = match rest.first() {
        Some(s) => state.monitors.resolve(&MonitorSel::parse(s)).unwrap_or(state.monitors.focus),
        None => state.monitors.focus,
    };
    let cur_tag = state.monitors.list.get(mon_idx).map(|m| m.tag);
    let mut out = String::new();
    for tag in 1..=9u32 {
        let occupied = state.tags.get(&tag).map_or(false, |t| !t.all_windows().is_empty());
        let urgent = state.windows.values().any(|w| w.tag == tag && w.urgent);
        let visible = state.monitors.tag_visible(tag);
        let prefix = if Some(tag) == cur_tag {
            '#'
        } else if urgent {
            '!'
        } else if visible {
            '%'
        } else if occupied {
            ':'
        } else {
            '.'
        };
        out.push(prefix);
        out.push_str(&tag.to_string());
        out.push('\t');
    }
    out.push('\n');
    out
}

fn list_rules(state: &State) -> String {
    let mut out = String::new();
    for (i, r) in state.rules.iter().enumerate() {
        out.push_str(&format!("{i}:{}\n", r.describe()));
    }
    if out.is_empty() {
        out.push_str("(no rules)\n");
    }
    out
}

// --- command combinators -------------------------------------------------------

enum ChainMode {
    Chain,
    And,
    Or,
}

fn failed(reply: &str) -> bool {
    reply.starts_with("error:")
}

/// Split `args` into groups on the separator token `sep`.
fn split_groups(args: &[String], sep: &str) -> Vec<Vec<String>> {
    let mut groups = Vec::new();
    let mut cur = Vec::new();
    for a in args {
        if a == sep {
            groups.push(std::mem::take(&mut cur));
        } else {
            cur.push(a.clone());
        }
    }
    groups.push(cur);
    groups
}

/// `chain`/`and`/`or <SEP> cmd1 <SEP> cmd2 …`. Chain runs all and concatenates
/// output; `and` stops at the first failure; `or` stops at the first success.
fn cmd_chain(state: &mut State, rest: &[String], mode: ChainMode) -> String {
    let Some(sep) = rest.first() else {
        return err("chain: expected a separator token");
    };
    let groups = split_groups(&rest[1..], sep);
    let mut acc = String::new();
    let mut last = ok();
    for g in groups {
        if g.is_empty() {
            continue;
        }
        let reply = dispatch(state, &g);
        match mode {
            ChainMode::Chain => acc.push_str(&reply),
            ChainMode::And => {
                last = reply.clone();
                if failed(&reply) {
                    return reply;
                }
            }
            ChainMode::Or => {
                if !failed(&reply) {
                    return reply;
                }
                last = reply;
            }
        }
    }
    match mode {
        ChainMode::Chain => acc,
        _ => last,
    }
}

/// `! cmd…` / `negate cmd…` — invert a command's success status.
fn cmd_negate(state: &mut State, rest: &[String]) -> String {
    if rest.is_empty() {
        return err("negate: expected a command");
    }
    let reply = dispatch(state, rest);
    if failed(&reply) {
        ok()
    } else {
        err("negate: command succeeded")
    }
}

/// `compare <path> <op> <value>` — succeed iff the comparison holds. Numeric if
/// both sides parse as integers, else lexicographic. Ops: = != lt le gt ge.
fn cmd_compare(state: &mut State, rest: &[String]) -> String {
    if rest.len() < 3 {
        return err("compare: expected <path> <op> <value>");
    }
    let lhs = match crate::attr::get(state, &rest[0]) {
        Ok(v) => v,
        Err(e) => return err(&e),
    };
    let (op, rhs) = (rest[1].as_str(), rest[2].as_str());
    let ord = match (lhs.parse::<i64>(), rhs.parse::<i64>()) {
        (Ok(a), Ok(b)) => a.cmp(&b),
        _ => lhs.as_str().cmp(rhs),
    };
    use std::cmp::Ordering::*;
    let truth = match op {
        "=" | "==" => ord == Equal,
        "!=" => ord != Equal,
        "lt" => ord == Less,
        "le" => ord != Greater,
        "gt" => ord == Greater,
        "ge" => ord != Less,
        _ => return err(&format!("compare: unknown operator '{op}'")),
    };
    if truth {
        ok()
    } else {
        err("compare: false")
    }
}

/// `substitute IDENT ATTR CMD…` — replace whole-token occurrences of IDENT in
/// the command with the value of attribute ATTR, then run it.
fn cmd_substitute(state: &mut State, rest: &[String]) -> String {
    if rest.len() < 3 {
        return err("substitute: expected <ident> <attr> <command...>");
    }
    let ident = rest[0].clone();
    let val = match crate::attr::get(state, &rest[1]) {
        Ok(v) => v,
        Err(e) => return err(&e),
    };
    let cmd: Vec<String> = rest[2..]
        .iter()
        .map(|a| if *a == ident { val.clone() } else { a.clone() })
        .collect();
    dispatch(state, &cmd)
}

/// `sprintf IDENT FORMAT [ATTR…] CMD…` — build a string from FORMAT (only `%s`
/// supported), consuming one attribute per `%s`, bind it to IDENT, then run CMD.
fn cmd_sprintf(state: &mut State, rest: &[String]) -> String {
    if rest.len() < 2 {
        return err("sprintf: expected <ident> <format> [attrs...] <command...>");
    }
    let ident = rest[0].clone();
    let fmt = rest[1].clone();
    let n = fmt.matches("%s").count();
    if rest.len() < 2 + n + 1 {
        return err("sprintf: not enough arguments for the format");
    }
    let mut result = fmt;
    for i in 0..n {
        let v = crate::attr::get(state, &rest[2 + i]).unwrap_or_else(|_| rest[2 + i].clone());
        result = result.replacen("%s", &v, 1);
    }
    let cmd: Vec<String> = rest[2 + n..]
        .iter()
        .map(|a| if *a == ident { result.clone() } else { a.clone() })
        .collect();
    dispatch(state, &cmd)
}

// --- helpers -------------------------------------------------------------------

fn parse_tag(s: &str) -> Option<TagId> {
    s.parse::<TagId>().ok()
}

fn parse_bool(s: &str) -> bool {
    matches!(s, "on" | "true" | "1")
}

fn ok() -> String {
    "ok\n".to_string()
}

fn err(msg: &str) -> String {
    format!("error: {msg}\n")
}

#[cfg(test)]
mod tests {
    use super::{parse_binding, split_groups};

    #[test]
    fn chain_splits_on_separator() {
        let args: Vec<String> = ["use", "2", ",", "spawn", "alacritty"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let g = split_groups(&args, ",");
        assert_eq!(g.len(), 2);
        assert_eq!(g[0], vec!["use", "2"]);
        assert_eq!(g[1], vec!["spawn", "alacritty"]);
    }

    // xkbcommon keysyms: ASCII letters/digits map to their code points; named
    // keys use the X keysym values.
    #[test]
    fn binding_modifiers_and_base_keysym() {
        // Mod4 = 64, Shift = 1; 'q' base keysym = 0x71 (we register the UNSHIFTED
        // keysym so river's base-layer match handles Mod4+Shift+q).
        assert_eq!(parse_binding("Mod4-Shift-q"), Ok((65, 0x71)));
        assert_eq!(parse_binding("Mod4+Return"), Ok((64, 0xff0d)));
        assert_eq!(parse_binding("super+1"), Ok((64, 0x31)));
        assert_eq!(parse_binding("Alt+Tab"), Ok((8, 0xff09)));
    }

    #[test]
    fn binding_resolves_xf86_keys() {
        // The whole reason for using libxkbcommon rather than an ASCII table.
        assert_eq!(parse_binding("XF86AudioRaiseVolume"), Ok((0, 0x1008ff13)));
    }

    #[test]
    fn binding_errors() {
        assert!(parse_binding("Mod9+q").is_err()); // unknown modifier
        assert!(parse_binding("Mod4+definitelynotakey").is_err()); // unknown key
    }
}
