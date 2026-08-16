//! Workspace, tab and pane model.
//!
//! heddle maps Matrix's existing hierarchy onto a terminal workspace manager:
//!
//! | heddle    | Matrix | Hermes                    |
//! |-----------|--------|---------------------------|
//! | Workspace | Space  | project                   |
//! | Tab       | Room   | room-scoped session lane  |
//! | Pane      | Thread | agent session             |
//!
//! Rooms belonging to no Space collect in an implicit workspace. Threadless room
//! timelines render as the tab's root pane.
//!
//! See `docs/SPEC.md` §2.

use heddle_agent::AgentState;
use ratatui_hypertile::PaneId;
use serde::{Deserialize, Serialize};

/// Identifier for the implicit workspace holding rooms with no parent Space.
pub const ORPHAN_WORKSPACE: &str = "~";

/// What a pane is showing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneKind {
    /// The room's main timeline, outside any thread.
    Room { room_id: String },
    /// One thread: a single agent session.
    Thread {
        room_id: String,
        /// Event ID of the thread root. This is the agent session key.
        root: String,
    },
}

impl PaneKind {
    pub fn room_id(&self) -> &str {
        match self {
            Self::Room { room_id } | Self::Thread { room_id, .. } => room_id,
        }
    }

    /// The thread root, when this pane is a thread.
    pub fn thread_root(&self) -> Option<&str> {
        match self {
            Self::Thread { root, .. } => Some(root),
            Self::Room { .. } => None,
        }
    }
}

/// Unread counts, as the homeserver reports them.
///
/// `highlights` are messages that named you; `notifications` is everything the push
/// rules think is worth a badge. Rolled up separately: only the first justifies
/// interrupting a focused pane.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unread {
    pub notifications: u64,
    pub highlights: u64,
}

impl Unread {
    pub const fn new(notifications: u64, highlights: u64) -> Self {
        Self {
            notifications,
            highlights,
        }
    }

    pub const fn any(self) -> bool {
        self.notifications > 0 || self.highlights > 0
    }

    pub const fn is_highlight(self) -> bool {
        self.highlights > 0
    }

    /// The number worth showing.
    ///
    /// The larger counter: `notifications` is usually the superset, but a push rule can
    /// raise a highlight without counting a notification, giving `(@0)`.
    pub const fn count(self) -> u64 {
        if self.highlights > self.notifications {
            self.highlights
        } else {
            self.notifications
        }
    }

    /// Badge text, or `None` when there is nothing unread.
    ///
    /// `(3)` is three unread; `(@3)` is three of which at least one names you. ASCII:
    /// this sits in a tab cell measured with `unicode-width`, and an emoji that measures
    /// narrow but paints wide drifts the whole strip.
    pub fn label(self) -> Option<String> {
        if !self.any() {
            return None;
        }
        Some(if self.is_highlight() {
            format!("(@{})", self.count())
        } else {
            format!("({})", self.count())
        })
    }
}

impl std::ops::Add for Unread {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            notifications: self.notifications.saturating_add(other.notifications),
            highlights: self.highlights.saturating_add(other.highlights),
        }
    }
}

impl std::iter::Sum for Unread {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::default(), |total, next| total + next)
    }
}

/// A single tiled view.
#[derive(Debug, Clone)]
pub struct Pane {
    /// Assigned by the tiling engine.
    pub id: PaneId,
    pub kind: PaneKind,
    /// Shown in the pane header. Falls back to the thread's first line.
    pub title: String,
    /// Derived agent state; drives the pane badge and rolls up to the tab.
    pub state: AgentState,
    /// Set when this pane's agent events came from the fallback parser.
    pub degraded: bool,
    /// Set when this pane's session is missing events outright.
    ///
    /// Distinct from `degraded`, and worse: degraded means the structure was recovered
    /// lossily from what did arrive, a gap means something never arrived at all.
    pub gaps: bool,
}

impl Pane {
    pub fn new(id: PaneId, kind: PaneKind, title: impl Into<String>) -> Self {
        Self {
            id,
            kind,
            title: title.into(),
            state: AgentState::Idle,
            degraded: false,
            gaps: false,
        }
    }

    /// Header text including the badge and any lossiness markers.
    ///
    /// Markers run worst-first, so the more serious one is the one nearest the badge and
    /// a pane that is both does not read as either alone.
    pub fn header(&self) -> String {
        let gaps = if self.gaps { "!" } else { "" };
        let degraded = if self.degraded { "~" } else { "" };
        format!("{} {}{}{}", self.state.glyph(), gaps, degraded, self.title)
    }
}

/// A room, rendered as a tab containing tiled panes.
#[derive(Debug, Clone)]
pub struct Tab {
    pub room_id: String,
    pub title: String,
    pub panes: Vec<Pane>,
    /// Index into `panes`. Kept in range by every mutating method.
    focused: usize,
    pub is_encrypted: bool,
    pub unread: Unread,
}

impl Tab {
    pub fn new(room_id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            room_id: room_id.into(),
            title: title.into(),
            panes: Vec::new(),
            focused: 0,
            is_encrypted: false,
            unread: Unread::default(),
        }
    }

    pub fn focused_pane(&self) -> Option<&Pane> {
        self.panes.get(self.focused)
    }

    pub fn focused_pane_mut(&mut self) -> Option<&mut Pane> {
        self.panes.get_mut(self.focused)
    }

    /// Focus a pane by its tiling id. Returns `false` if it is not in this tab.
    pub fn focus(&mut self, id: PaneId) -> bool {
        match self.panes.iter().position(|p| p.id == id) {
            Some(i) => {
                self.focused = i;
                true
            }
            None => false,
        }
    }

    pub fn push_pane(&mut self, pane: Pane) {
        self.panes.push(pane);
        self.focused = self.panes.len() - 1;
    }

    /// Remove a pane, keeping focus on the pane the user was actually looking at.
    pub fn remove_pane(&mut self, id: PaneId) -> Option<Pane> {
        let i = self.panes.iter().position(|p| p.id == id)?;
        let pane = self.panes.remove(i);

        // `focused` indexes a vector that just got shorter, so removing anything below
        // it shifts the pane it names. Clamping alone does not catch that: the index
        // stays in range the whole way through.
        if i < self.focused {
            self.focused -= 1;
        }
        self.focused = self.focused.min(self.panes.len().saturating_sub(1));
        Some(pane)
    }

    pub fn pane_for_thread(&self, root: &str) -> Option<&Pane> {
        self.panes
            .iter()
            .find(|p| p.kind.thread_root() == Some(root))
    }

    /// The tab badge: the most urgent state among its panes.
    pub fn state(&self) -> AgentState {
        self.panes.iter().map(|p| p.state).max().unwrap_or_default()
    }

    /// How many panes are in the given state.
    pub fn count_in(&self, state: AgentState) -> usize {
        self.panes.iter().filter(|p| p.state == state).count()
    }
}

/// A Space, or the implicit orphan workspace.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// Space room ID, or [`ORPHAN_WORKSPACE`].
    pub id: String,
    pub title: String,
    pub tabs: Vec<Tab>,
    focused: usize,
}

impl Workspace {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            tabs: Vec::new(),
            focused: 0,
        }
    }

    pub fn focused_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.focused)
    }

    pub fn focused_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.focused)
    }

    pub fn focus_tab(&mut self, index: usize) -> bool {
        if index < self.tabs.len() {
            self.focused = index;
            true
        } else {
            false
        }
    }

    pub fn next_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.focused = (self.focused + 1) % self.tabs.len();
        }
    }

    pub fn prev_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.focused = (self.focused + self.tabs.len() - 1) % self.tabs.len();
        }
    }

    pub fn tab_for_room(&self, room_id: &str) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.room_id == room_id)
    }

    pub fn tab_for_room_mut(&mut self, room_id: &str) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.room_id == room_id)
    }

    /// The workspace badge: the most urgent state among its tabs.
    pub fn state(&self) -> AgentState {
        self.tabs.iter().map(Tab::state).max().unwrap_or_default()
    }

    pub fn count_in(&self, state: AgentState) -> usize {
        self.tabs.iter().map(|t| t.count_in(state)).sum()
    }

    /// Unread rolled up from every tab, so an unwatched workspace is distinguishable
    /// from an empty one.
    pub fn unread(&self) -> Unread {
        self.tabs.iter().map(|t| t.unread).sum()
    }
}

/// The whole workspace bar.
#[derive(Debug, Clone, Default)]
pub struct Workspaces {
    pub items: Vec<Workspace>,
    focused: usize,
}

impl Workspaces {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn focused(&self) -> Option<&Workspace> {
        self.items.get(self.focused)
    }

    pub fn focused_mut(&mut self) -> Option<&mut Workspace> {
        self.items.get_mut(self.focused)
    }

    pub fn focus(&mut self, index: usize) -> bool {
        if index < self.items.len() {
            self.focused = index;
            true
        } else {
            false
        }
    }

    /// Cycle to the next workspace, wrapping. Mirrors [`Workspace::next_tab`].
    pub fn next(&mut self) {
        if !self.items.is_empty() {
            self.focused = (self.focused + 1) % self.items.len();
        }
    }

    pub fn prev(&mut self) {
        if !self.items.is_empty() {
            self.focused = (self.focused + self.items.len() - 1) % self.items.len();
        }
    }

    pub fn entry(&mut self, id: &str, title: &str) -> &mut Workspace {
        if let Some(i) = self.items.iter().position(|w| w.id == id) {
            return &mut self.items[i];
        }
        self.items.push(Workspace::new(id, title));
        let last = self.items.len() - 1;
        &mut self.items[last]
    }

    pub fn locate_room(&self, room_id: &str) -> Option<(usize, usize)> {
        self.items.iter().enumerate().find_map(|(wi, w)| {
            w.tabs
                .iter()
                .position(|t| t.room_id == room_id)
                .map(|ti| (wi, ti))
        })
    }

    /// Global badge, shown in the top-right.
    pub fn state(&self) -> AgentState {
        self.items
            .iter()
            .map(Workspace::state)
            .max()
            .unwrap_or_default()
    }

    /// Total panes in a given state across everything.
    pub fn count_in(&self, state: AgentState) -> usize {
        self.items.iter().map(|w| w.count_in(state)).sum()
    }

    /// Unread rolled up across every workspace.
    pub fn unread(&self) -> Unread {
        self.items.iter().map(Workspace::unread).sum()
    }

    /// Order workspaces so the ones needing attention come first, then alphabetically.
    ///
    /// The alphabetical tiebreak is what makes this deterministic, not `sort_by` being
    /// stable: the comparator is a total order, so insertion order never survives it.
    pub fn sort_by_urgency(&mut self) {
        let focused_id = self.focused().map(|w| w.id.clone());
        self.items.sort_by(|a, b| {
            b.state()
                .cmp(&a.state())
                .then_with(|| a.title.cmp(&b.title))
        });
        if let Some(id) = focused_id {
            if let Some(i) = self.items.iter().position(|w| w.id == id) {
                self.focused = i;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    fn pane(n: u64, state: AgentState) -> Pane {
        let mut p = Pane::new(
            PaneId::new(n),
            PaneKind::Thread {
                room_id: "!r:x".into(),
                root: format!("$t{n}"),
            },
            format!("thread {n}"),
        );
        p.state = state;
        p
    }

    #[test]
    fn tab_badge_takes_the_most_urgent_pane() {
        let mut tab = Tab::new("!r:x", "#backend");
        tab.push_pane(pane(1, AgentState::Idle));
        tab.push_pane(pane(2, AgentState::Blocked));
        tab.push_pane(pane(3, AgentState::Working));
        assert_eq!(tab.state(), AgentState::Blocked);
        assert_eq!(tab.count_in(AgentState::Working), 1);
    }

    #[test]
    fn unread_labels_distinguish_a_mention_from_mere_traffic() {
        assert_eq!(Unread::default().label(), None);
        assert_eq!(Unread::new(4, 0).label().as_deref(), Some("(4)"));
        assert_eq!(Unread::new(4, 1).label().as_deref(), Some("(@4)"));
    }

    #[test]
    fn a_highlight_without_a_notification_still_counts() {
        // Some push rules raise a highlight without incrementing the notification
        // counter. Trusting `notifications` alone would print `(@0)`.
        let unread = Unread::new(0, 2);
        assert!(unread.any());
        assert_eq!(unread.count(), 2);
        assert_eq!(unread.label().as_deref(), Some("(@2)"));
    }

    #[test]
    fn unread_rolls_up_from_tabs_to_the_whole_bar() {
        let mut ws = Workspaces::new();

        let quiet = ws.entry("!a:x", "alpha");
        let mut tab = Tab::new("!r1:x", "r1");
        tab.unread = Unread::new(3, 0);
        quiet.tabs.push(tab);

        let loud = ws.entry("!b:x", "bravo");
        let mut one = Tab::new("!r2:x", "r2");
        one.unread = Unread::new(5, 1);
        let mut two = Tab::new("!r3:x", "r3");
        two.unread = Unread::new(2, 0);
        loud.tabs.push(one);
        loud.tabs.push(two);

        assert_eq!(ws.items[0].unread(), Unread::new(3, 0));
        assert_eq!(ws.items[1].unread(), Unread::new(7, 1));
        assert_eq!(ws.unread(), Unread::new(10, 1));
        assert!(ws.unread().is_highlight());
    }

    #[test]
    fn an_unread_workspace_is_not_silent_just_because_no_agent_is_running() {
        // The bug this exists to prevent: a room arriving in a workspace nobody is
        // looking at, with no agent attached, and nothing on screen to say so.
        let mut ws = Workspaces::new();
        let w = ws.entry("~", ORPHAN_WORKSPACE);
        let mut tab = Tab::new("!r:x", "Another");
        tab.unread = Unread::new(1, 0);
        w.tabs.push(tab);

        assert_eq!(ws.state(), AgentState::Idle);
        assert!(ws.unread().any(), "unread must survive an idle agent state");
    }

    #[test]
    fn badges_roll_all_the_way_up() {
        let mut ws = Workspaces::new();
        let w = ws.entry("!space:x", "hermes-proj");
        let mut tab = Tab::new("!r:x", "#backend");
        tab.push_pane(pane(1, AgentState::Blocked));
        w.tabs.push(tab);

        assert_eq!(ws.state(), AgentState::Blocked);
        assert_eq!(ws.count_in(AgentState::Blocked), 1);
    }

    #[test]
    fn entry_is_idempotent() {
        let mut ws = Workspaces::new();
        ws.entry("!s:x", "one");
        ws.entry("!s:x", "one");
        assert_eq!(ws.items.len(), 1);
    }

    #[test]
    fn closing_a_pane_keeps_focus_in_range() {
        let mut tab = Tab::new("!r:x", "#backend");
        tab.push_pane(pane(1, AgentState::Idle));
        tab.push_pane(pane(2, AgentState::Idle));
        tab.push_pane(pane(3, AgentState::Idle));
        assert_eq!(tab.focused_pane().expect("focused").id, PaneId::new(3));

        tab.remove_pane(PaneId::new(3));
        assert_eq!(
            tab.focused_pane().expect("focused").id,
            PaneId::new(2),
            "focus should land on a neighbour, not snap to the first pane"
        );

        tab.remove_pane(PaneId::new(1));
        tab.remove_pane(PaneId::new(2));
        assert!(tab.focused_pane().is_none());
    }

    #[test]
    fn closing_a_pane_below_the_focused_one_does_not_move_the_focus() {
        // Otherwise the user carries on typing into a different conversation, with
        // nothing on screen to say it changed.
        let mut tab = Tab::new("!r:x", "#backend");
        for id in 1..=4 {
            tab.push_pane(pane(id, AgentState::Idle));
        }
        tab.focused = 1;
        assert_eq!(tab.focused_pane().expect("focused").id, PaneId::new(2));

        tab.remove_pane(PaneId::new(1));

        assert_eq!(
            tab.focused_pane().expect("focused").id,
            PaneId::new(2),
            "closing another pane must not change which pane you are looking at"
        );
    }

    #[test]
    fn closing_a_pane_above_the_focused_one_leaves_it_alone_too() {
        let mut tab = Tab::new("!r:x", "#backend");
        for id in 1..=4 {
            tab.push_pane(pane(id, AgentState::Idle));
        }
        tab.focused = 1;

        tab.remove_pane(PaneId::new(4));

        assert_eq!(tab.focused_pane().expect("focused").id, PaneId::new(2));
    }

    #[test]
    fn tab_cycling_wraps_both_ways() {
        let mut w = Workspace::new("~", "orphans");
        w.tabs.push(Tab::new("!a:x", "a"));
        w.tabs.push(Tab::new("!b:x", "b"));

        w.next_tab();
        assert_eq!(w.focused_tab().expect("tab").room_id, "!b:x");
        w.next_tab();
        assert_eq!(w.focused_tab().expect("tab").room_id, "!a:x");
        w.prev_tab();
        assert_eq!(w.focused_tab().expect("tab").room_id, "!b:x");
    }

    #[test]
    fn cycling_an_empty_workspace_does_not_panic() {
        let mut w = Workspace::new("~", "empty");
        w.next_tab();
        w.prev_tab();
        assert!(w.focused_tab().is_none());
    }

    #[test]
    fn threads_are_locatable_by_root() {
        let mut tab = Tab::new("!r:x", "#backend");
        tab.push_pane(pane(1, AgentState::Idle));
        tab.push_pane(pane(2, AgentState::Idle));
        assert_eq!(tab.pane_for_thread("$t2").expect("pane").id, PaneId::new(2));
        assert!(tab.pane_for_thread("$nope").is_none());
    }

    #[test]
    fn urgency_sort_keeps_the_focused_workspace_focused() {
        let mut ws = Workspaces::new();
        for (id, title) in [("!a:x", "alpha"), ("!b:x", "bravo"), ("!c:x", "charlie")] {
            let w = ws.entry(id, title);
            w.tabs.push(Tab::new("!r:x", "r"));
        }
        ws.focus(0);

        // Make the last workspace blocked; it should sort to the front without
        // dragging focus with it.
        let w = ws.entry("!c:x", "charlie");
        w.tabs[0].push_pane(pane(1, AgentState::Blocked));

        ws.sort_by_urgency();
        assert_eq!(ws.items[0].id, "!c:x");
        assert_eq!(
            ws.focused().expect("focused").id,
            "!a:x",
            "sorting must not move the user's focus"
        );
    }

    #[test]
    fn rooms_are_locatable_across_workspaces() {
        let mut ws = Workspaces::new();
        ws.entry("!a:x", "alpha").tabs.push(Tab::new("!r1:x", "r1"));
        ws.entry("!b:x", "bravo").tabs.push(Tab::new("!r2:x", "r2"));
        assert_eq!(ws.locate_room("!r2:x"), Some((1, 0)));
        assert_eq!(ws.locate_room("!nope:x"), None);
    }

    #[test]
    fn pane_header_shows_badge_and_lossiness_markers() {
        let mut p = pane(1, AgentState::Working);
        assert_eq!(p.header(), "● thread 1");
        p.degraded = true;
        assert_eq!(p.header(), "● ~thread 1");
        p.gaps = true;
        assert_eq!(p.header(), "● !~thread 1");
        p.degraded = false;
        assert_eq!(p.header(), "● !thread 1");
    }
}
