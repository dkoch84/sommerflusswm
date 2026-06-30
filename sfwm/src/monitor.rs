//! The monitor model — sommerflusswm's port of herbstluftwm's virtual monitors.
//!
//! A `Monitor` is a WM-side abstraction, fully decoupled from `river_output_v1`:
//! an arbitrary rectangle in the global logical coordinate space, with a stacking
//! `z`, reserved `pad`, a currently-displayed `tag`, and an optional `locked_tag`.
//! This is the hlwm `set_monitors` / `add_monitor` / `raise_monitor` / `lock_tag`
//! model — including the defining requirement: **overlapping monitors** (the
//! `float1`/`float2` overlays that sit on the same rect as a base monitor but with
//! a higher `z`, so a tag toggled onto them pops a full-screen scratchpad on top).
//!
//! This module is deliberately pure: it knows nothing about Wayland. It owns the
//! topology and answers the geometric/ordering questions the render pass needs.
//! That keeps the defining-risk logic unit-testable without a running river.

/// A tag (workspace) identifier. hlwm uses string tag names ("1".."9"); we keep
/// them as small integers for the monitor layer. The richer tag model (names,
/// `my_monitor` affinity, the frame tree) lands in milestone 3.
pub type TagId = u32;

/// A monitor identifier, stable across the monitor's lifetime. Distinct from the
/// monitor's *index* in the list, which shifts as monitors are added/removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MonitorId(pub u32);

/// A rectangle in absolute logical coordinates. `x`/`y` may be negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Rect { x, y, w, h }
    }

    /// Parse an hlwm-style geometry: `WxH+X+Y` (also accepts negative offsets,
    /// e.g. `WxH-X-Y`). Returns `None` on any malformed input.
    ///
    /// Mirrors the rects in the autostart, e.g. `4096x2160+1440+2160`.
    pub fn parse(s: &str) -> Option<Rect> {
        // Split "WxH" from the trailing "+X+Y" / "-X-Y" by finding the first
        // sign character after the 'x'.
        let x_pos = s.find('x')?;
        let (size, offset) = {
            // The offset starts at the first '+' or '-' after the size.
            let after_size = &s[x_pos + 1..];
            let rel = after_size.find(['+', '-'])?;
            (&s[..x_pos + 1 + rel], &s[x_pos + 1 + rel..])
        };
        let (w_str, h_str) = size.split_once('x')?;
        let w: i32 = w_str.parse().ok()?;
        let h: i32 = h_str.parse().ok()?;

        // offset is like "+1440+2160" or "-10+20"; re-split on sign boundaries.
        let signs: Vec<usize> = offset
            .char_indices()
            .filter(|(_, c)| *c == '+' || *c == '-')
            .map(|(i, _)| i)
            .collect();
        if signs.len() != 2 {
            return None;
        }
        let x: i32 = offset[signs[0]..signs[1]].parse().ok()?;
        let y: i32 = offset[signs[1]..].parse().ok()?;
        Some(Rect { x, y, w, h })
    }
}

/// Reserved space on each edge of a monitor (hlwm `pad`). Order matches hlwm's
/// `pad <mon> <top> <right> <bottom> <left>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Insets {
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

/// A virtual monitor: an arbitrary rect with stacking, padding, and a tag.
#[derive(Debug, Clone)]
pub struct Monitor {
    pub id: MonitorId,
    /// Optional name ("float1", "float2"). Selectable by name in IPC commands.
    pub name: Option<String>,
    /// Absolute logical rect.
    pub rect: Rect,
    /// Stacking order among monitors; higher renders on top. Overlapping overlays
    /// get a higher `z` than the base monitor they sit over.
    pub z: i32,
    /// Reserved edges (hlwm `pad`) — a bar lives here. Reconciled with
    /// layer-shell exclusive zones in a later milestone.
    pub pad: Insets,
    /// The tag currently displayed on this monitor.
    pub tag: TagId,
    /// If set, this monitor only ever displays this tag (hlwm `lock_tag`).
    pub locked_tag: Option<TagId>,
}

impl Monitor {
    /// The usable tiling area: `rect` shrunk by `pad`. Never negative in w/h.
    pub fn usable(&self) -> Rect {
        let r = self.rect;
        let p = self.pad;
        Rect {
            x: r.x + p.left,
            y: r.y + p.top,
            w: (r.w - p.left - p.right).max(0),
            h: (r.h - p.top - p.bottom).max(0),
        }
    }
}

/// How to select a monitor in an IPC command: by list index or by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorSel {
    Index(usize),
    Name(String),
}

impl MonitorSel {
    /// Parse a selector token: all-digits → index, otherwise a name.
    pub fn parse(s: &str) -> MonitorSel {
        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
            MonitorSel::Index(s.parse().unwrap())
        } else {
            MonitorSel::Name(s.to_string())
        }
    }
}

/// The full monitor topology plus which monitor currently has focus.
#[derive(Debug, Default)]
pub struct Monitors {
    pub list: Vec<Monitor>,
    /// Index into `list` of the focused monitor.
    pub focus: usize,
    next_id: u32,
}

impl Monitors {
    pub fn new() -> Self {
        Monitors::default()
    }

    fn alloc_id(&mut self) -> MonitorId {
        let id = MonitorId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Replace all monitors with a fresh set of base monitors from rects (hlwm
    /// `set_monitors`). Base monitor `i` displays tag `i + 1`, matching hlwm's
    /// default of putting the first tags on the first monitors. Named overlays
    /// are expected to be re-added with `add_monitor` afterwards, exactly as the
    /// autostart does.
    pub fn set_monitors(&mut self, rects: &[Rect]) {
        self.list.clear();
        for (i, r) in rects.iter().enumerate() {
            let id = self.alloc_id();
            self.list.push(Monitor {
                id,
                name: None,
                rect: *r,
                z: 0,
                pad: Insets::default(),
                tag: (i as TagId) + 1,
                locked_tag: None,
            });
        }
        self.focus = self.focus.min(self.list.len().saturating_sub(1));
    }

    /// Add a monitor (hlwm `add_monitor <rect> <tag> [name]`). Returns its id.
    /// New monitors start at the current max `z` so a subsequent `raise_monitor`
    /// is what actually lifts an overlay above the base monitors.
    pub fn add_monitor(&mut self, rect: Rect, tag: TagId, name: Option<String>) -> MonitorId {
        let z = self.max_z();
        let id = self.alloc_id();
        self.list.push(Monitor {
            id,
            name,
            rect,
            z,
            pad: Insets::default(),
            tag,
            locked_tag: None,
        });
        id
    }

    fn max_z(&self) -> i32 {
        self.list.iter().map(|m| m.z).max().unwrap_or(0)
    }

    /// Resolve a selector to an index into `list`.
    pub fn resolve(&self, sel: &MonitorSel) -> Option<usize> {
        match sel {
            MonitorSel::Index(i) => {
                if *i < self.list.len() {
                    Some(*i)
                } else {
                    None
                }
            }
            MonitorSel::Name(n) => self
                .list
                .iter()
                .position(|m| m.name.as_deref() == Some(n.as_str())),
        }
    }

    /// Raise a monitor above all others (hlwm `raise_monitor`).
    pub fn raise_monitor(&mut self, sel: &MonitorSel) -> bool {
        let max = self.max_z();
        match self.resolve(sel) {
            Some(i) => {
                self.list[i].z = max + 1;
                true
            }
            None => false,
        }
    }

    /// Lock a tag to a monitor (hlwm `lock_tag`). Also displays the tag there.
    pub fn lock_tag(&mut self, tag: TagId, sel: &MonitorSel) -> bool {
        match self.resolve(sel) {
            Some(i) => {
                self.list[i].locked_tag = Some(tag);
                self.list[i].tag = tag;
                true
            }
            None => false,
        }
    }

    /// Set a monitor's padding (hlwm `pad`).
    pub fn set_pad(&mut self, sel: &MonitorSel, pad: Insets) -> bool {
        match self.resolve(sel) {
            Some(i) => {
                self.list[i].pad = pad;
                true
            }
            None => false,
        }
    }

    /// Focus a monitor (hlwm `focus_monitor`).
    pub fn focus_monitor(&mut self, sel: &MonitorSel) -> bool {
        match self.resolve(sel) {
            Some(i) => {
                self.focus = i;
                true
            }
            None => false,
        }
    }

    /// Cycle focus among monitors (hlwm `cycle_monitor`). `delta` wraps.
    pub fn cycle_monitor(&mut self, delta: i32) {
        if self.list.is_empty() {
            return;
        }
        let n = self.list.len() as i32;
        let cur = self.focus as i32;
        self.focus = (((cur + delta) % n + n) % n) as usize;
    }

    pub fn focused(&self) -> Option<&Monitor> {
        self.list.get(self.focus)
    }

    pub fn focused_mut(&mut self) -> Option<&mut Monitor> {
        self.list.get_mut(self.focus)
    }

    /// The set of tags currently displayed on some monitor. A primitive for the
    /// bar IPC and arbitrary-tag-set `--skip-visible` in later milestones.
    #[allow(dead_code)]
    pub fn visible_tags(&self) -> Vec<TagId> {
        let mut v: Vec<TagId> = self.list.iter().map(|m| m.tag).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    pub fn tag_visible(&self, tag: TagId) -> bool {
        self.list.iter().any(|m| m.tag == tag)
    }

    /// Display `tag` on the focused monitor, unless that monitor is locked to a
    /// different tag. Returns the index of the monitor the tag should be shown
    /// on (honoring a lock elsewhere is the caller's concern — see `use_tag`).
    pub fn show_on_focused(&mut self, tag: TagId) {
        if let Some(m) = self.focused_mut() {
            if m.locked_tag.map_or(true, |lt| lt == tag) {
                m.tag = tag;
            }
        }
    }

    /// Remove a monitor (hlwm `remove_monitor`). Keeps `focus` in range.
    pub fn remove_monitor(&mut self, sel: &MonitorSel) -> bool {
        match self.resolve(sel) {
            Some(i) => {
                self.list.remove(i);
                if self.focus >= self.list.len() {
                    self.focus = self.list.len().saturating_sub(1);
                }
                true
            }
            None => false,
        }
    }

    /// Clear a monitor's tag lock (hlwm `unlock_tag`).
    pub fn unlock_tag(&mut self, sel: &MonitorSel) -> bool {
        match self.resolve(sel) {
            Some(i) => {
                self.list[i].locked_tag = None;
                true
            }
            None => false,
        }
    }

    /// Reposition/resize a monitor's rect (hlwm `move_monitor`).
    pub fn move_monitor(&mut self, sel: &MonitorSel, rect: Rect) -> bool {
        match self.resolve(sel) {
            Some(i) => {
                self.list[i].rect = rect;
                true
            }
            None => false,
        }
    }

    /// Rename a monitor (hlwm `rename_monitor`). `None` clears the name.
    pub fn rename_monitor(&mut self, sel: &MonitorSel, name: Option<String>) -> bool {
        match self.resolve(sel) {
            Some(i) => {
                self.list[i].name = name;
                true
            }
            None => false,
        }
    }

    /// The tag currently displayed on the selected monitor.
    pub fn tag_of(&self, sel: &MonitorSel) -> Option<TagId> {
        self.resolve(sel).map(|i| self.list[i].tag)
    }

    /// Indices of monitors in render order: ascending `z`, ties broken by list
    /// index (stable). The first entry renders at the bottom, the last on top —
    /// so overlays (high `z`) end up above the base monitors they overlap.
    pub fn render_order(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.list.len()).collect();
        idx.sort_by(|&a, &b| {
            self.list[a]
                .z
                .cmp(&self.list[b].z)
                .then(a.cmp(&b))
        });
        idx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_geometry() {
        assert_eq!(Rect::parse("4096x2160+1440+2160"), Some(Rect::new(1440, 2160, 4096, 2160)));
        assert_eq!(Rect::parse("1440x2560+0+1876"), Some(Rect::new(0, 1876, 1440, 2560)));
        assert_eq!(Rect::parse("100x200-10+20"), Some(Rect::new(-10, 20, 100, 200)));
        assert_eq!(Rect::parse("garbage"), None);
        assert_eq!(Rect::parse("100x200"), None); // missing offset
    }

    #[test]
    fn usable_subtracts_pad() {
        let m = Monitor {
            id: MonitorId(0),
            name: None,
            rect: Rect::new(0, 0, 1000, 1000),
            z: 0,
            // hlwm pad order: top right bottom left
            pad: Insets { top: 36, right: 4, bottom: 4, left: 4 },
            tag: 1,
            locked_tag: None,
        };
        assert_eq!(m.usable(), Rect::new(4, 36, 992, 960));
    }

    #[test]
    fn set_monitors_assigns_sequential_tags() {
        let mut m = Monitors::new();
        m.set_monitors(&[
            Rect::new(0, 0, 100, 100),
            Rect::new(100, 0, 100, 100),
        ]);
        assert_eq!(m.list.len(), 2);
        assert_eq!(m.list[0].tag, 1);
        assert_eq!(m.list[1].tag, 2);
    }

    /// The defining requirement: an overlay added over a base monitor and raised
    /// must render *above* it, and a tag locked onto the overlay must display
    /// there. This mirrors the autostart's float1/float2 setup.
    #[test]
    fn overlapping_overlay_renders_on_top() {
        let mut m = Monitors::new();
        // base monitor over a 4K panel
        m.set_monitors(&[Rect::new(1440, 0, 3840, 2160)]);
        // float1 overlay on the SAME rect
        let float1 = m.add_monitor(Rect::new(1440, 0, 3840, 2160), 8, Some("float1".into()));
        m.raise_monitor(&MonitorSel::Name("float1".into()));
        m.lock_tag(8, &MonitorSel::Name("float1".into()));

        // render order: base (index 0, z=0) first, overlay last (on top)
        let order = m.render_order();
        assert_eq!(order, vec![0, 1]);
        let top_idx = *order.last().unwrap();
        assert_eq!(m.list[top_idx].id, float1);
        assert_eq!(m.list[top_idx].tag, 8); // lock displayed tag 8 there
        assert!(m.list[top_idx].z > m.list[0].z);
    }

    #[test]
    fn lock_prevents_showing_other_tag_on_focused() {
        let mut m = Monitors::new();
        m.set_monitors(&[Rect::new(0, 0, 100, 100)]);
        m.add_monitor(Rect::new(0, 0, 100, 100), 9, Some("float2".into()));
        m.focus_monitor(&MonitorSel::Name("float2".into()));
        m.lock_tag(9, &MonitorSel::Name("float2".into()));
        // Attempting to show a different tag on the locked monitor is a no-op.
        m.show_on_focused(3);
        assert_eq!(m.focused().unwrap().tag, 9);
        // Showing the locked tag itself is fine.
        m.show_on_focused(9);
        assert_eq!(m.focused().unwrap().tag, 9);
    }

    #[test]
    fn cycle_monitor_wraps() {
        let mut m = Monitors::new();
        m.set_monitors(&[
            Rect::new(0, 0, 1, 1),
            Rect::new(1, 0, 1, 1),
            Rect::new(2, 0, 1, 1),
        ]);
        assert_eq!(m.focus, 0);
        m.cycle_monitor(1);
        assert_eq!(m.focus, 1);
        m.cycle_monitor(-2);
        assert_eq!(m.focus, 2); // (1 - 2) mod 3 = 2
    }

    #[test]
    fn visible_tags_dedup() {
        let mut m = Monitors::new();
        m.set_monitors(&[Rect::new(0, 0, 1, 1), Rect::new(1, 0, 1, 1)]);
        m.list[1].tag = 1; // both show tag 1
        assert_eq!(m.visible_tags(), vec![1]);
        assert!(m.tag_visible(1));
        assert!(!m.tag_visible(5));
    }

    #[test]
    fn remove_monitor_keeps_focus_in_range() {
        let mut m = Monitors::new();
        m.set_monitors(&[Rect::new(0, 0, 1, 1), Rect::new(1, 0, 1, 1), Rect::new(2, 0, 1, 1)]);
        m.focus = 2;
        assert!(m.remove_monitor(&MonitorSel::Index(2)));
        assert_eq!(m.list.len(), 2);
        assert_eq!(m.focus, 1); // clamped back into range
        assert!(m.rename_monitor(&MonitorSel::Index(0), Some("primary".into())));
        assert_eq!(m.tag_of(&MonitorSel::Name("primary".into())), Some(1));
        assert!(!m.remove_monitor(&MonitorSel::Index(9)));
    }
}
