//! Thin facade over the BSP tiling engine.
//!
//! `ratatui-hypertile` is at `0.4` with a single maintainer, so every call into it is
//! funnelled through this type. Swapping or forking the engine means rewriting this
//! file and nothing else.
//!
//! See `docs/SPEC.md` §4.1.

use ratatui::layout::{Direction, Rect};
use ratatui_hypertile::raw::Node;
use ratatui_hypertile::{Hypertile, PaneId, SplitPolicy, StateError};
use serde::{Deserialize, Serialize};

/// Split ratios are clamped to keep every pane usable.
const MIN_RATIO: f32 = 0.1;
const MAX_RATIO: f32 = 0.9;

/// Which way to move focus, split, or resize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    fn axis(self) -> Direction {
        match self {
            Self::Left | Self::Right => Direction::Horizontal,
            Self::Up | Self::Down => Direction::Vertical,
        }
    }
}

/// A pane's position after layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub id: PaneId,
    pub rect: Rect,
    pub is_focused: bool,
}

/// BSP tiling for one tab.
pub struct Tiling {
    inner: Hypertile,
    /// Set when a pane is zoomed to fill the tab.
    zoomed: Option<PaneId>,
    /// The last area passed to [`Tiling::layout`], needed to resolve clicks while
    /// zoomed.
    area: Rect,
}

/// Everything needed to rebuild a [`Tiling`] exactly as it was.
///
/// The tree is the engine's own `Node`, serialised by the engine, so split directions
/// and ratios persist without this crate inventing a second representation of them
/// that could drift out of step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TilingSnapshot {
    tree: Node,
    /// Pane ids as `u64`, so the file does not depend on the engine's newtype.
    focused: Option<u64>,
    zoomed: Option<u64>,
}

impl Default for Tiling {
    fn default() -> Self {
        Self::new()
    }
}

impl Tiling {
    pub fn new() -> Self {
        Self {
            inner: Hypertile::builder()
                .with_split_policy(SplitPolicy::Half)
                .with_focus_highlight(true)
                .build(),
            zoomed: None,
            area: Rect::ZERO,
        }
    }

    /// Capture the tiling for persistence.
    pub fn snapshot(&self) -> TilingSnapshot {
        TilingSnapshot {
            tree: self.inner.root().clone(),
            focused: self.focused().map(PaneId::get),
            zoomed: self.zoomed.map(PaneId::get),
        }
    }

    /// Rebuild a tiling from a snapshot.
    ///
    /// `set_root` normalises the tree, rejects duplicate pane ids and moves the id
    /// allocator past the highest one restored, so a file that has been hand-edited or
    /// truncated fails here rather than producing a tiling whose next split collides
    /// with an existing pane.
    pub fn restore(snapshot: &TilingSnapshot) -> Result<Self, StateError> {
        let mut tiling = Self::new();
        tiling.inner.set_root(snapshot.tree.clone())?;

        // Focus and zoom are advisory: a pane id that is not in the tree means the file
        // disagrees with itself, and losing the cursor position is a far better outcome
        // than refusing to restore the layout at all.
        if let Some(id) = snapshot.focused.map(PaneId::new) {
            let _ = tiling.inner.focus_pane(id);
        }
        tiling.zoomed = snapshot
            .zoomed
            .map(PaneId::new)
            .filter(|id| tiling.inner.pane_path(*id).is_some());

        Ok(tiling)
    }

    /// The pane the user is interacting with.
    pub fn focused(&self) -> Option<PaneId> {
        self.inner.focused_pane()
    }

    /// Focus a specific pane.
    pub fn focus(&mut self, id: PaneId) -> bool {
        self.inner.focus_pane(id).is_ok()
    }

    /// Split the focused pane, returning the new pane's id.
    pub fn split(&mut self, dir: Dir) -> Option<PaneId> {
        // Splitting while zoomed is confusing: the new pane would be invisible. Unzoom
        // first so the result is what the user sees.
        self.zoomed = None;
        self.inner.split_focused(dir.axis()).ok()
    }

    /// Close the focused pane, returning its id.
    pub fn close_focused(&mut self) -> Option<PaneId> {
        let closed = self.inner.close_focused().ok();
        if self.zoomed == closed {
            self.zoomed = None;
        }
        closed
    }

    /// Move focus geometrically.
    ///
    /// Uses the computed rectangles rather than the tree, so movement matches what the
    /// user sees rather than how the splits happen to nest.
    pub fn focus_dir(&mut self, dir: Dir) -> Option<PaneId> {
        let current = self.focused()?;
        let from = self.inner.pane_rect(current)?;

        let (cx, cy) = (from.x + from.width / 2, from.y + from.height / 2);

        let best = self
            .inner
            .panes_iter()
            .filter(|p| p.id != current)
            .filter(|p| {
                let r = p.rect;
                match dir {
                    Dir::Left => r.x + r.width <= from.x,
                    Dir::Right => r.x >= from.x + from.width,
                    Dir::Up => r.y + r.height <= from.y,
                    Dir::Down => r.y >= from.y + from.height,
                }
            })
            .min_by_key(|p| {
                let r = p.rect;
                let (px, py) = (r.x + r.width / 2, r.y + r.height / 2);
                // Primary: distance along the axis of travel. Secondary: perpendicular
                // offset, so moving right from a tall pane lands on the pane opposite
                // rather than a distant corner.
                let (along, across) = match dir {
                    Dir::Left | Dir::Right => (px.abs_diff(cx), py.abs_diff(cy)),
                    Dir::Up | Dir::Down => (py.abs_diff(cy), px.abs_diff(cx)),
                };
                (along as u32) * 1024 + across as u32
            })?;

        self.inner.focus_pane(best.id).ok()?;
        Some(best.id)
    }

    /// Grow or shrink the focused pane along `dir`.
    pub fn resize(&mut self, dir: Dir, amount: f32) {
        let Some(id) = self.focused() else { return };
        let Some(path) = self.inner.pane_path(id) else {
            return;
        };
        // The parent split is the pane's path minus the pane's own index within it.
        if path.is_empty() {
            return;
        }
        let (split_path, index) = path.split_at(path.len() - 1);
        let first_child = index[0] == 0;

        // Growing means giving a larger share to whichever side the focused pane is on.
        let delta = match dir {
            Dir::Right | Dir::Down if first_child => amount,
            Dir::Left | Dir::Up if !first_child => amount,
            _ => -amount,
        };

        // Read the current ratio back out of the tree rather than shadowing it in a
        // side table. The engine has no ratio getter, but the tree it hands back has
        // the number in it, and a second copy could only ever drift -- most obviously
        // after a restore, where the side table would start empty and the first resize
        // would snap a carefully placed border back to the middle.
        let current = ratio_at(self.inner.root(), split_path).unwrap_or(0.5);
        let updated = (current + delta).clamp(MIN_RATIO, MAX_RATIO);
        let _ = self.inner.set_split_ratio(split_path, updated);
    }

    /// Toggle zoom on the focused pane.
    pub fn toggle_zoom(&mut self) {
        let focused = self.focused();
        self.zoomed = match self.zoomed {
            Some(z) if Some(z) == focused => None,
            _ => focused,
        };
    }

    pub fn is_zoomed(&self) -> bool {
        self.zoomed.is_some()
    }

    /// Compute rectangles for `area` and return them.
    pub fn layout(&mut self, area: Rect) -> Vec<Placement> {
        self.area = area;
        self.inner.compute_layout(area);

        if let Some(zoomed) = self.zoomed {
            // A zoomed pane owns the whole area; the rest are simply not drawn.
            if self.inner.pane_rect(zoomed).is_some() {
                return vec![Placement {
                    id: zoomed,
                    rect: area,
                    is_focused: true,
                }];
            }
            // The zoomed pane vanished (closed elsewhere); fall through to normal.
        }

        self.inner
            .panes_iter()
            .map(|p| Placement {
                id: p.id,
                rect: p.rect,
                is_focused: p.is_focused,
            })
            .collect()
    }

    /// Which pane is under a terminal cell, for click-to-focus.
    pub fn pane_at(&self, column: u16, row: u16) -> Option<PaneId> {
        if let Some(zoomed) = self.zoomed {
            return self.area.contains((column, row).into()).then_some(zoomed);
        }
        self.inner.pane_at(column, row)
    }

    /// Every pane in the tree, whether or not a layout has been computed.
    ///
    /// Distinct from the ids [`Tiling::layout`] returns: those come from the layout
    /// cache, which is empty until the first frame, and a freshly restored tiling has
    /// panes long before it has geometry.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        ratatui_hypertile::raw::collect_pane_ids(self.inner.root())
    }

    /// Number of panes.
    pub fn len(&self) -> usize {
        self.inner.panes_iter().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The ratio of the split at `path`, or `None` if the path does not name a split.
fn ratio_at(root: &Node, path: &[usize]) -> Option<f32> {
    let mut node = root;
    for step in path {
        let Node::Split { first, second, .. } = node else {
            return None;
        };
        node = match step {
            0 => first,
            1 => second,
            _ => return None,
        };
    }
    match node {
        Node::Split { ratio, .. } => Some(*ratio),
        Node::Pane(_) => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 24,
    };

    #[test]
    fn starts_with_a_single_pane() {
        let mut t = Tiling::new();
        let placements = t.layout(AREA);
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].rect, AREA);
    }

    #[test]
    fn splitting_halves_the_area() {
        let mut t = Tiling::new();
        t.split(Dir::Right).expect("splits");
        let placements = t.layout(AREA);
        assert_eq!(placements.len(), 2);
        let total: u32 = placements.iter().map(|p| p.rect.area()).sum();
        assert!(total <= AREA.area(), "panes must not overlap the area");
    }

    #[test]
    fn focus_moves_geometrically() {
        let mut t = Tiling::new();
        let right = t.split(Dir::Right).expect("splits");
        t.layout(AREA);

        // After splitting, focus is on the new pane. Moving left must reach the old one.
        assert_eq!(t.focused(), Some(right));
        let left = t.focus_dir(Dir::Left).expect("moves left");
        assert_ne!(left, right);
        assert_eq!(t.focus_dir(Dir::Right), Some(right));
    }

    #[test]
    fn focus_does_not_move_past_an_edge() {
        let mut t = Tiling::new();
        t.split(Dir::Right);
        t.layout(AREA);
        t.focus_dir(Dir::Left);
        assert_eq!(t.focus_dir(Dir::Left), None, "no pane further left");
    }

    #[test]
    fn zoom_gives_one_pane_the_whole_area() {
        let mut t = Tiling::new();
        t.split(Dir::Right);
        t.layout(AREA);

        t.toggle_zoom();
        assert!(t.is_zoomed());
        let placements = t.layout(AREA);
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].rect, AREA);

        t.toggle_zoom();
        assert!(!t.is_zoomed());
        assert_eq!(t.layout(AREA).len(), 2);
    }

    #[test]
    fn splitting_while_zoomed_unzooms() {
        // Otherwise the new pane would be created invisible.
        let mut t = Tiling::new();
        t.split(Dir::Right);
        t.layout(AREA);
        t.toggle_zoom();
        t.split(Dir::Down);
        assert!(!t.is_zoomed());
        assert_eq!(t.layout(AREA).len(), 3);
    }

    #[test]
    fn closing_the_zoomed_pane_clears_zoom() {
        let mut t = Tiling::new();
        t.split(Dir::Right);
        t.layout(AREA);
        t.toggle_zoom();
        t.close_focused();
        assert!(!t.is_zoomed());
    }

    #[test]
    fn click_resolves_to_a_pane() {
        let mut t = Tiling::new();
        t.split(Dir::Right);
        let placements = t.layout(AREA);
        let target = placements[0];
        let hit = t.pane_at(target.rect.x + 1, target.rect.y + 1);
        assert_eq!(hit, Some(target.id));
    }
}
