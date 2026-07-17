//! Touchpad gestures — native 3/4-finger swipe support.
//!
//! river's window-management protocol does not forward touchpad gestures to
//! the WM (and as a Wayland client we'd only see gestures over our own
//! surfaces), so we read them straight from libinput via its udev backend.
//! This requires read access to /dev/input/event* — in practice: the user
//! must be in the `input` group. If that fails, gestures are simply disabled
//! (logged, non-fatal).
//!
//! Recognition is native; the *action* is configured like a keybind, which is
//! what makes the swipe-up/-down cases scriptable:
//!
//!   sc gesturebind swipe3-left  use_index +1 --skip-visible
//!   sc gesturebind swipe3-up    spawn ~/scripts/sfwm/float-toggle.sh
//!
//! Specs are `swipe<fingers>-<left|right|up|down>`.

use std::fs::OpenOptions;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use input::event::gesture::{
    GestureEndEvent, GestureEventCoordinates, GestureEventTrait, GestureSwipeEvent,
};
use input::event::{Event, GestureEvent};
use input::{Libinput, LibinputInterface};

/// Minimum accumulated travel (libinput logical units, ~mm) before a swipe
/// counts. Filters out accidental brushes without making real swipes sluggish.
const THRESHOLD: f64 = 40.0;

struct Interface;

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        OpenOptions::new()
            .custom_flags(flags)
            .read(true)
            .write((flags & libc::O_RDWR != 0) || (flags & libc::O_WRONLY != 0))
            .open(path)
            .map(Into::into)
            .map_err(|e| e.raw_os_error().unwrap_or(-1))
    }
    fn close_restricted(&mut self, fd: OwnedFd) {
        drop(fd);
    }
}

/// An in-progress swipe: finger count at begin + accumulated deltas.
#[derive(Default)]
struct Swipe {
    fingers: i32,
    dx: f64,
    dy: f64,
}

pub struct Gestures {
    li: Libinput,
    swipe: Swipe,
}

impl Gestures {
    pub fn new() -> Result<Self, String> {
        let mut li = Libinput::new_with_udev(Interface);
        li.udev_assign_seat("seat0").map_err(|_| {
            "libinput: failed to assign seat0 (is the user in the `input` group?)".to_string()
        })?;
        Ok(Self { li, swipe: Swipe::default() })
    }

    /// The libinput epoll fd, for calloop registration.
    pub fn raw_fd(&self) -> RawFd {
        self.li.as_raw_fd()
    }

    /// Pump libinput and return the specs of any completed swipes
    /// ("swipe3-left", "swipe4-down", ...), in order.
    pub fn poll(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if self.li.dispatch().is_err() {
            return out;
        }
        while let Some(ev) = self.li.next() {
            let Event::Gesture(g) = ev else { continue };
            match g {
                GestureEvent::Swipe(GestureSwipeEvent::Begin(b)) => {
                    self.swipe = Swipe { fingers: b.finger_count(), dx: 0.0, dy: 0.0 };
                }
                GestureEvent::Swipe(GestureSwipeEvent::Update(u)) => {
                    self.swipe.dx += u.dx();
                    self.swipe.dy += u.dy();
                }
                GestureEvent::Swipe(GestureSwipeEvent::End(e)) => {
                    if !e.cancelled() {
                        if let Some(dir) = classify(self.swipe.dx, self.swipe.dy) {
                            out.push(format!("swipe{}-{dir}", self.swipe.fingers));
                        }
                    }
                    self.swipe = Swipe::default();
                }
                _ => {}
            }
        }
        out
    }
}

/// Dominant-axis direction of a finished swipe, or None below the threshold.
/// libinput y grows downward, so dy < 0 is "up".
fn classify(dx: f64, dy: f64) -> Option<&'static str> {
    if dx.abs() >= dy.abs() {
        (dx.abs() >= THRESHOLD).then(|| if dx < 0.0 { "left" } else { "right" })
    } else {
        (dy.abs() >= THRESHOLD).then(|| if dy < 0.0 { "up" } else { "down" })
    }
}

/// Validate a gesture spec (`swipe<fingers>-<dir>`); returns a normalized copy.
pub fn parse_spec(spec: &str) -> Result<String, String> {
    let rest = spec
        .strip_prefix("swipe")
        .ok_or_else(|| format!("bad gesture '{spec}' (want swipe<fingers>-<left|right|up|down>)"))?;
    let (n, dir) = rest
        .split_once('-')
        .ok_or_else(|| format!("bad gesture '{spec}' (want swipe<fingers>-<left|right|up|down>)"))?;
    let fingers: u8 = n.parse().map_err(|_| format!("bad finger count in '{spec}'"))?;
    if !(2..=5).contains(&fingers) {
        return Err(format!("bad finger count in '{spec}' (want 2-5)"));
    }
    if !matches!(dir, "left" | "right" | "up" | "down") {
        return Err(format!("bad direction in '{spec}' (want left|right|up|down)"));
    }
    Ok(format!("swipe{fingers}-{dir}"))
}
