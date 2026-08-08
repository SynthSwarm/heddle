//! Application state and the update function.
//!
//! Deliberately separated from terminal I/O so the interesting transitions — focus
//! movement, composer editing, approval resolution — are testable without a terminal.

use crate::config::Config;
use crate::keymap::{Action, Mode, Prefix};
use heddle_agent::{AgentState, AgentStore};
use heddle_layout::{Dir, Pane, PaneId, PaneKind, Tab, Tiling, Workspaces, ORPHAN_WORKSPACE};
use heddle_matrix::{Command, Entry, RoomSummary, SyncState, View, WorkerEvent};
use heddle_render::{Options, Overrides, Theme};
use std::collections::{HashMap, HashSet};

/// How much of a split to move per resize keypress.
const RESIZE_STEP: f32 = 0.05;

/// How close to the top of the loaded transcript the user must scroll before older
/// events are requested. A margin rather than zero, so the page arrives before the
/// scrollback runs out rather than after it visibly stops.
const PAGINATE_MARGIN: u16 = 20;

/// Maximum worker events folded into state per frame. Beyond this the rest wait for the
/// next frame, so a sync burst cannot stall input.
pub const EVENT_BUDGET: usize = 128;

/// A clickable cell on one of the bars, in absolute terminal columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    /// First column, inclusive.
    pub x0: u16,
    /// Last column, exclusive.
    pub x1: u16,
    pub index: usize,
}

impl Hit {
    fn contains(&self, column: u16) -> bool {
        column >= self.x0 && column < self.x1
    }
}

/// Where the bars were drawn, recorded by the renderer so clicks can be routed back.
///
/// Only the renderer knows this: cell widths depend on labels, padding and emoji width.
#[derive(Debug, Clone, Default)]
pub struct BarHits {
    pub workspace_row: u16,
    pub workspaces: Vec<Hit>,
    pub tab_row: u16,
    pub tabs: Vec<Hit>,
    /// Column span of the `+` affordance on the tab bar.
    pub new_tab: Option<(u16, u16)>,
}

/// Everything the UI draws from.
pub struct App {
    pub config: Config,
    pub theme: Theme,
    pub render_options: Options,
    pub overrides: Overrides,

    pub workspaces: Workspaces,
    /// One tiling tree per room, keyed by room ID.
    pub tilings: HashMap<String, Tiling>,
    /// Timeline entries per view.
    pub timelines: HashMap<View, Vec<Entry>>,
    /// Agent sessions, keyed by thread root.
    pub agents: AgentStore,

    pub mode: Mode,
    pub prefix: Prefix,
    pub composer: String,
    pub scroll: HashMap<View, u16>,
    pub sync: SyncState,
    pub status: Option<String>,

    /// Views with a pagination request in flight. Without this, holding `<c-u>` at the
    /// top of a transcript would queue one request per keypress.
    paginating: HashSet<View>,
    /// Rendered line count of the focused transcript, recorded by the renderer. The app
    /// cannot compute it: wrapping depends on the pane width.
    pub rendered_lines: u16,
    /// Visible height of the focused pane, recorded by the renderer.
    pub viewport_height: u16,
    /// Clickable regions of the workspace and tab bars, recorded by the renderer.
    pub bars: BarHits,
    /// Whether the `<prefix> ?` key overlay is showing.
    pub help: bool,

    pub should_quit: bool,
    /// Commands produced by the last update, drained by the caller.
    pending: Vec<Command>,
}

impl App {
    pub fn new(config: Config) -> Self {
        let prefix = Prefix::parse(&config.ui.prefix).unwrap_or_else(|| {
            tracing::warn!(spec = %config.ui.prefix, "unparseable ui.prefix; using ctrl+a");
            Prefix::default()
        });

        let render_options = Options {
            auto_expand: config.agent.auto_expand(),
            show_commentary: config.agent.show_commentary,
            mark_degraded: true,
        };

        Self {
            config,
            theme: Theme::default(),
            render_options,
            overrides: Overrides::new(),
            workspaces: Workspaces::new(),
            tilings: HashMap::new(),
            timelines: HashMap::new(),
            agents: AgentStore::new(),
            mode: Mode::Normal,
            prefix,
            composer: String::new(),
            scroll: HashMap::new(),
            sync: SyncState::Idle,
            status: None,
            paginating: HashSet::new(),
            rendered_lines: 0,
            viewport_height: 0,
            bars: BarHits::default(),
            help: false,
            should_quit: false,
            pending: Vec::new(),
        }
    }

    /// Take the commands produced since the last call.
    pub fn take_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.pending)
    }

    fn queue(&mut self, command: Command) {
        self.pending.push(command);
    }

    /// The view the focused pane is showing.
    pub fn focused_view(&self) -> Option<View> {
        let tab = self.workspaces.focused()?.focused_tab()?;
        let pane = tab.focused_pane()?;
        Some(match pane.kind.thread_root() {
            Some(root) => View::thread(&tab.room_id, root),
            None => View::room(&tab.room_id),
        })
    }

    pub fn focused_entries(&self) -> &[Entry] {
        self.focused_view()
            .and_then(|v| self.timelines.get(&v))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    // ---------------------------------------------------------------- worker events

    pub fn apply_worker_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::SyncState(state) => self.sync = state,

            WorkerEvent::Rooms(rooms) => {
                self.apply_rooms(rooms);
                // The app opens its focused view once at startup, but the room list has
                // not arrived yet at that point so there is nothing to focus and the
                // call is a no-op. Without re-trying here the transcript stays empty
                // until the user happens to touch a pane or a tab.
                self.open_focused_view();
            }

            WorkerEvent::Timeline { view, entries } => {
                // Whatever the request achieved, it is no longer in flight. Clearing on
                // any snapshot also covers the case where pagination returned nothing.
                self.paginating.remove(&view);
                self.ingest_agent_events(&view, &entries);
                self.timelines.insert(view, entries);
            }

            WorkerEvent::Typing { room_id, users } => {
                // An agent typing means `working`, which is what makes a pane look busy
                // during model latency, before the first token lands.
                let typing = users.iter().any(|u| self.config.agent.is_agent(u));
                for session in self.session_ids_for_room(&room_id) {
                    if let Some(s) = self.agents.get_mut(&session) {
                        s.set_typing(typing);
                    }
                }
                self.refresh_pane_states();
            }

            WorkerEvent::Warning(text) => self.status = Some(text),

            WorkerEvent::Fatal(text) => {
                self.status = Some(format!("fatal: {text}"));
                self.should_quit = true;
            }
        }
    }

    fn apply_rooms(&mut self, rooms: Vec<RoomSummary>) {
        // Spaces become workspaces; everything else becomes a tab. Rooms with no parent
        // Space collect in the implicit orphan workspace.
        let spaces: Vec<&RoomSummary> = rooms.iter().filter(|r| r.is_space).collect();
        for space in &spaces {
            self.workspaces.entry(&space.room_id, &space.display_name);
        }

        for room in rooms.iter().filter(|r| !r.is_space) {
            let workspace_id = room
                .parents
                .first()
                .cloned()
                .unwrap_or_else(|| ORPHAN_WORKSPACE.to_owned());
            // The orphan workspace is titled with its own marker rather than a word, so
            // it reads as "the rooms with no Space" instead of a section heading.
            // SPEC.md §2.
            let title = if workspace_id == ORPHAN_WORKSPACE {
                ORPHAN_WORKSPACE
            } else {
                &room.display_name
            };

            let workspace = self.workspaces.entry(&workspace_id, title);
            match workspace.tab_for_room_mut(&room.room_id) {
                Some(tab) => {
                    tab.title.clone_from(&room.display_name);
                    tab.is_encrypted = room.is_encrypted;
                    tab.notification_count = room.notification_count;
                    tab.highlight_count = room.highlight_count;
                }
                None => {
                    let mut tab = Tab::new(&room.room_id, &room.display_name);
                    tab.is_encrypted = room.is_encrypted;
                    tab.notification_count = room.notification_count;
                    tab.highlight_count = room.highlight_count;

                    // Every room starts with a root pane showing its main timeline.
                    let mut tiling = Tiling::new();
                    let root = tiling.layout(ratatui::layout::Rect::ZERO);
                    if let Some(first) = root.first() {
                        tab.push_pane(Pane::new(
                            first.id,
                            PaneKind::Room {
                                room_id: room.room_id.clone(),
                            },
                            room.display_name.clone(),
                        ));
                    }
                    self.tilings.insert(room.room_id.clone(), tiling);
                    workspace.tabs.push(tab);
                }
            }
        }
    }

    /// Fold a view's agent events into the store.
    fn ingest_agent_events(&mut self, view: &View, entries: &[Entry]) {
        use heddle_matrix::{AgentPayload, EntryKind};

        for entry in entries {
            let EntryKind::Message(message) = &entry.kind else {
                continue;
            };
            if !self.config.agent.is_agent(&message.sender) {
                continue;
            }
            match &message.agent {
                AgentPayload::Structured(event) => self.agents.apply(event),
                AgentPayload::Degraded(_) => {
                    if let Some(root) = &view.thread_root {
                        self.agents.mark_degraded(root);
                    }
                }
                AgentPayload::None => {}
            }
        }
        self.refresh_pane_states();
    }

    fn session_ids_for_room(&self, room_id: &str) -> Vec<String> {
        self.workspaces
            .items
            .iter()
            .flat_map(|w| w.tabs.iter())
            .filter(|t| t.room_id == room_id)
            .flat_map(|t| t.panes.iter())
            .filter_map(|p| p.kind.thread_root().map(str::to_owned))
            .collect()
    }

    /// Copy derived agent state onto panes so badges roll up.
    fn refresh_pane_states(&mut self) {
        for workspace in &mut self.workspaces.items {
            for tab in &mut workspace.tabs {
                for pane in &mut tab.panes {
                    let Some(root) = pane.kind.thread_root() else {
                        continue;
                    };
                    if let Some(session) = self.agents.get(root) {
                        pane.state = session.state();
                        pane.degraded = session.degraded;
                    }
                }
            }
        }
    }

    // ---------------------------------------------------------------------- actions

    pub fn apply_action(&mut self, action: Action) {
        match action {
            Action::None => {}
            Action::Quit => self.should_quit = true,

            Action::EnterInsert => self.mode = Mode::Insert,
            Action::EnterNormal => self.mode = Mode::Normal,

            Action::Insert(c) => self.composer.push(c),
            Action::Backspace => {
                self.composer.pop();
            }
            Action::Newline => self.composer.push('\n'),
            Action::Submit => self.submit(),

            Action::Split(dir) => self.split(dir),
            Action::ClosePane => self.close_pane(),
            Action::FocusPane(dir) => self.focus_pane(dir),
            Action::ResizePane(dir) => self.resize_pane(dir),
            Action::ZoomPane => {
                if let Some(tiling) = self.focused_tiling_mut() {
                    tiling.toggle_zoom();
                }
            }
            Action::NewThread => {
                self.status = Some("new thread: send a message to start one".into())
            }

            Action::NextTab => {
                if let Some(w) = self.workspaces.focused_mut() {
                    w.next_tab();
                }
                self.mark_focused_seen();
                self.open_focused_view();
            }
            Action::PrevTab => {
                if let Some(w) = self.workspaces.focused_mut() {
                    w.prev_tab();
                }
                self.mark_focused_seen();
                self.open_focused_view();
            }

            Action::ScrollUp(n) => self.scroll_by(-(n as i32)),
            Action::ScrollDown(n) => self.scroll_by(n as i32),
            Action::ScrollTop => self.set_scroll(u16::MAX),
            Action::ScrollBottom => self.set_scroll(0),

            Action::ToggleCard => self.toggle_card(),

            Action::Approve => self.resolve_prompt(true),
            Action::Deny => self.resolve_prompt(false),

            Action::ToggleHelp => self.help = !self.help,
            Action::CloseHelp => self.help = false,

            Action::WorkspaceSwitcher | Action::FuzzyJump | Action::CommandPalette => {
                // Overlays land in M4 alongside the rest of the workspace UI.
                self.status = Some("not implemented yet".into());
            }
        }
    }

    fn submit(&mut self) {
        let body = self.composer.trim().to_owned();
        if body.is_empty() {
            return;
        }
        let Some(view) = self.focused_view() else {
            self.status = Some("no pane focused".into());
            return;
        };
        self.composer.clear();
        self.queue(Command::SendMessage { view, body });
    }

    fn focused_tiling_mut(&mut self) -> Option<&mut Tiling> {
        let room_id = self.workspaces.focused()?.focused_tab()?.room_id.clone();
        self.tilings.get_mut(&room_id)
    }

    fn split(&mut self, dir: Dir) {
        let Some(view) = self.focused_view() else {
            return;
        };
        let Some(new_id) = self.focused_tiling_mut().and_then(|t| t.split(dir)) else {
            return;
        };
        // A new pane starts on the same view as the one it was split from; the user
        // then navigates it elsewhere. Splitting into an empty pane would be useless.
        let title = view
            .thread_root
            .clone()
            .unwrap_or_else(|| "room".to_owned());
        let kind = match &view.thread_root {
            Some(root) => PaneKind::Thread {
                room_id: view.room_id.clone(),
                root: root.clone(),
            },
            None => PaneKind::Room {
                room_id: view.room_id.clone(),
            },
        };
        if let Some(tab) = self
            .workspaces
            .focused_mut()
            .and_then(Workspace_focused_tab_mut)
        {
            tab.push_pane(Pane::new(new_id, kind, title));
        }
    }

    fn close_pane(&mut self) {
        let Some(closed) = self.focused_tiling_mut().and_then(Tiling::close_focused) else {
            return;
        };
        let focused = self.focused_tiling_mut().and_then(|t| t.focused());
        if let Some(tab) = self
            .workspaces
            .focused_mut()
            .and_then(Workspace_focused_tab_mut)
        {
            tab.remove_pane(closed);
            if let Some(id) = focused {
                tab.focus(id);
            }
        }
        self.open_focused_view();
    }

    fn focus_pane(&mut self, dir: Dir) {
        let Some(id) = self.focused_tiling_mut().and_then(|t| t.focus_dir(dir)) else {
            return;
        };
        if let Some(tab) = self
            .workspaces
            .focused_mut()
            .and_then(Workspace_focused_tab_mut)
        {
            tab.focus(id);
        }
        self.mark_focused_seen();
        self.open_focused_view();
    }

    fn resize_pane(&mut self, dir: Dir) {
        if let Some(tiling) = self.focused_tiling_mut() {
            tiling.resize(dir, RESIZE_STEP);
        }
    }

    /// Focus a pane by its tiling id, for click-to-focus.
    pub fn focus_pane_id(&mut self, id: PaneId) {
        if let Some(tiling) = self.focused_tiling_mut() {
            tiling.focus(id);
        }
        if let Some(tab) = self
            .workspaces
            .focused_mut()
            .and_then(Workspace_focused_tab_mut)
        {
            tab.focus(id);
        }
        self.mark_focused_seen();
        self.open_focused_view();
    }

    /// Looking at a pane collapses its `Done` badge to `Idle` and sends a read receipt.
    fn mark_focused_seen(&mut self) {
        let Some(view) = self.focused_view() else {
            return;
        };
        // Only tell the server about views we are actually streaming; the worker needs
        // an open timeline to place the receipt against.
        if self.timelines.contains_key(&view) {
            self.queue(Command::MarkRead { view });
        }

        let Some(root) = self
            .workspaces
            .focused()
            .and_then(|w| w.focused_tab())
            .and_then(|t| t.focused_pane())
            .and_then(|p| p.kind.thread_root().map(str::to_owned))
        else {
            return;
        };
        if let Some(session) = self.agents.get_mut(&root) {
            session.mark_seen();
        }
        self.refresh_pane_states();
    }

    /// Route a left click on the workspace or tab bar.
    ///
    /// Returns `true` when the click landed on a bar, so the caller does not also
    /// hit-test the panes. Clicks anywhere on a bar are swallowed, including the gaps
    /// between cells: falling through to the tiling would focus a pane the user did not
    /// aim at.
    pub fn click_bar(&mut self, column: u16, row: u16) -> bool {
        if row == self.bars.workspace_row && !self.bars.workspaces.is_empty() {
            let index = self
                .bars
                .workspaces
                .iter()
                .find(|h| h.contains(column))
                .map(|h| h.index);
            if let Some(index) = index {
                if self.workspaces.focus(index) {
                    self.mark_focused_seen();
                    self.open_focused_view();
                }
            }
            return true;
        }

        if row == self.bars.tab_row {
            if self.bars.new_tab.is_some_and(|(x0, x1)| {
                let hit = Hit { x0, x1, index: 0 };
                hit.contains(column)
            }) {
                self.apply_action(Action::NewThread);
                return true;
            }
            let index = self
                .bars
                .tabs
                .iter()
                .find(|h| h.contains(column))
                .map(|h| h.index);
            if let Some(index) = index {
                if self
                    .workspaces
                    .focused_mut()
                    .is_some_and(|w| w.focus_tab(index))
                {
                    self.mark_focused_seen();
                    self.open_focused_view();
                }
            }
            return true;
        }

        false
    }

    /// Ask the worker for the focused view if it is not already streaming.
    pub fn open_focused_view(&mut self) {
        let Some(view) = self.focused_view() else {
            return;
        };
        if !self.timelines.contains_key(&view) {
            self.queue(Command::OpenView(view.clone()));
            // A live timeline starts with only what sync delivered, which for a room
            // opened at launch is usually nothing. Without this first page the pane is
            // simply empty and the client looks broken. The worker handles commands in
            // order and `OpenView` is awaited, so the timeline exists by the time this
            // is processed.
            self.paginating.insert(view.clone());
            self.queue(Command::Paginate { view, count: 0 });
        }
    }

    /// The largest meaningful scroll offset: one screen short of the oldest line.
    ///
    /// Derived from the geometry the renderer recorded, since wrapping depends on the
    /// pane width and the app cannot know it.
    fn max_scroll(&self) -> u16 {
        self.rendered_lines.saturating_sub(self.viewport_height)
    }

    fn scroll_by(&mut self, delta: i32) {
        let Some(view) = self.focused_view() else {
            return;
        };
        let current = *self.scroll.get(&view).unwrap_or(&0);
        // Scroll is an offset *from the bottom*, so scrolling up increases it.
        //
        // Clamping at the top is what makes scrolling back down work. Unclamped, every
        // keypress past the oldest line increments a counter with no visible effect,
        // and scrolling down then has to unwind all of it before the transcript moves —
        // which looks exactly like the newest messages having been lost.
        let next = ((current as i32 - delta).max(0) as u16).min(self.max_scroll());
        self.scroll.insert(view.clone(), next);
        if delta < 0 {
            self.paginate_if_near_top(&view, next);
        }
    }

    /// Request older events once the user is within [`PAGINATE_MARGIN`] of the top.
    fn paginate_if_near_top(&mut self, view: &View, scroll: u16) {
        // Already at the start of the room: there is nothing older to fetch, and asking
        // anyway would re-request on every keypress.
        if self
            .timelines
            .get(view)
            .and_then(|entries| entries.first())
            .is_some_and(|first| matches!(first.kind, heddle_matrix::EntryKind::TimelineStart))
        {
            return;
        }

        let ceiling = self.rendered_lines.saturating_sub(self.viewport_height);
        if scroll + PAGINATE_MARGIN < ceiling {
            return;
        }
        if !self.paginating.insert(view.clone()) {
            return;
        }
        self.queue(Command::Paginate {
            view: view.clone(),
            count: 0,
        });
    }

    fn set_scroll(&mut self, value: u16) {
        // `ScrollTop` passes u16::MAX rather than computing the ceiling itself. Left
        // unclamped that would need 65,000 keypresses to scroll back to the bottom.
        let value = value.min(self.max_scroll());
        let Some(view) = self.focused_view() else {
            return;
        };
        self.scroll.insert(view.clone(), value);
        // Jumping to the top is as much a pagination trigger as scrolling there.
        self.paginate_if_near_top(&view, value);
    }

    /// Toggle the newest tool card in the focused view.
    fn toggle_card(&mut self) {
        use heddle_matrix::{AgentPayload, EntryKind};

        let Some(view) = self.focused_view() else {
            return;
        };
        let Some(entries) = self.timelines.get(&view) else {
            return;
        };

        let newest = entries.iter().rev().find_map(|entry| {
            let EntryKind::Message(message) = &entry.kind else {
                return None;
            };
            let AgentPayload::Structured(event) = &message.agent else {
                return None;
            };
            let tool = event.tool.as_ref()?;
            Some((entry.event_id.clone()?, tool.index, tool.clone()))
        });

        let Some((event_id, index, tool)) = newest else {
            return;
        };
        let key = (event_id, index);
        let currently = heddle_render::card::is_expanded(
            &tool,
            self.render_options.auto_expand,
            self.overrides.get(&key).copied(),
        );
        self.overrides.insert(key, !currently);
    }

    /// Resolve the focused pane's pending prompt by sending the matching reaction.
    fn resolve_prompt(&mut self, approve: bool) {
        use heddle_agent::{ApprovalChoice, Pending};

        let Some(view) = self.focused_view() else {
            return;
        };
        let Some(root) = view.thread_root.clone() else {
            return;
        };
        let Some(session) = self.agents.get(&root) else {
            return;
        };
        let Some(pending) = session.pending.first() else {
            return;
        };

        let (id, emoji) = match pending {
            Pending::Approval(approval) => {
                // Prefer the emoji Hermes actually advertised; fall back to the
                // conventional pair so an older gateway still works.
                let wanted = if approve { "approve" } else { "deny" };
                let emoji = approval
                    .reactions
                    .iter()
                    .find(|(_, choice)| choice.as_str() == wanted)
                    .map(|(emoji, _)| emoji.clone())
                    .unwrap_or_else(|| if approve { "✅" } else { "❌" }.to_owned());
                (approval.id.clone(), emoji)
            }
            Pending::Picker(_) => {
                self.status = Some("use the picker overlay to choose a model".into());
                return;
            }
        };

        // The prompt's own event is what carries the reaction.
        let Some(event_id) = self.prompt_event_id(&view, &id) else {
            self.status = Some("cannot locate the approval event".into());
            return;
        };

        self.queue(Command::ToggleReaction {
            view,
            event_id,
            key: emoji,
        });
        self.agents.resolve_pending(
            &root,
            &id,
            if approve {
                ApprovalChoice::Approve
            } else {
                ApprovalChoice::Deny
            },
        );
        self.refresh_pane_states();
    }

    fn prompt_event_id(&self, view: &View, approval_id: &str) -> Option<String> {
        use heddle_matrix::{AgentPayload, EntryKind};

        self.timelines.get(view)?.iter().rev().find_map(|entry| {
            let EntryKind::Message(message) = &entry.kind else {
                return None;
            };
            let AgentPayload::Structured(event) = &message.agent else {
                return None;
            };
            let approval = event.approval.as_ref()?;
            (approval.id == approval_id)
                .then(|| entry.event_id.clone())
                .flatten()
        })
    }

    /// Global badge and count for the status bar.
    pub fn badge(&self) -> (AgentState, usize) {
        let state = self.workspaces.state();
        (state, self.workspaces.count_in(state))
    }
}

/// Helper so `and_then` can borrow a tab mutably out of a workspace.
#[allow(non_snake_case)]
fn Workspace_focused_tab_mut(w: &mut heddle_layout::Workspace) -> Option<&mut Tab> {
    w.focused_tab_mut()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use heddle_agent::{AgentEvent, Approval, Kind, Tool, ToolStatus};
    use heddle_matrix::{AgentPayload, EntryKind, Message};

    fn app() -> App {
        let mut app = App::new(Config::default());
        app.apply_worker_event(WorkerEvent::Rooms(vec![RoomSummary {
            room_id: "!r:x".into(),
            display_name: "#backend".into(),
            is_space: false,
            parents: Vec::new(),
            is_direct: false,
            is_encrypted: false,
            notification_count: 0,
            highlight_count: 0,
        }]));
        // The room list now opens the focused view, so drain the resulting OpenView and
        // Paginate. Tests asserting on commands want to see only what they triggered.
        let _ = app.take_commands();
        app
    }

    fn agent_entry(event_id: &str, event: AgentEvent) -> Entry {
        Entry {
            id: event_id.into(),
            event_id: Some(event_id.into()),
            kind: EntryKind::Message(Message {
                sender: "@hermes:x".into(),
                sender_display: "hermes".into(),
                body: "chrome".into(),
                timestamp: 0,
                is_own: false,
                is_edited: false,
                thread_root: Some("$root".into()),
                reactions: Vec::new(),
                agent: AgentPayload::Structured(Box::new(event)),
            }),
        }
    }

    fn event(kind: Kind) -> AgentEvent {
        AgentEvent {
            v: 1,
            session_id: "$root".into(),
            turn_id: "t1".into(),
            seq: 1,
            kind,
            agent: None,
            text: None,
            final_: None,
            tool: None,
            notice: None,
            approval: None,
            picker: None,
            usage: None,
        }
    }

    #[test]
    fn rooms_become_tabs_with_a_root_pane() {
        let app = app();
        let workspace = app.workspaces.focused().expect("workspace");
        let tab = workspace.focused_tab().expect("tab");
        assert_eq!(tab.room_id, "!r:x");
        assert_eq!(tab.panes.len(), 1);
        assert_eq!(app.focused_view(), Some(View::room("!r:x")));
    }

    #[test]
    fn spaces_become_workspaces_not_tabs() {
        let mut app = App::new(Config::default());
        app.apply_worker_event(WorkerEvent::Rooms(vec![RoomSummary {
            room_id: "!space:x".into(),
            display_name: "hermes-proj".into(),
            is_space: true,
            parents: Vec::new(),
            is_direct: false,
            is_encrypted: false,
            notification_count: 0,
            highlight_count: 0,
        }]));
        assert_eq!(app.workspaces.items.len(), 1);
        assert_eq!(app.workspaces.items[0].id, "!space:x");
        assert!(app.workspaces.items[0].tabs.is_empty());
    }

    #[test]
    fn a_room_update_does_not_duplicate_its_tab() {
        let mut app = app();
        let room = RoomSummary {
            room_id: "!r:x".into(),
            display_name: "#backend-renamed".into(),
            is_space: false,
            parents: Vec::new(),
            is_direct: false,
            is_encrypted: true,
            notification_count: 3,
            highlight_count: 1,
        };
        app.apply_worker_event(WorkerEvent::Rooms(vec![room]));

        let tab = app
            .workspaces
            .focused()
            .expect("workspace")
            .focused_tab()
            .expect("tab");
        assert_eq!(tab.title, "#backend-renamed");
        assert!(tab.is_encrypted);
        assert_eq!(tab.notification_count, 3);
        assert_eq!(
            app.workspaces.focused().expect("workspace").tabs.len(),
            1,
            "re-sending the room list must not clone tabs"
        );
    }

    #[test]
    fn submitting_sends_and_clears_the_composer() {
        let mut app = app();
        app.composer = "  hello  ".into();
        app.apply_action(Action::Submit);

        assert!(app.composer.is_empty());
        let commands = app.take_commands();
        match commands.as_slice() {
            [Command::SendMessage { view, body }] => {
                assert_eq!(view, &View::room("!r:x"));
                assert_eq!(body, "hello");
            }
            other => panic!("expected one SendMessage, got {other:?}"),
        }
    }

    #[test]
    fn submitting_an_empty_composer_sends_nothing() {
        let mut app = app();
        app.composer = "   \n ".into();
        app.apply_action(Action::Submit);
        assert!(app.take_commands().is_empty());
    }

    #[test]
    fn splitting_adds_a_pane_to_the_tab() {
        let mut app = app();
        app.apply_action(Action::Split(Dir::Right));
        let tab = app
            .workspaces
            .focused()
            .expect("workspace")
            .focused_tab()
            .expect("tab");
        assert_eq!(tab.panes.len(), 2);
    }

    #[test]
    fn closing_the_last_pane_leaves_the_tab_empty_without_panicking() {
        let mut app = app();
        app.apply_action(Action::ClosePane);
        let tab = app
            .workspaces
            .focused()
            .expect("workspace")
            .focused_tab()
            .expect("tab");
        assert!(tab.panes.len() <= 1);
    }

    #[test]
    fn agent_events_drive_pane_state() {
        let mut app = app();

        // Point the root pane at a thread so it maps to an agent session.
        if let Some(tab) = app
            .workspaces
            .focused_mut()
            .and_then(|w| w.focused_tab_mut())
        {
            if let Some(pane) = tab.focused_pane_mut() {
                pane.kind = PaneKind::Thread {
                    room_id: "!r:x".into(),
                    root: "$root".into(),
                };
            }
        }

        let mut call = event(Kind::ToolCall);
        call.tool = Some(Tool {
            name: "bash".into(),
            index: 0,
            args: None,
            preview: Some("cargo test".into()),
            status: ToolStatus::Running,
            duration_ms: None,
            mime: None,
            body: None,
            truncated: false,
        });

        app.apply_worker_event(WorkerEvent::Timeline {
            view: View::thread("!r:x", "$root"),
            entries: vec![agent_entry("$e1", call)],
        });

        let tab = app
            .workspaces
            .focused()
            .expect("workspace")
            .focused_tab()
            .expect("tab");
        assert_eq!(tab.state(), AgentState::Working);
        assert_eq!(app.badge().0, AgentState::Working);
    }

    #[test]
    fn approving_reacts_on_the_prompt_event_and_unblocks() {
        let mut app = app();
        if let Some(tab) = app
            .workspaces
            .focused_mut()
            .and_then(|w| w.focused_tab_mut())
        {
            if let Some(pane) = tab.focused_pane_mut() {
                pane.kind = PaneKind::Thread {
                    room_id: "!r:x".into(),
                    root: "$root".into(),
                };
            }
        }

        let mut request = event(Kind::ApprovalRequest);
        request.approval = Some(Approval {
            id: "a1".into(),
            kind: "exec".into(),
            command: Some("rm -rf ./build".into()),
            cwd: None,
            expires_at: None,
            reactions: [("👍".to_owned(), "approve".to_owned())]
                .into_iter()
                .collect(),
            choice: None,
            by: None,
        });

        let view = View::thread("!r:x", "$root");
        app.apply_worker_event(WorkerEvent::Timeline {
            view: view.clone(),
            entries: vec![agent_entry("$prompt", request)],
        });
        let _ = app.take_commands();

        assert_eq!(app.badge().0, AgentState::Blocked);

        app.apply_action(Action::Approve);
        match app.take_commands().as_slice() {
            [Command::ToggleReaction { event_id, key, .. }] => {
                assert_eq!(event_id, "$prompt");
                assert_eq!(key, "👍", "must use the emoji Hermes advertised");
            }
            other => panic!("expected one ToggleReaction, got {other:?}"),
        }
        assert_ne!(app.badge().0, AgentState::Blocked);
    }

    #[test]
    fn approving_without_advertised_emoji_falls_back_to_the_convention() {
        let mut app = app();
        if let Some(tab) = app
            .workspaces
            .focused_mut()
            .and_then(|w| w.focused_tab_mut())
        {
            if let Some(pane) = tab.focused_pane_mut() {
                pane.kind = PaneKind::Thread {
                    room_id: "!r:x".into(),
                    root: "$root".into(),
                };
            }
        }
        let mut request = event(Kind::ApprovalRequest);
        request.approval = Some(Approval {
            id: "a1".into(),
            kind: "exec".into(),
            command: None,
            cwd: None,
            expires_at: None,
            reactions: Default::default(),
            choice: None,
            by: None,
        });
        app.apply_worker_event(WorkerEvent::Timeline {
            view: View::thread("!r:x", "$root"),
            entries: vec![agent_entry("$prompt", request)],
        });
        let _ = app.take_commands();

        app.apply_action(Action::Deny);
        match app.take_commands().as_slice() {
            [Command::ToggleReaction { key, .. }] => assert_eq!(key, "❌"),
            other => panic!("expected ToggleReaction, got {other:?}"),
        }
    }

    #[test]
    fn opening_a_view_requests_it_and_a_first_page() {
        let mut app = app();
        app.open_focused_view();
        // The first page matters: a live timeline opened at launch is usually empty, so
        // without it the pane renders blank and the client looks broken.
        match app.take_commands().as_slice() {
            [Command::OpenView(a), Command::Paginate { view: b, .. }] => {
                assert_eq!(a, &View::room("!r:x"));
                assert_eq!(b, &View::room("!r:x"));
            }
            other => panic!("expected OpenView then Paginate, got {other:?}"),
        }

        app.apply_worker_event(WorkerEvent::Timeline {
            view: View::room("!r:x"),
            entries: Vec::new(),
        });
        app.open_focused_view();
        assert!(
            app.take_commands().is_empty(),
            "an already-streaming view must not be re-opened"
        );
    }

    #[test]
    fn focusing_a_streaming_view_sends_a_read_receipt() {
        let mut app = app();
        app.apply_worker_event(WorkerEvent::Timeline {
            view: View::room("!r:x"),
            entries: Vec::new(),
        });
        let _ = app.take_commands();

        app.focus_pane_id(
            app.workspaces
                .focused()
                .and_then(|w| w.focused_tab())
                .and_then(|t| t.focused_pane())
                .expect("pane")
                .id,
        );
        assert!(
            app.take_commands()
                .iter()
                .any(|c| matches!(c, Command::MarkRead { .. })),
            "looking at a room must mark it read"
        );
    }

    #[test]
    fn a_view_that_is_not_open_is_not_marked_read() {
        let mut app = app();
        app.mark_focused_seen();
        assert!(
            !app.take_commands()
                .iter()
                .any(|c| matches!(c, Command::MarkRead { .. })),
            "the worker needs an open timeline to place a receipt against"
        );
    }

    #[test]
    fn scrolling_to_the_top_requests_older_events_once() {
        let mut app = app();
        app.apply_worker_event(WorkerEvent::Timeline {
            view: View::room("!r:x"),
            entries: Vec::new(),
        });
        let _ = app.take_commands();

        // Geometry the renderer would have recorded: a long transcript in a short pane.
        app.rendered_lines = 200;
        app.viewport_height = 20;

        app.apply_action(Action::ScrollUp(10));
        assert!(
            app.take_commands().is_empty(),
            "scrolling near the bottom must not paginate"
        );

        app.apply_action(Action::ScrollUp(170));
        assert!(
            app.take_commands()
                .iter()
                .any(|c| matches!(c, Command::Paginate { .. })),
            "reaching the top must request older events"
        );

        // Holding the key down must not queue a request per keypress.
        app.apply_action(Action::ScrollUp(5));
        assert!(
            app.take_commands().is_empty(),
            "a pagination request must not be duplicated while one is in flight"
        );
    }

    #[test]
    fn pagination_stops_at_the_start_of_the_room() {
        let mut app = app();
        app.apply_worker_event(WorkerEvent::Timeline {
            view: View::room("!r:x"),
            entries: vec![Entry {
                id: "start".into(),
                event_id: None,
                kind: heddle_matrix::EntryKind::TimelineStart,
            }],
        });
        let _ = app.take_commands();

        app.rendered_lines = 200;
        app.viewport_height = 20;
        app.apply_action(Action::ScrollUp(200));
        assert!(
            app.take_commands().is_empty(),
            "there is nothing older than the start of the room"
        );
    }

    #[test]
    fn rooms_without_a_space_land_in_the_orphan_workspace() {
        let app = app();
        // SPEC.md §2 names it `~`; a word here reads as a section heading instead.
        assert_eq!(app.workspaces.items[0].id, ORPHAN_WORKSPACE);
        assert_eq!(app.workspaces.items[0].title, ORPHAN_WORKSPACE);
    }

    /// Two rooms, so there is a tab to switch *to*.
    fn app_with_two_rooms() -> App {
        let mut app = App::new(Config::default());
        app.apply_worker_event(WorkerEvent::Rooms(vec![
            RoomSummary {
                room_id: "!a:x".into(),
                display_name: "Commons".into(),
                is_space: false,
                parents: Vec::new(),
                is_direct: false,
                is_encrypted: false,
                notification_count: 0,
                highlight_count: 0,
            },
            RoomSummary {
                room_id: "!b:x".into(),
                display_name: "smith".into(),
                is_space: false,
                parents: Vec::new(),
                is_direct: false,
                is_encrypted: true,
                notification_count: 0,
                highlight_count: 0,
            },
        ]));
        let _ = app.take_commands();
        app
    }

    #[test]
    fn clicking_a_tab_focuses_its_room() {
        let mut app = app_with_two_rooms();
        assert_eq!(app.focused_view(), Some(View::room("!a:x")));

        // Geometry as the renderer would have recorded it.
        app.bars.tab_row = 1;
        app.bars.tabs = vec![
            Hit {
                x0: 1,
                x1: 13,
                index: 0,
            },
            Hit {
                x0: 14,
                x1: 26,
                index: 1,
            },
        ];

        assert!(app.click_bar(20, 1), "a click on the tab bar is handled");
        assert_eq!(
            app.focused_view(),
            Some(View::room("!b:x")),
            "clicking the second tab must focus the second room"
        );
    }

    #[test]
    fn a_click_on_the_bar_never_falls_through_to_a_pane() {
        let mut app = app_with_two_rooms();
        app.bars.tab_row = 1;
        app.bars.tabs = vec![Hit {
            x0: 1,
            x1: 13,
            index: 0,
        }];

        // A gap between cells is still the bar; falling through would focus a pane the
        // user did not aim at.
        assert!(app.click_bar(200, 1));
        // And a row below the bars is not.
        assert!(!app.click_bar(5, 9));
    }

    #[test]
    fn clicking_a_workspace_focuses_it() {
        let mut app = app_with_two_rooms();
        app.workspaces.entry("!space:x", "hermes-proj");
        app.bars.workspace_row = 0;
        app.bars.workspaces = vec![
            Hit {
                x0: 1,
                x1: 13,
                index: 0,
            },
            Hit {
                x0: 14,
                x1: 26,
                index: 1,
            },
        ];

        assert!(app.click_bar(18, 0));
        assert_eq!(app.workspaces.focused().expect("workspace").id, "!space:x");
    }

    #[test]
    fn switching_tabs_marks_the_new_one_read() {
        let mut app = app_with_two_rooms();
        app.apply_worker_event(WorkerEvent::Timeline {
            view: View::room("!b:x"),
            entries: Vec::new(),
        });
        let _ = app.take_commands();

        app.apply_action(Action::NextTab);
        assert_eq!(app.focused_view(), Some(View::room("!b:x")));
        assert!(
            app.take_commands()
                .iter()
                .any(|c| matches!(c, Command::MarkRead { .. })),
            "moving to a tab is looking at it"
        );
    }

    #[test]
    fn a_fatal_worker_error_quits() {
        let mut app = app();
        app.apply_worker_event(WorkerEvent::Fatal("store corrupt".into()));
        assert!(app.should_quit);
        assert!(app.status.expect("status").contains("store corrupt"));
    }

    #[test]
    fn scrolling_never_goes_below_the_bottom() {
        let mut app = app();
        app.rendered_lines = 200;
        app.viewport_height = 20;

        app.apply_action(Action::ScrollDown(50));
        let view = app.focused_view().expect("view");
        assert_eq!(*app.scroll.get(&view).unwrap_or(&0), 0);

        app.apply_action(Action::ScrollUp(5));
        assert_eq!(*app.scroll.get(&view).unwrap_or(&0), 5);
    }

    #[test]
    fn scrolling_up_past_the_top_does_not_strand_the_transcript() {
        let mut app = app();
        // 200 wrapped rows in a 20-row pane: 180 is as far up as it goes.
        app.rendered_lines = 200;
        app.viewport_height = 20;
        let view = app.focused_view().expect("view");

        app.apply_action(Action::ScrollUp(1_000));
        assert_eq!(
            *app.scroll.get(&view).expect("scroll"),
            180,
            "scroll must clamp at the oldest line, not keep counting"
        );

        // One page down from the top must actually move, rather than unwinding an
        // invisible counter. This is the bug where the newest messages appeared lost.
        app.apply_action(Action::ScrollDown(20));
        assert_eq!(*app.scroll.get(&view).expect("scroll"), 160);
    }

    #[test]
    fn jumping_to_the_top_is_reversible() {
        let mut app = app();
        app.rendered_lines = 200;
        app.viewport_height = 20;
        let view = app.focused_view().expect("view");

        // ScrollTop passes u16::MAX; unclamped it would take 65,000 keypresses to
        // scroll back to the newest message.
        app.apply_action(Action::ScrollTop);
        assert_eq!(*app.scroll.get(&view).expect("scroll"), 180);

        app.apply_action(Action::ScrollBottom);
        assert_eq!(*app.scroll.get(&view).expect("scroll"), 0);
    }

    #[test]
    fn the_room_list_arriving_opens_the_focused_view() {
        // At startup the app opens its focused view before any room exists, so the
        // call is a no-op. If nothing retries, the transcript stays empty until the
        // user happens to click a pane.
        let mut app = App::new(Config::default());
        app.open_focused_view();
        assert!(app.take_commands().is_empty(), "nothing to focus yet");

        app.apply_worker_event(WorkerEvent::Rooms(vec![RoomSummary {
            room_id: "!r:x".into(),
            display_name: "Commons".into(),
            is_space: false,
            parents: Vec::new(),
            is_direct: false,
            is_encrypted: false,
            notification_count: 0,
            highlight_count: 0,
        }]));

        let commands = app.take_commands();
        assert!(
            commands
                .iter()
                .any(|c| matches!(c, Command::OpenView(v) if v == &View::room("!r:x"))),
            "the first room list must open the focused view: {commands:?}"
        );
    }

    #[test]
    fn the_help_overlay_toggles_and_escape_closes_it() {
        let mut app = app();
        assert!(!app.help);
        app.apply_action(Action::ToggleHelp);
        assert!(app.help);
        app.apply_action(Action::ToggleHelp);
        assert!(!app.help);

        app.apply_action(Action::ToggleHelp);
        app.apply_action(Action::CloseHelp);
        assert!(!app.help, "escape must close the overlay");
    }

    #[test]
    fn toggling_a_card_records_an_override() {
        let mut app = app();
        let mut result = event(Kind::ToolResult);
        result.tool = Some(Tool {
            name: "edit".into(),
            index: 0,
            args: None,
            preview: None,
            status: ToolStatus::Ok,
            duration_ms: None,
            mime: Some("text/plain".into()),
            body: Some("body".into()),
            truncated: false,
        });
        app.apply_worker_event(WorkerEvent::Timeline {
            view: View::room("!r:x"),
            entries: vec![agent_entry("$e1", result)],
        });

        app.apply_action(Action::ToggleCard);
        assert_eq!(
            app.overrides.get(&("$e1".to_owned(), 0)),
            Some(&true),
            "a finished card is collapsed, so toggling opens it"
        );
    }
}
