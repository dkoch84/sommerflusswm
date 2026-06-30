//! The frame tree — herbstluftwm's core layout model, per tag.
//!
//! Each tag owns a binary tree. Internal nodes are **splits** (horizontal =
//! side-by-side, or vertical = stacked) with a `ratio` and a `selection` marking
//! which child holds focus. Leaves are **frames** holding a stack of windows with
//! one `selected`, arranged by the leaf's `layout` (max / vertical / horizontal /
//! grid). Focus is the path from the root following each split's `selection` down
//! to a leaf, then that leaf's `selected` window.
//!
//! This module is pure: it knows nothing about Wayland. Windows are opaque
//! [`WinId`]s, and geometry is computed against a plain [`Rect`]. That keeps the
//! tree logic — the actual hlwm intellectual content — fully unit-testable
//! without a running compositor.

use crate::monitor::Rect;

/// Opaque window identifier used inside the tree. `State` maps these to the
/// Wayland `river_window_v1` objects.
pub type WinId = u64;

/// Split orientation. `Horizontal` lays the two children out left/right (the
/// divider is vertical); `Vertical` lays them out top/bottom.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Orientation {
    Horizontal,
    Vertical,
}

/// How a leaf arranges the multiple windows it holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LayoutMode {
    /// All windows overlap filling the frame; only the selected one is shown.
    Max,
    /// Stacked top-to-bottom.
    Vertical,
    /// Side-by-side left-to-right.
    Horizontal,
    /// Roughly-square grid.
    Grid,
}

impl LayoutMode {
    pub fn parse(s: &str) -> Option<LayoutMode> {
        Some(match s {
            "max" => LayoutMode::Max,
            "vertical" => LayoutMode::Vertical,
            "horizontal" => LayoutMode::Horizontal,
            "grid" => LayoutMode::Grid,
            _ => return None,
        })
    }
    /// Cycle order for `cycle_layout`.
    pub fn next(self) -> LayoutMode {
        match self {
            LayoutMode::Vertical => LayoutMode::Horizontal,
            LayoutMode::Horizontal => LayoutMode::Max,
            LayoutMode::Max => LayoutMode::Grid,
            LayoutMode::Grid => LayoutMode::Vertical,
        }
    }
}

/// A direction for `focus`, `shift`, and `resize`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    pub fn parse(s: &str) -> Option<Dir> {
        Some(match s {
            "left" => Dir::Left,
            "right" => Dir::Right,
            "up" => Dir::Up,
            "down" => Dir::Down,
            _ => return None,
        })
    }
    fn horizontal(self) -> bool {
        matches!(self, Dir::Left | Dir::Right)
    }
}

/// How to split the focused frame (the `split` command).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SplitDir {
    Left,
    Right,
    Up,
    Down,
    Explode,
}

impl SplitDir {
    pub fn parse(s: &str) -> Option<SplitDir> {
        Some(match s {
            "left" => SplitDir::Left,
            "right" | "horizontal" => SplitDir::Right,
            "top" | "up" => SplitDir::Up,
            "bottom" | "down" | "vertical" => SplitDir::Down,
            "explode" => SplitDir::Explode,
            _ => return None,
        })
    }
}

/// A leaf frame: a stack of windows with a selection and an arrangement.
#[derive(Clone, Debug)]
pub struct Leaf {
    pub windows: Vec<WinId>,
    pub selected: usize,
    pub layout: LayoutMode,
}

impl Leaf {
    fn empty() -> Leaf {
        Leaf {
            windows: Vec::new(),
            selected: 0,
            layout: LayoutMode::Vertical,
        }
    }
    fn selected_window(&self) -> Option<WinId> {
        self.windows.get(self.selected).copied()
    }
}

/// An internal split node.
#[derive(Clone, Debug)]
pub struct Split {
    pub orient: Orientation,
    /// Fraction of the area given to the first (left/top) child, in `0.05..=0.95`.
    pub ratio: f64,
    /// Which child (0 or 1) currently holds focus.
    pub selection: usize,
    pub children: [Box<Frame>; 2],
}

/// A node in the frame tree.
#[derive(Clone, Debug)]
pub enum Frame {
    Leaf(Leaf),
    Split(Split),
}

/// The placement computed for one window during a layout pass.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Placement {
    pub win: WinId,
    pub rect: Rect,
    /// False for the obscured windows of a `Max` leaf (they get hidden).
    pub visible: bool,
}

impl Frame {
    /// A fresh tree: one empty leaf.
    pub fn new() -> Frame {
        Frame::Leaf(Leaf::empty())
    }

    /// A fresh tree whose single leaf adopts `layout` (hlwm `default_frame_layout`).
    pub fn with_layout(layout: LayoutMode) -> Frame {
        Frame::Leaf(Leaf {
            windows: Vec::new(),
            selected: 0,
            layout,
        })
    }

    #[allow(dead_code)] // used by tests; a primitive for empty-frame pruning later
    pub fn is_empty(&self) -> bool {
        match self {
            Frame::Leaf(l) => l.windows.is_empty(),
            Frame::Split(_) => false,
        }
    }

    /// True when the whole tree is a single leaf (no splits) — used by
    /// `smart_frame_surroundings` to drop the gap around a lone frame.
    pub fn is_single_leaf(&self) -> bool {
        matches!(self, Frame::Leaf(_))
    }

    // --- focus -----------------------------------------------------------------

    /// The window that currently holds focus, if any.
    pub fn focused_window(&self) -> Option<WinId> {
        match self {
            Frame::Leaf(l) => l.selected_window(),
            Frame::Split(s) => s.children[s.selection].focused_window(),
        }
    }

    /// Mutable access to the focused leaf.
    fn focused_leaf_mut(&mut self) -> &mut Leaf {
        match self {
            Frame::Leaf(l) => l,
            Frame::Split(s) => s.children[s.selection].focused_leaf_mut(),
        }
    }

    /// Path of child-indices from the root to the focused leaf.
    fn focused_path(&self) -> Vec<usize> {
        let mut p = Vec::new();
        let mut cur = self;
        while let Frame::Split(s) = cur {
            p.push(s.selection);
            cur = &s.children[s.selection];
        }
        p
    }

    /// Point every split's selection along `path` so that leaf becomes focused.
    fn set_focus_path(&mut self, path: &[usize]) {
        let mut cur = self;
        for &idx in path {
            if let Frame::Split(s) = cur {
                s.selection = idx;
                cur = &mut s.children[idx];
            } else {
                break;
            }
        }
    }

    // --- windows ---------------------------------------------------------------

    /// Insert a window into the focused leaf, just after the current selection,
    /// and focus it.
    pub fn insert_window(&mut self, w: WinId) {
        let leaf = self.focused_leaf_mut();
        let at = (leaf.selected + 1).min(leaf.windows.len());
        leaf.windows.insert(at, w);
        leaf.selected = at;
    }

    /// Remove a window from wherever it is in the tree. Returns true if found.
    /// Empty frames are left in place (as in hlwm); use `remove_frame` to prune.
    pub fn remove_window(&mut self, w: WinId) -> bool {
        match self {
            Frame::Leaf(l) => {
                if let Some(i) = l.windows.iter().position(|&x| x == w) {
                    l.windows.remove(i);
                    if l.selected >= l.windows.len() {
                        l.selected = l.windows.len().saturating_sub(1);
                    }
                    true
                } else {
                    false
                }
            }
            Frame::Split(s) => {
                s.children[0].remove_window(w) || s.children[1].remove_window(w)
            }
        }
    }

    /// Every window in the tree, focus-order irrelevant (used by `cycle_all`).
    pub fn all_windows(&self) -> Vec<WinId> {
        let mut v = Vec::new();
        self.collect_windows(&mut v);
        v
    }
    fn collect_windows(&self, out: &mut Vec<WinId>) {
        match self {
            Frame::Leaf(l) => out.extend_from_slice(&l.windows),
            Frame::Split(s) => {
                s.children[0].collect_windows(out);
                s.children[1].collect_windows(out);
            }
        }
    }

    /// Focus a specific window: point the selection path at the leaf holding it
    /// and select it within that leaf. Returns false if the window isn't present.
    pub fn focus_window(&mut self, w: WinId) -> bool {
        let Some((path, idx)) = self.find_window(w) else {
            return false;
        };
        self.set_focus_path(&path);
        if let Frame::Leaf(l) = self.node_at_mut(&path) {
            l.selected = idx;
        }
        true
    }

    /// Path to the leaf containing `w`, plus the window's index within that leaf.
    fn find_window(&self, w: WinId) -> Option<(Vec<usize>, usize)> {
        match self {
            Frame::Leaf(l) => l.windows.iter().position(|&x| x == w).map(|i| (Vec::new(), i)),
            Frame::Split(s) => {
                for (ci, child) in s.children.iter().enumerate() {
                    if let Some((mut p, i)) = child.find_window(w) {
                        p.insert(0, ci);
                        return Some((p, i));
                    }
                }
                None
            }
        }
    }

    /// Paths to every leaf in depth-first (left-child-first) order.
    fn leaf_paths(&self) -> Vec<Vec<usize>> {
        let mut out = Vec::new();
        self.collect_leaf_paths(&mut Vec::new(), &mut out);
        out
    }
    fn collect_leaf_paths(&self, path: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
        match self {
            Frame::Leaf(_) => out.push(path.clone()),
            Frame::Split(s) => {
                path.push(0);
                s.children[0].collect_leaf_paths(path, out);
                path.pop();
                path.push(1);
                s.children[1].collect_leaf_paths(path, out);
                path.pop();
            }
        }
    }

    /// Total number of leaves (frames) in the tree.
    pub fn leaves_total(&self) -> usize {
        self.leaf_paths().len()
    }

    /// Depth-first index of the currently focused leaf.
    pub fn focused_leaf_index(&self) -> usize {
        let fp = self.focused_path();
        self.leaf_paths().iter().position(|p| *p == fp).unwrap_or(0)
    }

    /// Focus the `n`-th leaf in depth-first order (hlwm-ish absolute frame focus,
    /// used by loadstate to place windows). Returns false if out of range.
    pub fn focus_nth_leaf(&mut self, n: usize) -> bool {
        match self.leaf_paths().get(n) {
            Some(p) => {
                let p = p.clone();
                self.set_focus_path(&p);
                true
            }
            None => false,
        }
    }

    /// Cycle frame focus by `delta` in depth-first order, wrapping (hlwm
    /// `cycle_frame`).
    pub fn cycle_frame(&mut self, delta: i32) {
        let total = self.leaves_total() as i32;
        if total <= 1 {
            return;
        }
        let cur = self.focused_leaf_index() as i32;
        let next = ((cur + delta) % total + total) % total;
        self.focus_nth_leaf(next as usize);
    }

    /// Cycle the selection within the focused leaf by `delta`.
    pub fn cycle(&mut self, delta: i32) {
        let leaf = self.focused_leaf_mut();
        let n = leaf.windows.len();
        if n == 0 {
            return;
        }
        let cur = leaf.selected as i32;
        leaf.selected = (((cur + delta) % n as i32 + n as i32) % n as i32) as usize;
    }

    /// Set the focused leaf's layout.
    pub fn set_layout(&mut self, layout: LayoutMode) {
        self.focused_leaf_mut().layout = layout;
    }

    /// Cycle the focused leaf's layout.
    pub fn cycle_layout(&mut self) {
        let leaf = self.focused_leaf_mut();
        leaf.layout = leaf.layout.next();
    }

    // --- structural: split / remove --------------------------------------------

    /// Split the focused frame. For directional splits a new empty leaf is
    /// created beside the focused one and focus stays on the original (hlwm
    /// behavior); `Explode` instead moves the focused leaf's non-selected
    /// windows into the new sibling.
    pub fn split(&mut self, dir: SplitDir, fraction: f64) {
        // Reach the focused frame node and transform it in place.
        let target = self.focus_node_mut();
        let frac = fraction.clamp(0.05, 0.95);

        if let SplitDir::Explode = dir {
            // Distribute the focused leaf's windows: selected stays, rest move.
            let Frame::Leaf(leaf) = target else { return };
            if leaf.windows.len() < 2 {
                return;
            }
            let layout = leaf.layout;
            let sel = leaf.selected;
            let moved: Vec<WinId> = leaf
                .windows
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != sel)
                .map(|(_, &w)| w)
                .collect();
            let kept = leaf.windows[sel];
            let original = Frame::Leaf(Leaf {
                windows: vec![kept],
                selected: 0,
                layout,
            });
            let sibling = Frame::Leaf(Leaf {
                windows: moved,
                selected: 0,
                layout,
            });
            *target = Frame::Split(Split {
                orient: Orientation::Horizontal,
                ratio: 0.5,
                selection: 0, // keep focus on the kept window
                children: [Box::new(original), Box::new(sibling)],
            });
            return;
        }

        let (orient, new_first, ratio) = match dir {
            SplitDir::Left => (Orientation::Horizontal, true, frac),
            SplitDir::Right => (Orientation::Horizontal, false, 1.0 - frac),
            SplitDir::Up => (Orientation::Vertical, true, frac),
            SplitDir::Down => (Orientation::Vertical, false, 1.0 - frac),
            SplitDir::Explode => unreachable!(),
        };
        let original = std::mem::replace(target, Frame::Leaf(Leaf::empty()));
        let empty = Frame::Leaf(Leaf::empty());
        let (children, selection): ([Box<Frame>; 2], usize) = if new_first {
            ([Box::new(empty), Box::new(original)], 1) // focus stays on original
        } else {
            ([Box::new(original), Box::new(empty)], 0)
        };
        *target = Frame::Split(Split {
            orient,
            ratio,
            selection,
            children,
        });
    }

    /// Reach the focused frame node itself (leaf or, for the freshly created
    /// split, the node being replaced).
    fn focus_node_mut(&mut self) -> &mut Frame {
        match self {
            Frame::Leaf(_) => self,
            Frame::Split(s) => {
                let sel = s.selection;
                s.children[sel].focus_node_mut()
            }
        }
    }

    /// Remove the focused leaf, replacing its parent split with the sibling and
    /// merging the focused leaf's windows into the sibling's focused leaf.
    /// No-op at the root (you cannot remove the last frame).
    pub fn remove_frame(&mut self) {
        let path = self.focused_path();
        if path.is_empty() {
            return; // focused frame is the root leaf
        }
        // Collect the windows being removed.
        let removed: Vec<WinId> = match self.node_at(&path) {
            Frame::Leaf(l) => l.windows.clone(),
            Frame::Split(_) => return,
        };

        // Replace the parent split with the sibling subtree.
        let (parent_path, child_idx) = path.split_at(path.len() - 1);
        let child_idx = child_idx[0];
        let sibling_idx = 1 - child_idx;

        let parent = self.node_at_mut(parent_path);
        let Frame::Split(s) = parent else { return };
        // Move the sibling out, drop the split, splice the sibling in.
        let sibling = std::mem::replace(&mut *s.children[sibling_idx], Frame::Leaf(Leaf::empty()));
        *parent = sibling;

        // Merge removed windows into the new focused leaf under `parent`.
        if !removed.is_empty() {
            let leaf = parent.focused_leaf_mut();
            for w in removed {
                leaf.windows.push(w);
            }
        }
    }

    fn node_at(&self, path: &[usize]) -> &Frame {
        let mut cur = self;
        for &i in path {
            if let Frame::Split(s) = cur {
                cur = &s.children[i];
            }
        }
        cur
    }
    fn node_at_mut(&mut self, path: &[usize]) -> &mut Frame {
        let mut cur = self;
        for &i in path {
            if let Frame::Split(s) = cur {
                cur = &mut s.children[i];
            } else {
                break;
            }
        }
        cur
    }

    // --- directional focus / shift / resize ------------------------------------

    /// Move focus to the nearest leaf in `dir`. Returns true if focus moved.
    pub fn focus_dir(&mut self, dir: Dir, area: Rect, gap: i32) -> bool {
        let leaves = self.leaf_rects(area, gap);
        let fpath = self.focused_path();
        let Some(from) = leaves.iter().find(|(p, _)| *p == fpath).map(|(_, r)| *r) else {
            return false;
        };
        let Some((target, _)) = pick_neighbor(from, &leaves, &fpath, dir) else {
            return false;
        };
        self.set_focus_path(&target);
        true
    }

    /// Move the focused window into the nearest leaf in `dir`. Returns true if moved.
    pub fn shift_dir(&mut self, dir: Dir, area: Rect, gap: i32) -> bool {
        let leaves = self.leaf_rects(area, gap);
        let fpath = self.focused_path();
        let Some(from) = leaves.iter().find(|(p, _)| *p == fpath).map(|(_, r)| *r) else {
            return false;
        };
        let Some((target, _)) = pick_neighbor(from, &leaves, &fpath, dir) else {
            return false;
        };
        let Some(w) = self.focused_window() else {
            return false;
        };
        self.remove_window(w);
        // Insert into the target leaf and focus there.
        self.set_focus_path(&target);
        self.insert_window(w);
        true
    }

    /// Adjust the ratio of the nearest ancestor split on `dir`'s axis.
    pub fn resize(&mut self, dir: Dir, delta: f64) -> bool {
        let path = self.focused_path();
        let want_h = dir.horizontal();
        // Walk from the focused leaf up to the root looking for a matching split.
        for cut in (0..path.len()).rev() {
            let ancestor_path = &path[..cut];
            let child_idx = path[cut];
            let node = self.node_at_mut(ancestor_path);
            if let Frame::Split(s) = node {
                let matches = matches!(s.orient, Orientation::Horizontal) == want_h;
                if matches {
                    // Growing "toward" the focused child: if focus is the first
                    // child, Right/Down grows it; if second, they shrink it.
                    let sign = match dir {
                        Dir::Right | Dir::Down => 1.0,
                        Dir::Left | Dir::Up => -1.0,
                    };
                    let adj = if child_idx == 0 { sign } else { -sign };
                    s.ratio = (s.ratio + adj * delta).clamp(0.05, 0.95);
                    return true;
                }
            }
        }
        false
    }

    // --- geometry --------------------------------------------------------------

    /// Compute the on-screen placement of every window in this tree within `area`.
    pub fn placements(&self, area: Rect, gap: i32) -> Vec<Placement> {
        let mut out = Vec::new();
        self.place(area, gap, &mut out);
        out
    }

    fn place(&self, area: Rect, gap: i32, out: &mut Vec<Placement>) {
        match self {
            Frame::Split(s) => {
                let (a, b) = split_rect(area, s.orient, s.ratio, gap);
                s.children[0].place(a, gap, out);
                s.children[1].place(b, gap, out);
            }
            Frame::Leaf(l) => layout_leaf(l, area, gap, out),
        }
    }

    /// The rect of every leaf, tagged with its path (used for focus navigation).
    fn leaf_rects(&self, area: Rect, gap: i32) -> Vec<(Vec<usize>, Rect)> {
        let mut out = Vec::new();
        self.collect_leaf_rects(area, gap, &mut Vec::new(), &mut out);
        out
    }
    fn collect_leaf_rects(
        &self,
        area: Rect,
        gap: i32,
        path: &mut Vec<usize>,
        out: &mut Vec<(Vec<usize>, Rect)>,
    ) {
        match self {
            Frame::Leaf(_) => out.push((path.clone(), area)),
            Frame::Split(s) => {
                let (a, b) = split_rect(area, s.orient, s.ratio, gap);
                path.push(0);
                s.children[0].collect_leaf_rects(a, gap, path, out);
                path.pop();
                path.push(1);
                s.children[1].collect_leaf_rects(b, gap, path, out);
                path.pop();
            }
        }
    }
}

impl Default for Frame {
    fn default() -> Self {
        Frame::new()
    }
}

impl Frame {
    /// A human-readable rendering of the tree structure (for `sc dump`).
    pub fn describe(&self) -> String {
        use std::fmt::Write;
        fn go(f: &Frame, depth: usize, out: &mut String) {
            let pad = "  ".repeat(depth);
            match f {
                Frame::Leaf(l) => {
                    let _ = writeln!(
                        out,
                        "{pad}leaf layout={:?} selected={} windows={:?}",
                        l.layout, l.selected, l.windows
                    );
                }
                Frame::Split(s) => {
                    let _ = writeln!(
                        out,
                        "{pad}split {:?} ratio={:.2} focus=child{}",
                        s.orient, s.ratio, s.selection
                    );
                    go(&s.children[0], depth + 1, out);
                    go(&s.children[1], depth + 1, out);
                }
            }
        }
        let mut out = String::new();
        go(self, 0, &mut out);
        out
    }
}

// --- serialization (hlwm `dump`/`load`) ----------------------------------------

impl Frame {
    /// Serialize the tree to an s-expression layout string (hlwm `dump`):
    ///   `(split horizontal:0.5000:0 (clients vertical:0 1 2) (clients max:0 3))`
    /// Window ids are this run's `WinId`s. Round-trips with [`Frame::deserialize`].
    pub fn serialize(&self) -> String {
        let mut s = String::new();
        self.write_sexpr(&mut s);
        s
    }

    fn write_sexpr(&self, out: &mut String) {
        use std::fmt::Write;
        match self {
            Frame::Leaf(l) => {
                let lay = match l.layout {
                    LayoutMode::Max => "max",
                    LayoutMode::Vertical => "vertical",
                    LayoutMode::Horizontal => "horizontal",
                    LayoutMode::Grid => "grid",
                };
                let _ = write!(out, "(clients {lay}:{}", l.selected);
                for w in &l.windows {
                    let _ = write!(out, " {w}");
                }
                out.push(')');
            }
            Frame::Split(s) => {
                let o = match s.orient {
                    Orientation::Horizontal => "horizontal",
                    Orientation::Vertical => "vertical",
                };
                let _ = write!(out, "(split {o}:{:.4}:{} ", s.ratio, s.selection);
                s.children[0].write_sexpr(out);
                out.push(' ');
                s.children[1].write_sexpr(out);
                out.push(')');
            }
        }
    }

    /// Parse a layout string produced by [`Frame::serialize`]. Returns `None` on
    /// malformed input.
    pub fn deserialize(s: &str) -> Option<Frame> {
        let spaced = s.replace('(', " ( ").replace(')', " ) ");
        let mut toks = spaced.split_whitespace().peekable();
        let frame = parse_node(&mut toks)?;
        // Reject trailing junk.
        if toks.next().is_some() {
            return None;
        }
        Some(frame)
    }

    /// Remove any window not in `keep` (used after loading a layout whose ids
    /// may no longer all exist).
    pub fn retain_windows(&mut self, keep: &std::collections::HashSet<WinId>) {
        match self {
            Frame::Leaf(l) => {
                l.windows.retain(|w| keep.contains(w));
                if l.selected >= l.windows.len() {
                    l.selected = l.windows.len().saturating_sub(1);
                }
            }
            Frame::Split(s) => {
                s.children[0].retain_windows(keep);
                s.children[1].retain_windows(keep);
            }
        }
    }
}

fn parse_node<'a, I: Iterator<Item = &'a str>>(
    toks: &mut std::iter::Peekable<I>,
) -> Option<Frame> {
    if toks.next()? != "(" {
        return None;
    }
    let kind = toks.next()?;
    match kind {
        "clients" => {
            // "<layout>:<selected>"
            let head = toks.next()?;
            let (lay, sel) = head.split_once(':')?;
            let layout = LayoutMode::parse(lay)?;
            let selected: usize = sel.parse().ok()?;
            let mut windows = Vec::new();
            loop {
                match toks.next()? {
                    ")" => break,
                    w => windows.push(w.parse::<WinId>().ok()?),
                }
            }
            let selected = if windows.is_empty() { 0 } else { selected.min(windows.len() - 1) };
            Some(Frame::Leaf(Leaf { windows, selected, layout }))
        }
        "split" => {
            // "<orient>:<ratio>:<selection>"
            let head = toks.next()?;
            let mut parts = head.split(':');
            let orient = match parts.next()? {
                "horizontal" => Orientation::Horizontal,
                "vertical" => Orientation::Vertical,
                _ => return None,
            };
            let ratio: f64 = parts.next()?.parse().ok()?;
            let selection: usize = parts.next()?.parse().ok()?;
            if selection > 1 {
                return None;
            }
            let c0 = parse_node(toks)?;
            let c1 = parse_node(toks)?;
            if toks.next()? != ")" {
                return None;
            }
            Some(Frame::Split(Split {
                orient,
                ratio: ratio.clamp(0.05, 0.95),
                selection,
                children: [Box::new(c0), Box::new(c1)],
            }))
        }
        _ => None,
    }
}

// --- free helpers --------------------------------------------------------------

/// Split `area` into two by `orient`/`ratio`, leaving `gap` between.
fn split_rect(area: Rect, orient: Orientation, ratio: f64, gap: i32) -> (Rect, Rect) {
    match orient {
        Orientation::Horizontal => {
            let avail = (area.w - gap).max(0);
            let w0 = ((avail as f64) * ratio).round() as i32;
            let a = Rect::new(area.x, area.y, w0, area.h);
            let b = Rect::new(area.x + w0 + gap, area.y, avail - w0, area.h);
            (a, b)
        }
        Orientation::Vertical => {
            let avail = (area.h - gap).max(0);
            let h0 = ((avail as f64) * ratio).round() as i32;
            let a = Rect::new(area.x, area.y, area.w, h0);
            let b = Rect::new(area.x, area.y + h0 + gap, area.w, avail - h0);
            (a, b)
        }
    }
}

/// Lay a leaf's windows out within `area` per its layout mode.
fn layout_leaf(l: &Leaf, area: Rect, gap: i32, out: &mut Vec<Placement>) {
    let n = l.windows.len();
    if n == 0 {
        return;
    }
    match l.layout {
        LayoutMode::Max => {
            for (i, &w) in l.windows.iter().enumerate() {
                out.push(Placement {
                    win: w,
                    rect: area,
                    visible: i == l.selected,
                });
            }
        }
        LayoutMode::Vertical => {
            for (i, &w) in l.windows.iter().enumerate() {
                out.push(Placement {
                    win: w,
                    rect: nth_strip(area, n, i, gap, true),
                    visible: true,
                });
            }
        }
        LayoutMode::Horizontal => {
            for (i, &w) in l.windows.iter().enumerate() {
                out.push(Placement {
                    win: w,
                    rect: nth_strip(area, n, i, gap, false),
                    visible: true,
                });
            }
        }
        LayoutMode::Grid => {
            let cols = (n as f64).sqrt().ceil() as i32;
            let rows = ((n as i32) + cols - 1) / cols;
            for (i, &w) in l.windows.iter().enumerate() {
                let i = i as i32;
                let (col, row) = (i % cols, i / cols);
                // Cells in this row (the last row may be short).
                let cells_in_row = if row == rows - 1 && n as i32 % cols != 0 {
                    n as i32 % cols
                } else {
                    cols
                };
                let cell_w = (area.w - (cells_in_row - 1) * gap) / cells_in_row;
                let cell_h = (area.h - (rows - 1) * gap) / rows;
                out.push(Placement {
                    win: w,
                    rect: Rect::new(
                        area.x + col * (cell_w + gap),
                        area.y + row * (cell_h + gap),
                        cell_w,
                        cell_h,
                    ),
                    visible: true,
                });
            }
        }
    }
}

/// The i-th of n equal strips of `area`, vertical (stacked) or horizontal.
fn nth_strip(area: Rect, n: usize, i: usize, gap: i32, vertical: bool) -> Rect {
    let n = n as i32;
    let i = i as i32;
    if vertical {
        let h = (area.h - (n - 1) * gap) / n;
        Rect::new(area.x, area.y + i * (h + gap), area.w, h)
    } else {
        let w = (area.w - (n - 1) * gap) / n;
        Rect::new(area.x + i * (w + gap), area.y, w, area.h)
    }
}

/// Pick the nearest leaf to `from` in direction `dir`, excluding the focused
/// leaf's own path. Returns the chosen leaf's path and rect.
fn pick_neighbor(
    from: Rect,
    leaves: &[(Vec<usize>, Rect)],
    fpath: &[usize],
    dir: Dir,
) -> Option<(Vec<usize>, Rect)> {
    let (fcx, fcy) = (from.x + from.w / 2, from.y + from.h / 2);
    let mut best: Option<(i64, Vec<usize>, Rect)> = None;
    for (path, r) in leaves {
        if path == fpath {
            continue;
        }
        let (cx, cy) = (r.x + r.w / 2, r.y + r.h / 2);
        // Must lie in the requested direction, with perpendicular overlap.
        let in_dir = match dir {
            Dir::Left => cx < fcx && overlaps(from.y, from.h, r.y, r.h),
            Dir::Right => cx > fcx && overlaps(from.y, from.h, r.y, r.h),
            Dir::Up => cy < fcy && overlaps(from.x, from.w, r.x, r.w),
            Dir::Down => cy > fcy && overlaps(from.x, from.w, r.x, r.w),
        };
        if !in_dir {
            continue;
        }
        let dist = (((cx - fcx) as i64).pow(2) + ((cy - fcy) as i64).pow(2)) as i64;
        if best.as_ref().map_or(true, |(d, _, _)| dist < *d) {
            best = Some((dist, path.clone(), *r));
        }
    }
    best.map(|(_, p, r)| (p, r))
}

/// Do 1-D ranges [a0,a0+al) and [b0,b0+bl) overlap?
fn overlaps(a0: i32, al: i32, b0: i32, bl: i32) -> bool {
    a0 < b0 + bl && b0 < a0 + al
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 1000, 800)
    }

    #[test]
    fn insert_and_focus() {
        let mut f = Frame::new();
        f.insert_window(1);
        f.insert_window(2);
        assert_eq!(f.focused_window(), Some(2));
        f.cycle(-1);
        assert_eq!(f.focused_window(), Some(1));
    }

    #[test]
    fn split_right_creates_empty_sibling_keeping_focus() {
        let mut f = Frame::new();
        f.insert_window(1);
        f.split(SplitDir::Right, 0.5);
        // Focus stays on the original (window 1); new frame is empty.
        assert_eq!(f.focused_window(), Some(1));
        let leaves = f.leaf_rects(area(), 0);
        assert_eq!(leaves.len(), 2);
        // Left half holds the window, right half is empty.
        let placements = f.placements(area(), 0);
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].win, 1);
        assert_eq!(placements[0].rect, Rect::new(0, 0, 500, 800));
    }

    #[test]
    fn focus_right_moves_into_empty_frame_then_new_window_lands_there() {
        let mut f = Frame::new();
        f.insert_window(1);
        f.split(SplitDir::Right, 0.5);
        assert!(f.focus_dir(Dir::Right, area(), 0));
        // The empty right frame is now focused; a new window goes there.
        f.insert_window(2);
        let p = f.placements(area(), 0);
        let w2 = p.iter().find(|p| p.win == 2).unwrap();
        assert_eq!(w2.rect, Rect::new(500, 0, 500, 800));
        // focus_left goes back to window 1.
        assert!(f.focus_dir(Dir::Left, area(), 0));
        assert_eq!(f.focused_window(), Some(1));
    }

    #[test]
    fn vertical_split_geometry() {
        let mut f = Frame::new();
        f.insert_window(1);
        f.split(SplitDir::Down, 0.5); // original on top, empty below
        f.focus_dir(Dir::Down, area(), 0);
        f.insert_window(2);
        let p = f.placements(area(), 0);
        let w1 = p.iter().find(|p| p.win == 1).unwrap();
        let w2 = p.iter().find(|p| p.win == 2).unwrap();
        assert_eq!(w1.rect, Rect::new(0, 0, 1000, 400));
        assert_eq!(w2.rect, Rect::new(0, 400, 1000, 400));
    }

    #[test]
    fn leaf_layouts() {
        let mut f = Frame::new();
        f.insert_window(1);
        f.insert_window(2);
        // Two windows, vertical (default): stacked halves.
        let p = f.placements(area(), 0);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].rect, Rect::new(0, 0, 1000, 400));
        assert_eq!(p[1].rect, Rect::new(0, 400, 1000, 400));
        assert!(p[0].visible && p[1].visible);

        // Max: both full-area, only selected visible.
        f.set_layout(LayoutMode::Max);
        let p = f.placements(area(), 0);
        assert!(p.iter().all(|p| p.rect == area()));
        assert_eq!(p.iter().filter(|p| p.visible).count(), 1);
    }

    #[test]
    fn shift_moves_window_between_frames() {
        let mut f = Frame::new();
        f.insert_window(1);
        f.split(SplitDir::Right, 0.5);
        // window 1 on the left, focused. Shift it right into the empty frame.
        assert!(f.shift_dir(Dir::Right, area(), 0));
        let p = f.placements(area(), 0);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].win, 1);
        assert_eq!(p[0].rect, Rect::new(500, 0, 500, 800));
        assert_eq!(f.focused_window(), Some(1));
    }

    #[test]
    fn remove_frame_merges_into_sibling() {
        let mut f = Frame::new();
        f.insert_window(1);
        f.split(SplitDir::Right, 0.5);
        f.focus_dir(Dir::Right, area(), 0);
        f.insert_window(2); // window 2 in the right frame, focused
        f.remove_frame(); // remove right frame, merge into left
        let p = f.placements(area(), 0);
        // Single frame again, full area, holding both windows (vertical layout).
        assert_eq!(p.len(), 2);
        assert!(p.iter().any(|p| p.win == 1));
        assert!(p.iter().any(|p| p.win == 2));
        // back to a single leaf spanning the whole area
        assert_eq!(f.leaf_rects(area(), 0).len(), 1);
    }

    #[test]
    fn resize_adjusts_ratio() {
        let mut f = Frame::new();
        f.insert_window(1);
        f.split(SplitDir::Right, 0.5); // focus on left child (index 0)
        f.resize(Dir::Right, 0.1); // grow focused (left) frame
        let p = f.placements(area(), 0);
        let w1 = p.iter().find(|p| p.win == 1).unwrap();
        assert_eq!(w1.rect.w, 600);
    }

    #[test]
    fn explode_distributes_windows() {
        let mut f = Frame::new();
        f.insert_window(1);
        f.insert_window(2);
        f.insert_window(3); // 3 windows in one leaf; selected = 3
        f.split(SplitDir::Explode, 0.5);
        // selected (3) kept in original; 1 and 2 moved to the sibling.
        let leaves = f.leaf_rects(area(), 0);
        assert_eq!(leaves.len(), 2);
        assert_eq!(f.focused_window(), Some(3));
        assert_eq!(f.all_windows().len(), 3);
    }

    #[test]
    fn serialize_round_trips() {
        let mut f = Frame::new();
        f.insert_window(1);
        f.insert_window(2);
        f.split(SplitDir::Right, 0.5);
        f.focus_dir(Dir::Right, area(), 0);
        f.insert_window(3);
        f.set_layout(LayoutMode::Max);

        let dumped = f.serialize();
        let back = Frame::deserialize(&dumped).expect("parse");
        // Same structure & windows survive a round trip.
        assert_eq!(back.serialize(), dumped);
        assert_eq!(back.all_windows().len(), 3);
        assert!(dumped.starts_with("(split horizontal:"));
        assert!(dumped.contains("(clients max:"));

        // Malformed input is rejected, not panicked.
        assert!(Frame::deserialize("(split horizontal:0.5:0 (clients max:0 1))").is_none());
        assert!(Frame::deserialize("garbage").is_none());
    }

    #[test]
    fn retain_drops_absent_windows() {
        let mut f = Frame::deserialize("(clients vertical:1 5 6 7)").unwrap();
        let keep: std::collections::HashSet<WinId> = [6u64].into_iter().collect();
        f.retain_windows(&keep);
        assert_eq!(f.all_windows(), vec![6]);
        assert_eq!(f.focused_window(), Some(6));
    }

    #[test]
    fn remove_window_fixes_selection() {
        let mut f = Frame::new();
        f.insert_window(1);
        f.insert_window(2);
        f.insert_window(3); // selected index 2 (window 3)
        assert!(f.remove_window(3));
        assert_eq!(f.focused_window(), Some(2));
        assert!(!f.remove_window(99));
    }
}
