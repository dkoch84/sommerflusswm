//! sc — the sommerflusswm control client (sommerflusswm's `herbstclient`).
//!
//! Connects to the sfwm IPC socket, sends its arguments as one command, and
//! prints the reply. The config layer is a shell script that calls `sc` over and
//! over — a near-direct port of the hlwm `autostart` (`hc` becomes `sc`).
//!
//! Wire format: arguments are sent NUL-separated; the write side is then
//! half-closed so the server reads to EOF. The reply is plain text.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

/// Resolve the IPC socket path. Kept in sync (by duplication) with sfwm's
/// `socket_path()` — both honor `SOMMERFLUSSWM_SOCKET` first.
fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("SOMMERFLUSSWM_SOCKET") {
        return PathBuf::from(p);
    }
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".to_string());
    PathBuf::from(dir).join(format!("sfwm-{display}.sock"))
}

/// Full command reference. `sc help` / `-h` / `--help` / no args prints this; it
/// needs no running WM. Kept grouped to mirror the hlwm command surface.
const HELP: &str = "\
sc — sommerflusswm control client (the herbstclient of sfwm).
Usage: sc <command> [args...]   ·   config is a shell script calling sc repeatedly.

MONITORS
  set_monitors <WxH+X+Y>...        define the virtual monitor rects (tags 1..N)
  add_monitor <rect> <tag> [name]  add an overlapping/overlay monitor
  remove_monitor <sel>             remove a monitor (sel = index or name)
  move_monitor <sel> <rect>        change a monitor's rect
  rename_monitor <sel> <name>      name a monitor
  raise_monitor <sel>              stack a monitor above overlapping ones
  focus_monitor <sel>              focus a monitor
  cycle_monitor [+1|-1]            focus the next/prev monitor
  detect_monitors                  re-detect from physical outputs
  list_monitors | list_outputs     list virtual monitors / physical outputs
  lock_tag <tag> <sel> | unlock_tag <sel>
  shift_to_monitor <sel>           move focused window to a monitor
  set_tag_monitor <tag> <sel>      tag's home monitor (use focuses it first)
  pad <mon> <top> [right] [bottom] [left]   reserve edge space (e.g. the bar)

TAGS
  use <tag> | use_index <±N> [--skip-visible] | use_previous
  move <tag> | bring <tag>         send / pull the focused window
  merge_tag <src> [dst]            merge a tag's windows into another
  tag_status [mon]                 panel view of tags (#focused :occupied .empty …)

WINDOWS & FOCUS
  focus <dir> | shift <dir>        dir = left|right|up|down
  cycle [±1] | cycle_all [±1]      cycle within / across frames
  cycle_frame [±1] | focus_frame <dir>
  jumpto <sel> | raise | lower
  close | close_or_remove          close window / close-or-remove empty frame
  fullscreen [on|off|toggle]       pseudotile [on|off|toggle]
  floating [on|off|toggle]         floating_geometry <WxH+X+Y>

FRAMES & LAYOUT
  split <top|bottom|left|right|explode> [ratio]
  resize <dir> <±frac>             remove            (remove the focused frame)
  set_layout <vertical|horizontal|max|grid> | cycle_layout
  dump [tag] | load <tag> <layout> | dump_tree

INPUT BINDINGS
  keybind <mods+key> <command...>  e.g. keybind Super+Return spawn alacritty
  keyunbind <spec|--all> | list_keybinds
  mousebind <mods+button> <move|resize|zoom> | mouseunbind <--all>
  gesturebind <swipeN-dir> <command...>  e.g. gesturebind swipe3-left use_index +1
  gestureunbind <spec|--all> | list_gesturebinds   (needs user in `input` group)
  select_region      native region select: drag, or click a window to snap; prints
                     X,Y WxH (grim -g format); empty + Esc = cancelled
  list_geometry      visible window rects, one X,Y WxH per line

RULES
  rule [class~re] [app_id=..] [tag=..] [floating=on] [focus=on] [monitor=..] [dock=top] …
  unrule | list_rules

BAR (native status bar)
  bar create [top|bottom] [height=N] [bg=#] [fg=#] [font=N] [margin/marginx/marginy=N]
  bar add executor '<cmd>' [interval=N|continuous] [family='Font'] [size=N] [fg=][bg=][pad=]
  bar add separator [size=N] [color=#] [style=line|empty|dot]
  bar add tray [size=N] [spacing=N]      (SNI system tray; size=0 auto-fits height)
  bar add spacer | bar clear | bar destroy
    executor lclick=/rclick= run on left/right click; tray: L=activate R=menu scroll=scroll
  tray                             diagnose the SNI tray (items seen + is the module present)

WALLPAPER  (see also the sfwm-appearance GUI)
  wallpaper color <#rrggbb> [monitor=all|N]
  wallpaper <path> [mode=fill|fit|stretch|center|tile] [monitor=all|N]
  wallpaper off [monitor=all|N]

LAUNCHER
  launcher                         toggle the fullscreen fuzzy app launcher
  menu                             dmenu: fuzzy-pick a stdin line, print it to stdout
                                   e.g. printf 'a\\nb\\nc' | sc menu

SETTINGS  (sc set <name> <value>)
  window_gap  border_width  border_color_active|normal|urgent  inactive_dim (0..1)
  focus_follows_mouse  raise_on_focus  smart_frame_surroundings
  smart_window_surroundings  default_frame_layout
  notify_bg|fg|body_fg|accent|accent_critical (#)  notify_width  notify_timeout(ms)
  launcher_dim(#rrggbbaa) launcher_bg|fg|sel_bg|sel_fg (#)  launcher_width

HOOKS, ATTRIBUTES & COMBINATORS (hlwm-style)
  --idle | emit_hook <name> [args...]
  attr [path] | get_attr <path> | set_attr <path> <val> | new_attr | remove_attr
  chain / and / or / negate / try / silent / compare / substitute / sprintf / echo / true / false

SESSION
  spawn <cmd> [args...] | reload | quit | version | list_clients
";

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("help" | "-h" | "--help")) {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if args.is_empty() {
        eprint!("{HELP}");
        return ExitCode::from(2);
    }

    // `sc menu` (dmenu): the items come from stdin, appended as extra arguments.
    let menu_mode = args.first().map(String::as_str) == Some("menu");
    if menu_mode {
        let mut input = String::new();
        let _ = std::io::stdin().read_to_string(&mut input);
        for line in input.lines() {
            if !line.is_empty() {
                args.push(line.to_string());
            }
        }
    }

    let path = socket_path();
    let mut stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sc: cannot connect to sfwm at {}: {e}", path.display());
            eprintln!("    (is sfwm running, and is SOMMERFLUSSWM_SOCKET/WAYLAND_DISPLAY set?)");
            return ExitCode::from(1);
        }
    };

    // Send arguments NUL-separated, then half-close so the server sees EOF.
    let mut payload = Vec::new();
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            payload.push(0);
        }
        payload.extend_from_slice(a.as_bytes());
    }
    if let Err(e) = stream.write_all(&payload) {
        eprintln!("sc: write failed: {e}");
        return ExitCode::from(1);
    }
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Write);

    // `sc --idle` keeps the connection open and streams hook lines as they
    // arrive (one per line), until sfwm exits or the pipe is closed.
    if matches!(args.first().map(String::as_str), Some("--idle") | Some("-i")) {
        let mut buf = [0u8; 4096];
        let mut out = std::io::stdout();
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break, // sfwm closed the connection
                Ok(n) => {
                    if out.write_all(&buf[..n]).and_then(|_| out.flush()).is_err() {
                        break; // our stdout went away (downstream pipe closed)
                    }
                }
                Err(e) => {
                    eprintln!("sc: idle read failed: {e}");
                    return ExitCode::from(1);
                }
            }
        }
        return ExitCode::SUCCESS;
    }

    let mut reply = String::new();
    if let Err(e) = stream.read_to_string(&mut reply) {
        eprintln!("sc: read failed: {e}");
        return ExitCode::from(1);
    }

    // dmenu: print the chosen line; empty (Esc / no match) → exit 1, like dmenu.
    if menu_mode {
        if reply.trim_end_matches('\n').is_empty() {
            return ExitCode::from(1);
        }
        print!("{reply}");
        return ExitCode::SUCCESS;
    }

    // Errors come back prefixed with "error:"; route them to stderr / nonzero.
    if let Some(rest) = reply.strip_prefix("error:") {
        eprintln!("sc: error:{}", rest.trim_end());
        return ExitCode::from(1);
    }
    print!("{reply}");
    ExitCode::SUCCESS
}
