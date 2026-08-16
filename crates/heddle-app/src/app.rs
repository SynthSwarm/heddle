//! Application state and the update function.
//!
//! Deliberately separated from terminal I/O so the interesting transitions — focus
//! movement, composer editing, approval resolution — are testable without a terminal.

use crate::composer::Composer;
use crate::config::Config;
use crate::keymap::{Action, Mode, Prefix};
use crate::palette::Palette;
use heddle_agent::{AgentState, AgentStore};
use heddle_layout::{
    Dir, Layout, Pane, PaneId, PaneKind, SplitHandle, Tab, Tiling, Unread, Workspaces,
    ORPHAN_WORKSPACE,
};
use heddle_matrix::{
    Command, Entry, EntryKind, MemberSummary, RecoveryState, RoomSummary, Shield, SyncState,
    ThreadSummary, Verification, View, WorkerEvent,
};
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

/// What the composer is about to do, when it is not sending a new message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// Submit will reply to this event.
    Reply(String),
    /// Submit will replace this event's content.
    Edit(String),
}

/// How long after the last keystroke heddle declares the user has stopped typing.
///
/// Under the four seconds the server keeps a notice alive, so the room sees an explicit
/// stop rather than watching it lapse. For an agent room that difference matters: the
/// stop is the cue that a question is finished being asked.
const TYPING_IDLE_MS: u64 = 3_000;

/// How often an ongoing notice is re-asserted while typing continues. Matches the SDK's
/// own resend window, which suppresses anything sent more often than this anyway.
const TYPING_REFRESH_MS: u64 = 3_000;

/// The local user's outbound typing state for one room.
#[derive(Debug, Clone)]
struct Typing {
    room_id: String,
    /// Set by the keypress, consumed by the tick. Keeping the clock out of the keypress
    /// path is what makes this testable without a fake clock in the composer.
    dirty: bool,
    /// Whether the room currently believes the user is typing.
    active: bool,
    last_input_ms: u64,
    last_sent_ms: u64,
}

/// The open thread picker.
#[derive(Debug, Clone, Default)]
pub struct ThreadPicker {
    pub room_id: String,
    pub threads: Vec<ThreadSummary>,
    pub selected: usize,
    /// True until the worker answers, so the overlay can say so.
    pub loading: bool,
}

impl ThreadPicker {
    pub fn selected(&self) -> Option<&ThreadSummary> {
        self.threads.get(self.selected)
    }
}

/// The open mention picker.
///
/// Unlike every other overlay this one is not modal: the composer keeps taking keys
/// underneath it, and the query is recomputed from the buffer after each edit rather
/// than accumulated here. Only movement and acceptance are intercepted.
#[derive(Debug, Clone, Default)]
pub struct MentionPicker {
    /// Byte offset of the `@` in the composer, so a completion knows what to replace.
    pub start: usize,
    /// Indices into the room's member list, best match first.
    pub matches: Vec<usize>,
    pub selected: usize,
}

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

/// The overlay that is open, if any.
///
/// One field rather than six, because only one can be open and saying so in prose did
/// not work. The comment on the old dispatch chain read "it is checked before the
/// thread picker only because the two can never be open at once" -- and they could:
/// the thread picker's match had a `_ => {}` arm, so `:` fell through it into the main
/// keymap, opened the palette, and left two overlays on screen at once. The palette was
/// checked first, so the picker underneath became unreachable and its own `Esc` was
/// eaten. The key overlay was worse: it was drawn but appeared in no chain at all, so
/// `D` still armed a redaction behind it.
///
/// With one field there is no ordering to get wrong and nothing to fall through. Note
/// [`MentionPicker`] is deliberately *not* here: it is a filter on the composer rather
/// than a modal, and the composer keeps taking keys underneath it.
#[derive(Debug, Clone)]
pub enum Modal {
    /// Interactive device verification.
    ///
    /// The emoji on screen are a security decision with a human at the other end, so
    /// nothing may act underneath it -- splitting a pane or sending a message while the
    /// user believes they are answering a yes/no question. Being a variant rather than
    /// a field is what guarantees that now; it used to be first in a hand-ordered chain
    /// whose draw order in `ui::draw` disagreed with it, so the recovery panel painted
    /// on top of the verification prompt while verification consumed the keys.
    Verification(Verification),
    /// The recovery panel. Holds a secret mid-typing.
    Recovery(RecoveryPanel),
    /// The emoji picker. A search box, so it owns every key.
    Emoji(crate::emoji::Picker),
    /// The command palette. A search box too.
    Palette(Palette),
    /// The thread picker.
    Threads(ThreadPicker),
    /// The `<prefix> ?` key overlay.
    Help,
}

/// Everything the UI draws from.
/// The recovery panel.
///
/// A state machine rather than one struct with optional fields, because the four things
/// it does are genuinely different questions: type a secret in, decide whether to create
/// one, decide whether to destroy one, and write one down. Flattening them would let the
/// UI show a text field where the answer is yes or no.
///
/// The key input is the panel's own, not the composer's. A recovery key typed into the
/// composer is one stray `enter` from being sent to a room, and unlike a mistyped
/// message it cannot be unsent in any meaningful sense once a homeserver has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryPanel {
    /// Asking for the existing recovery key.
    AskKey {
        key: String,
        /// Set once the key is with the worker, so the panel says it is working rather
        /// than appearing to have ignored the keypress.
        submitted: bool,
        /// Why the last attempt failed, if one did.
        error: Option<String>,
    },
    /// Offering to set recovery up for an account that has none.
    OfferEnable,
    /// Confirming the replacement of an existing recovery key.
    ConfirmReset,
    /// Waiting on the server.
    Busy(&'static str),
    /// Showing a newly created key. The one chance the user gets to keep it.
    ShowKey { key: String },
}

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
    /// One composer per view, so a half-written message survives switching room and
    /// coming back. Created on demand.
    composers: HashMap<View, Composer>,
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
    /// The overlay that is open, if any. See [`Modal`].
    pub modal: Option<Modal>,
    /// Selected message per view, by event id. Reply, edit and redact all act on it.
    pub selected: HashMap<View, String>,
    /// What the composer will do on submit, when it is not simply sending.
    pub composing: Option<Pending>,
    /// Joined members per room, for the mention picker.
    ///
    /// Asked for once per room, when it is first focused, so that typing `@` shows a
    /// list rather than a round trip. A room nobody has looked at costs nothing.
    pub members: HashMap<String, Vec<MemberSummary>>,
    /// Rooms already asked about, so a room with no members is not asked about forever.
    members_asked: HashSet<String>,
    /// The open mention picker, if any.
    pub mentions: Option<MentionPicker>,
    /// The split border the mouse currently has hold of, and the room it belongs to.
    ///
    /// The room is part of it because a drag is only meaningful in the tab it started
    /// in, and a tab can be switched from under it by an arriving keypress.
    dragging: Option<(String, SplitHandle)>,
    /// Whether this account's secrets are recoverable on a new device.
    pub recovery: RecoveryState,
    /// Rooms whose loaded history contains a message heddle cannot vouch for.
    pub unverified_rooms: HashSet<String>,
    /// Whether this device has been verified by the account's cross-signing identity.
    ///
    /// `None` until the crypto store can answer. Drawing "unverified" before we know
    /// would put a warning in front of a user who has done nothing wrong.
    pub device_verified: Option<bool>,
    /// What the local user is currently telling the room about their typing.
    typing: Option<Typing>,
    /// Event armed for redaction, awaiting a confirming second keypress.
    confirm_redact: Option<String>,
    /// Row of each event in the focused transcript, recorded by the renderer.
    pub anchors: Vec<heddle_render::transcript::Anchor>,
    /// Set when the terminal must be fully repainted rather than diffed.
    ///
    /// ratatui only rewrites cells it believes have changed. A glyph that paints wider
    /// than it was measured desynchronises that belief from the real screen, and the
    /// stale cells are never repainted because ratatui thinks they are already right.
    /// Switching view is the common case: the previous room's text stays on screen
    /// under the new one.
    pub needs_redraw: bool,

    pub should_quit: bool,

    /// The saved layout, consumed room by room as rooms arrive.
    ///
    /// Not the layout to write back: that is captured fresh from the live state. This
    /// is only ever drained.
    saved_layout: Layout,
    /// Whether the saved focus is still waiting to be applied.
    ///
    /// Rooms arrive over several sync responses, so the workspace the user was last in
    /// may not exist when the first batch lands. Cleared the moment the user moves
    /// focus themselves, because restoring a position someone has already left is
    /// indistinguishable from the cursor jumping about on its own.
    restoring_focus: bool,
    /// Set when the arrangement has changed and has not yet been written out.
    pub layout_dirty: bool,

    /// Commands produced by the last update, drained by the caller.
    pending: Vec<Command>,
}

impl App {
    /// `saved_layout` is drained as rooms arrive; pass [`Layout::default`] to start
    /// with one pane per room.
    pub fn new(config: Config, saved_layout: Layout) -> Self {
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
            composers: HashMap::new(),
            scroll: HashMap::new(),
            sync: SyncState::Idle,
            status: None,
            paginating: HashSet::new(),
            rendered_lines: 0,
            viewport_height: 0,
            bars: BarHits::default(),
            modal: None,
            selected: HashMap::new(),
            composing: None,
            members: HashMap::new(),
            members_asked: HashSet::new(),
            mentions: None,
            dragging: None,
            device_verified: None,
            recovery: RecoveryState::Unknown,
            unverified_rooms: HashSet::new(),
            typing: None,
            confirm_redact: None,
            anchors: Vec::new(),
            needs_redraw: false,
            should_quit: false,
            restoring_focus: saved_layout.workspace.is_some() || !saved_layout.tabs.is_empty(),
            saved_layout,
            layout_dirty: false,
            pending: Vec::new(),
        }
    }

    /// The current arrangement, ready to be written to disk.
    pub fn layout(&self) -> Layout {
        Layout::capture(&self.workspaces, &self.tilings)
    }

    /// Record that the arrangement has changed and should be saved.
    fn touch_layout(&mut self) {
        self.layout_dirty = true;
    }

    /// Open an overlay, replacing whatever was open.
    ///
    /// A keypress cannot reach this while something is already open -- the overlay
    /// swallows it -- so in practice the replacement path is the worker's: a
    /// verification request arriving from another device takes over whatever was on
    /// screen, which is what it should do.
    pub fn open_modal(&mut self, modal: Modal) {
        self.modal = Some(modal);
    }

    /// Close whatever overlay is open.
    pub fn close_modal(&mut self) {
        self.modal = None;
    }

    pub fn verification(&self) -> Option<&Verification> {
        match &self.modal {
            Some(Modal::Verification(v)) => Some(v),
            _ => None,
        }
    }

    pub fn recovery_prompt(&self) -> Option<&RecoveryPanel> {
        match &self.modal {
            Some(Modal::Recovery(r)) => Some(r),
            _ => None,
        }
    }

    pub fn emoji(&self) -> Option<&crate::emoji::Picker> {
        match &self.modal {
            Some(Modal::Emoji(e)) => Some(e),
            _ => None,
        }
    }

    pub fn palette(&self) -> Option<&Palette> {
        match &self.modal {
            Some(Modal::Palette(p)) => Some(p),
            _ => None,
        }
    }

    pub fn threads(&self) -> Option<&ThreadPicker> {
        match &self.modal {
            Some(Modal::Threads(t)) => Some(t),
            _ => None,
        }
    }

    pub fn threads_mut(&mut self) -> Option<&mut ThreadPicker> {
        match &mut self.modal {
            Some(Modal::Threads(t)) => Some(t),
            _ => None,
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

    /// The focused view's composer, for drawing. Empty when nothing is focused.
    pub fn composer(&self) -> Option<&Composer> {
        self.composers.get(&self.focused_view()?)
    }

    /// The focused view's composer, creating it on first keystroke.
    fn composer_mut(&mut self) -> Option<&mut Composer> {
        let view = self.focused_view()?;
        Some(self.composers.entry(view).or_default())
    }

    /// Move the caret up, falling back to recalling an older sent message.
    ///
    /// Matches every chat client: up is "the line above" when there is one and "the
    /// thing I said before" when there is not.
    fn composer_up(&mut self) {
        if let Some(composer) = self.composer_mut() {
            if !composer.up() {
                composer.history_prev();
            }
        }
    }

    fn composer_down(&mut self) {
        if let Some(composer) = self.composer_mut() {
            if !composer.down() {
                composer.history_next();
            }
        }
    }

    /// Event ids of the selectable messages in the focused view, oldest first.
    ///
    /// Messages only. Notices carry an event id and render a row, so they used to be
    /// selectable, which put the marker on membership changes and redactions -- and
    /// since those cluster at the end of a transcript, walking down appeared to run past
    /// the last message onto rows nothing can be done with. Every action reachable from
    /// the selection (reply, edit, redact, open thread) needs a message anyway.
    fn selectable(&self) -> Vec<String> {
        self.focused_view()
            .and_then(|v| self.timelines.get(&v))
            .map(|entries| {
                entries
                    .iter()
                    .filter(|e| matches!(&e.kind, heddle_matrix::EntryKind::Message(_)))
                    .filter_map(|e| e.event_id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The selected event in the focused view.
    pub fn selected_event(&self) -> Option<&str> {
        let view = self.focused_view()?;
        self.selected.get(&view).map(String::as_str)
    }

    /// Move the selection. `delta` is in messages; negative is towards older.
    ///
    /// With nothing selected, the first move selects the newest message, which is what
    /// someone pressing "up" in a chat client means.
    fn select_by(&mut self, delta: i32) {
        let ids = self.selectable();
        if ids.is_empty() {
            // Nothing to select yet — an empty or still-loading room. Scroll instead of
            // swallowing the keypress, so the transcript never feels dead. Older is up,
            // which is the same negative delta `scroll_by` already means.
            self.scroll_by(delta);
            return;
        }
        let Some(view) = self.focused_view() else {
            return;
        };

        let current = self
            .selected
            .get(&view)
            .and_then(|id| ids.iter().position(|c| c == id));

        let next = match current {
            None => ids.len() - 1,
            Some(i) => (i as i32 + delta).clamp(0, ids.len() as i32 - 1) as usize,
        };
        self.selected.insert(view.clone(), ids[next].clone());
        self.scroll_to_selection();

        // At the ends, "keep the selection on screen" is not enough. The newest message
        // is usually followed by notices, so stopping the moment it is merely visible
        // leaves those below it and the pane looks stuck short of the bottom; and the
        // oldest loaded message is the point at which more history is wanted, exactly as
        // it is when scrolling there by hand.
        if next + 1 == ids.len() {
            self.scroll.insert(view.clone(), 0);
        } else if next == 0 {
            let scroll = self.max_scroll();
            self.scroll.insert(view.clone(), scroll);
            self.paginate_if_near_top(&view, scroll);
        }
    }

    /// Scroll so the selected message is on screen.
    ///
    /// Uses the anchors the renderer recorded last frame; only it knows which row an
    /// event landed on.
    fn scroll_to_selection(&mut self) {
        let Some(view) = self.focused_view() else {
            return;
        };
        let Some(id) = self.selected.get(&view) else {
            return;
        };
        let Some(row) = self
            .anchors
            .iter()
            .find(|a| &a.event_id == id)
            .map(|a| a.row)
        else {
            return;
        };

        let height = self.viewport_height.max(1);
        let max = self.max_scroll();
        let offset = max.saturating_sub(*self.scroll.get(&view).unwrap_or(&0));

        // `scroll` counts from the bottom while anchors count from the top, so the
        // conversion goes through `max`.
        let wanted = if row < offset {
            max.saturating_sub(row)
        } else if row >= offset + height {
            max.saturating_sub(row.saturating_sub(height - 1))
        } else {
            return;
        };
        self.scroll.insert(view, wanted.min(max));
    }

    /// Body of the selected message, for pre-filling the composer on edit.
    fn selected_body(&self) -> Option<(String, bool)> {
        let view = self.focused_view()?;
        let id = self.selected.get(&view)?;
        let entry = self
            .timelines
            .get(&view)?
            .iter()
            .find(|e| e.event_id.as_deref() == Some(id.as_str()))?;
        match &entry.kind {
            heddle_matrix::EntryKind::Message(m) => Some((m.body.clone(), m.is_own)),
            _ => None,
        }
    }

    /// Begin a reply to the selected message.
    fn begin_reply(&mut self) {
        let Some(id) = self.selected_event().map(ToOwned::to_owned) else {
            self.status = Some("select a message first".into());
            return;
        };
        self.composing = Some(Pending::Reply(id));
        self.mode = Mode::Insert;
    }

    /// Begin editing the selected message, loading it into the composer.
    fn begin_edit(&mut self) {
        let Some(id) = self.selected_event().map(ToOwned::to_owned) else {
            self.status = Some("select a message first".into());
            return;
        };
        let Some((body, is_own)) = self.selected_body() else {
            self.status = Some("that is not an editable message".into());
            return;
        };
        // The server would reject it anyway; saying so now is cheaper and clearer.
        if !is_own {
            self.status = Some("you can only edit your own messages".into());
            return;
        }

        if let Some(composer) = self.composer_mut() {
            composer.set_text(&body);
        }
        self.composing = Some(Pending::Edit(id));
        self.mode = Mode::Insert;
    }

    /// Redact the selected message. The first press arms, the second confirms.
    fn redact_selected(&mut self) {
        let Some(id) = self.selected_event().map(ToOwned::to_owned) else {
            self.status = Some("select a message first".into());
            return;
        };
        let Some(view) = self.focused_view() else {
            return;
        };

        if self.confirm_redact.as_deref() == Some(id.as_str()) {
            self.confirm_redact = None;
            self.queue(Command::Redact { view, event_id: id });
            self.status = Some("deleted".into());
        } else {
            // Redaction cannot be undone, so it does not get to be one keypress.
            self.confirm_redact = Some(id);
            self.status = Some("press D again to delete".into());
        }
    }

    /// Abandon a reply, edit or arming redaction.
    fn cancel_pending(&mut self) {
        if self.composing.take().is_some() {
            if let Some(composer) = self.composer_mut() {
                composer.set_text("");
            }
        }
        self.confirm_redact = None;
    }

    /// The thread reachable from the selected message, as (root, title).
    ///
    /// A message either roots a thread or sits inside one; both are a way in.
    fn selected_thread(&self) -> Option<(String, String)> {
        let view = self.focused_view()?;
        let id = self.selected.get(&view)?;
        let entry = self
            .timelines
            .get(&view)?
            .iter()
            .find(|e| e.event_id.as_deref() == Some(id.as_str()))?;
        let heddle_matrix::EntryKind::Message(message) = &entry.kind else {
            return None;
        };
        let title = message
            .body
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned();

        if message.thread_replies.is_some() {
            Some((id.clone(), title))
        } else {
            message.thread_root.clone().map(|root| (root, title))
        }
    }

    /// Start a thread on the selected message.
    ///
    /// A thread has no existence of its own in Matrix: it is a root event plus whatever
    /// relates to it, so "starting" one is opening a pane rooted at a message that has no
    /// replies yet. Nothing is sent here. The first message typed into the pane goes
    /// through the thread-focused timeline, which attaches the `m.thread` relation and
    /// brings the thread into being -- so an abandoned pane leaves nothing behind.
    ///
    /// `enter` is the other half of this: it opens a thread that already exists. Starting
    /// one deliberately needs a different key, or every stray `enter` on a message would
    /// invite a thread nobody wanted.
    fn start_thread(&mut self) {
        let Some(view) = self.focused_view() else {
            return;
        };
        // Matrix has no thread of a thread; a reply inside one stays in the same thread.
        if view.thread_root.is_some() {
            self.status = Some("this pane is already a thread".into());
            return;
        }
        let Some(root) = self.selected_event().map(ToOwned::to_owned) else {
            self.status = Some("select a message to start a thread on".into());
            return;
        };

        let title = self
            .selected_body()
            .map(|(body, _)| body.lines().next().unwrap_or_default().trim().to_owned())
            .unwrap_or_default();

        self.open_thread_pane(root, title);
    }

    /// Open the thread on the selected message.
    fn accept_selection(&mut self) {
        let Some((root, title)) = self.selected_thread() else {
            self.status = Some("that message has no thread".into());
            return;
        };
        self.open_thread_pane(root, title);
    }

    /// Open a thread as a pane, or focus it if it is already open.
    fn open_thread_pane(&mut self, root: String, title: String) {
        let Some(room_id) = self
            .workspaces
            .focused()
            .and_then(|w| w.focused_tab())
            .map(|t| t.room_id.clone())
        else {
            return;
        };

        // Already open: focus it rather than growing a second pane onto the same thread.
        if let Some(existing) = self
            .workspaces
            .focused()
            .and_then(|w| w.focused_tab())
            .and_then(|t| t.pane_for_thread(&root))
            .map(|p| p.id)
        {
            self.focus_pane_id(existing);
            return;
        }

        // Geometry. The first thread splits the room pane vertically, so transcript and
        // thread sit side by side. Later threads stack under the newest thread instead,
        // because splitting the room pane again would squeeze the transcript towards
        // nothing while the threads stayed wide.
        let (anchor, dir) = match self.newest_thread_pane() {
            Some(id) => (Some(id), Dir::Down),
            None => (self.room_pane(), Dir::Right),
        };
        if let Some(id) = anchor {
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
        }

        let Some(new_id) = self.focused_tiling_mut().and_then(|t| t.split(dir)) else {
            return;
        };
        if let Some(tab) = self
            .workspaces
            .focused_mut()
            .and_then(Workspace_focused_tab_mut)
        {
            tab.push_pane(Pane::new(new_id, PaneKind::Thread { room_id, root }, title));
        }
        self.focus_moved();
    }

    /// The pane showing the room's main timeline, if it is still open.
    fn room_pane(&self) -> Option<PaneId> {
        self.workspaces
            .focused()?
            .focused_tab()?
            .panes
            .iter()
            .find(|p| p.kind.thread_root().is_none())
            .map(|p| p.id)
    }

    /// The most recently opened thread pane.
    fn newest_thread_pane(&self) -> Option<PaneId> {
        self.workspaces
            .focused()?
            .focused_tab()?
            .panes
            .iter()
            .rev()
            .find(|p| p.kind.thread_root().is_some())
            .map(|p| p.id)
    }

    // ------------------------------------------------------------------- threads

    // ---------------------------------------------------------------- typing notices

    /// Record that the composer was edited.
    ///
    /// Deliberately does no clock work and queues nothing: a notice per keystroke would
    /// be one request per character. The tick decides what the room needs to hear.
    fn note_typing(&mut self) {
        let Some(room_id) = self.focused_view().map(|v| v.room_id) else {
            return;
        };

        let fresh = Typing {
            room_id: room_id.clone(),
            dirty: true,
            active: false,
            last_input_ms: 0,
            last_sent_ms: 0,
        };

        match &mut self.typing {
            Some(typing) if typing.room_id == room_id => typing.dirty = true,
            // Typing in a different room from the one last announced: tell the old room
            // we stopped first, or it shows an indicator until the notice lapses.
            Some(_) => {
                self.stop_typing();
                self.typing = Some(fresh);
            }
            None => self.typing = Some(fresh),
        }
    }

    /// Drive outbound typing notices. Called from the event loop's tick.
    pub fn tick_typing(&mut self, now_ms: u64) {
        let Some(typing) = &mut self.typing else {
            return;
        };

        if typing.dirty {
            typing.dirty = false;
            typing.last_input_ms = now_ms;
            // Re-assert on a timer rather than per keystroke: the notice has a lifetime,
            // so it needs refreshing while a long message is written.
            let stale = now_ms.saturating_sub(typing.last_sent_ms) >= TYPING_REFRESH_MS;
            if !typing.active || stale {
                typing.active = true;
                typing.last_sent_ms = now_ms;
                let room_id = typing.room_id.clone();
                self.queue(Command::SendTyping {
                    room_id,
                    typing: true,
                });
            }
            return;
        }

        if typing.active && now_ms.saturating_sub(typing.last_input_ms) >= TYPING_IDLE_MS {
            self.stop_typing();
        }
    }

    /// Tell the room the user has stopped, if it currently believes otherwise.
    fn stop_typing(&mut self) {
        let Some(typing) = &self.typing else {
            return;
        };
        let was_active = typing.active;
        let room_id = typing.room_id.clone();
        self.typing = None;
        if was_active {
            self.queue(Command::SendTyping {
                room_id,
                typing: false,
            });
        }
    }

    /// Open the emoji picker to put an emoji in the composer.
    fn open_emoji_for_composer(&mut self) {
        self.open_modal(Modal::Emoji(crate::emoji::Picker::new(
            crate::emoji::Target::Composer,
        )));
        // The picker is a search box, so typing has to reach it. Insert mode is what
        // turns a keypress into `Insert(c)` rather than a normal-mode command.
        self.mode = Mode::Insert;
    }

    /// Open the emoji picker to react to the selected message.
    fn open_emoji_for_reaction(&mut self) {
        let Some(event_id) = self.selected_event().map(ToOwned::to_owned) else {
            self.status = Some("select a message to react to".into());
            return;
        };
        self.open_modal(Modal::Emoji(crate::emoji::Picker::new(
            crate::emoji::Target::Reaction { event_id },
        )));
        self.mode = Mode::Insert;
    }

    /// Route a key to the open emoji picker.
    fn emoji_action(&mut self, action: Action) {
        let Some(Modal::Emoji(picker)) = &mut self.modal else {
            return;
        };

        match action {
            Action::Cancel => self.close_emoji(),
            Action::Insert(c) => picker.push(c),
            Action::Backspace => picker.pop(),
            // Both pairs move the highlight: arrows are what the composer's own bindings
            // send here, j/k are what someone arriving from the transcript will press.
            Action::CaretUp | Action::ScrollUp(_) | Action::SelectOlder => picker.up(),
            Action::CaretDown | Action::ScrollDown(_) | Action::SelectNewer => picker.down(),
            Action::Submit | Action::Accept => self.accept_emoji(),
            // Anything else is swallowed rather than acted on: a stray binding firing
            // underneath an overlay is how a picker ends up splitting a pane.
            _ => {}
        }
    }

    /// Apply the highlighted emoji and close the picker.
    fn accept_emoji(&mut self) {
        let Some(Modal::Emoji(picker)) = &self.modal else {
            return;
        };
        let Some(chosen) = picker.chosen() else {
            self.status = Some("no emoji matches that".into());
            return;
        };
        let target = picker.target.clone();

        match target {
            crate::emoji::Target::Composer => {
                if let Some(composer) = self.composer_mut() {
                    for c in chosen.chars() {
                        composer.insert(c);
                    }
                }
                self.close_modal();
                // Straight back to typing: choosing an emoji is part of writing the
                // message, not a detour out of it.
                self.mode = Mode::Insert;
                return;
            }
            crate::emoji::Target::Reaction { event_id } => {
                if let Some(view) = self.focused_view() {
                    self.queue(Command::ToggleReaction {
                        view,
                        event_id,
                        key: chosen.to_owned(),
                    });
                }
            }
        }
        self.close_emoji();
    }

    fn close_emoji(&mut self) {
        self.close_modal();
        self.mode = Mode::Normal;
    }

    /// Open the thread picker for the focused room and ask the worker to fill it.
    fn open_thread_picker(&mut self) {
        let Some(room_id) = self
            .workspaces
            .focused()
            .and_then(|w| w.focused_tab())
            .map(|t| t.room_id.clone())
        else {
            return;
        };
        self.open_modal(Modal::Threads(ThreadPicker {
            room_id: room_id.clone(),
            threads: Vec::new(),
            selected: 0,
            loading: true,
        }));
        self.queue(Command::ListThreads { room_id });
    }

    /// Open the highlighted thread as a pane beside the current one.
    fn open_selected_thread(&mut self) {
        let Some(picker) = self.threads() else {
            return;
        };
        let Some(thread) = picker.selected() else {
            self.status = Some("no thread selected".into());
            return;
        };
        let root = thread.root_event_id.clone();
        let title = if thread.preview.is_empty() {
            thread.sender_display.clone()
        } else {
            thread.preview.clone()
        };
        self.close_modal();
        self.open_thread_pane(root, title);
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
                // A room-level shield, drawn from evidence rather than from a roster.
                // Auditing every member's devices would mean a device list per member
                // per room; what the user needs to know is that this room contains
                // messages heddle cannot vouch for, and the messages themselves say so.
                let suspect = entries.iter().any(|e| {
                    matches!(&e.kind, EntryKind::Message(m) if matches!(m.shield, Shield::Warning(_)))
                });
                if suspect {
                    self.unverified_rooms.insert(view.room_id.clone());
                } else {
                    self.unverified_rooms.remove(&view.room_id);
                }
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

            WorkerEvent::Threads { room_id, threads } => {
                // Ignore a late answer for a picker the user has already closed or
                // reopened elsewhere.
                if let Some(picker) = self.threads_mut() {
                    if picker.room_id == room_id {
                        picker.threads = threads;
                        picker.selected = 0;
                        picker.loading = false;
                    }
                }
            }

            WorkerEvent::Members { room_id, members } => {
                // Cached rather than handed to an open picker: the roster belongs to the
                // room, not to one moment of typing, and the picker is opened and closed
                // once per mention.
                self.members.insert(room_id, members);
                self.refilter_mentions();
            }

            WorkerEvent::Warning(text) => self.status = Some(text),

            WorkerEvent::Verification(state) => {
                // A finished flow reports itself in the status line and gets out of the
                // way. Leaving a "verified" panel on screen would mean the user has to
                // dismiss a dialog to acknowledge good news.
                match &state {
                    Verification::Done => {
                        self.status = Some("device verified".into());
                        self.device_verified = Some(true);
                        self.close_modal();
                        self.needs_redraw = true;
                    }
                    Verification::Cancelled { reason } => {
                        self.status = Some(format!("verification cancelled: {reason}"));
                        self.close_modal();
                        self.needs_redraw = true;
                    }
                    _ => self.open_modal(Modal::Verification(state)),
                }
            }

            WorkerEvent::Recovery(state) => {
                self.recovery = state;
                // The prompt has no other way to learn it succeeded. Left to itself it
                // sits on "unlocking…" for ever, over a screen full of messages that
                // plainly did decrypt -- which tells the user the client has hung at the
                // exact moment it actually worked.
                let waiting = matches!(
                    self.modal,
                    Some(Modal::Recovery(RecoveryPanel::AskKey {
                        submitted: true,
                        ..
                    }))
                );
                if state == RecoveryState::Enabled && waiting {
                    self.close_recovery();
                    self.status = Some("recovery unlocked; older messages will decrypt".into());
                    self.needs_redraw = true;
                }
            }

            WorkerEvent::RecoveryFailed(reason) => {
                // Kept open, emptied, and told why: a mistyped recovery key is worth a
                // second attempt, and closing the prompt would make the user find the
                // key again from the start.
                self.open_modal(Modal::Recovery(RecoveryPanel::AskKey {
                    key: String::new(),
                    submitted: false,
                    error: Some(reason.clone()),
                }));
                self.status = Some(format!("recovery failed: {reason}"));
            }

            WorkerEvent::RecoveryKeyCreated(key) => {
                // Straight to the panel, and nowhere else. It is never logged and never
                // put in the status line: the server keeps no copy, so this is the only
                // time it can be read, and it must not end up somewhere it outlives the
                // moment.
                self.open_modal(Modal::Recovery(RecoveryPanel::ShowKey { key }));
                self.needs_redraw = true;
            }

            WorkerEvent::DeviceVerified(verified) => {
                // `None` means the crypto layer has not decided yet; keeping the last
                // known answer avoids flickering the shield off and on during startup.
                if verified.is_some() {
                    self.device_verified = verified;
                }
            }

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
        let space_titles: HashMap<&str, &str> = spaces
            .iter()
            .map(|s| (s.room_id.as_str(), s.display_name.as_str()))
            .collect();

        for room in rooms.iter().filter(|r| !r.is_space) {
            let workspace_id = room
                .parents
                .first()
                .cloned()
                .unwrap_or_else(|| ORPHAN_WORKSPACE.to_owned());
            // The orphan workspace is titled with its own marker rather than a word, so
            // it reads as "the rooms with no Space" instead of a section heading.
            // SPEC.md §2.
            //
            // A Space workspace is titled after the Space. Falling back to the room's own
            // name would title the workspace after whichever of its rooms happened to be
            // processed first, which only shows up once rooms actually have parents.
            let title = if workspace_id == ORPHAN_WORKSPACE {
                ORPHAN_WORKSPACE
            } else {
                space_titles
                    .get(workspace_id.as_str())
                    .copied()
                    .unwrap_or(room.display_name.as_str())
            };

            let workspace = self.workspaces.entry(&workspace_id, title);
            match workspace.tab_for_room_mut(&room.room_id) {
                Some(tab) => {
                    tab.title.clone_from(&room.display_name);
                    tab.is_encrypted = room.is_encrypted;
                    tab.unread = Unread::new(room.notification_count, room.highlight_count);
                }
                None => {
                    let mut tab = Tab::new(&room.room_id, &room.display_name);
                    tab.is_encrypted = room.is_encrypted;
                    tab.unread = Unread::new(room.notification_count, room.highlight_count);

                    // A room the user had arranged comes back arranged. Anything else --
                    // no saved entry, an unreadable one, or one that disagrees with
                    // itself -- falls through to a single pane on the main timeline,
                    // which is the arrangement every room starts life with anyway.
                    let tiling = match self.saved_layout.take_room(&room.room_id, &mut tab) {
                        Some(tiling) => tiling,
                        None => {
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
                            tiling
                        }
                    };
                    self.tilings.insert(room.room_id.clone(), tiling);
                    workspace.tabs.push(tab);
                    self.layout_dirty = true;
                }
            }
        }

        self.restore_focus();
    }

    /// Put the user back where they left off, once the rooms to do it with exist.
    ///
    /// Attempted after every room batch rather than once at startup, because sync
    /// delivers rooms over several responses and the workspace someone was last in is
    /// rarely in the first one. It stops at the first user-driven focus change: a
    /// client that yanks the view away half a second after launch, because a late room
    /// finally arrived, is worse than one that simply starts where it starts.
    fn restore_focus(&mut self) {
        if !self.restoring_focus {
            return;
        }

        // Per-workspace tabs first, so that focusing the saved workspace lands on the
        // saved room in one step rather than showing its first tab and then switching.
        for (workspace_id, room_id) in &self.saved_layout.tabs {
            if let Some(workspace) = self
                .workspaces
                .items
                .iter_mut()
                .find(|w| &w.id == workspace_id)
            {
                if let Some(index) = workspace.tabs.iter().position(|t| &t.room_id == room_id) {
                    workspace.focus_tab(index);
                }
            }
        }

        let Some(wanted) = self.saved_layout.workspace.clone() else {
            // Nothing more to wait for; the tabs above are applied on every batch and
            // are harmless to reapply.
            return;
        };
        let Some(index) = self.workspaces.items.iter().position(|w| w.id == wanted) else {
            return;
        };

        self.workspaces.focus(index);
        self.restoring_focus = false;
        self.needs_redraw = true;
        // No `open_focused_view` here: the caller does it after every room batch, and
        // doing it twice queues the same view at the worker twice.
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
                AgentPayload::Structured { event, .. } => self.agents.apply(event),
                AgentPayload::Degraded { .. } => {
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

    /// Route a key to the verification overlay.
    ///
    /// Only the keys that mean something are honoured, and everything else is swallowed.
    /// The answer to "do these emoji match" is yes, no, or not now; a client that let
    /// any other key through would be a client where a mistyped `j` dismissed a security
    /// prompt.
    fn verification_action(&mut self, action: Action) {
        let Some(Modal::Verification(state)) = &self.modal else {
            return;
        };

        match (state, action) {
            // Accepting a request is not yet a judgement about keys: it only agrees to
            // start comparing them.
            (Verification::Requested { .. }, Action::Approve | Action::Accept) => {
                self.queue(Command::AcceptVerification);
            }

            // This is the judgement. `Deny` reports a mismatch rather than a withdrawal,
            // because a user pressing "they do not match" is reporting an attack, and the
            // other side needs to hear that rather than a shrug.
            (Verification::Compare { .. }, Action::Approve | Action::Accept) => {
                self.queue(Command::ConfirmVerification);
            }
            (Verification::Compare { .. }, Action::Deny) => {
                self.status = Some("reported a mismatch: those keys are not trusted".into());
                self.queue(Command::MismatchVerification);
            }

            (_, Action::Deny | Action::Cancel) => {
                self.queue(Command::CancelVerification);
            }

            // Redraw stays available: a corrupted screen is exactly when a user needs to
            // re-read emoji before answering.
            (_, Action::Redraw) => self.needs_redraw = true,

            _ => {}
        }
    }

    /// Route a key to the recovery panel.
    ///
    /// Only what each state actually asks for is honoured, and everything else is
    /// swallowed. This panel can hold a secret mid-typing, destroy a working recovery
    /// key, or be showing the only copy of a new one, and none of those are places for a
    /// stray binding to reach past.
    fn recovery_action(&mut self, action: Action) {
        let Some(Modal::Recovery(panel)) = &mut self.modal else {
            return;
        };

        match panel {
            RecoveryPanel::AskKey { key, submitted, .. } => match action {
                Action::Insert(c) if !*submitted => key.push(c),
                Action::Backspace if !*submitted => {
                    key.pop();
                }
                Action::Cancel => self.close_recovery(),
                Action::Submit | Action::Accept if !*submitted => {
                    let entered = std::mem::take(key);
                    if entered.trim().is_empty() {
                        self.status = Some("no recovery key entered".into());
                        self.close_recovery();
                        return;
                    }
                    *submitted = true;
                    self.status = Some("unlocking secret storage…".into());
                    self.queue(Command::RecoverWithKey(entered));
                }
                _ => {}
            },

            RecoveryPanel::OfferEnable => match action {
                Action::Approve | Action::Accept | Action::Submit => {
                    *panel = RecoveryPanel::Busy("setting up recovery…");
                    self.queue(Command::EnableRecovery);
                }
                Action::Deny | Action::Cancel => self.close_recovery(),
                _ => {}
            },

            RecoveryPanel::ConfirmReset => match action {
                // Only an explicit yes. Resetting leaves every other device holding a
                // key that no longer opens anything, so it is not something to fall into
                // by pressing enter on a dialog one did not read.
                Action::Approve => {
                    *panel = RecoveryPanel::Busy("creating a new recovery key…");
                    self.queue(Command::ResetRecoveryKey);
                }
                Action::Deny | Action::Cancel => self.close_recovery(),
                _ => {}
            },

            // Nothing to answer while the server is being waited on, and cancelling
            // would not recall the request.
            RecoveryPanel::Busy(_) => {}

            RecoveryPanel::ShowKey { .. } => match action {
                // Any deliberate acknowledgement closes it, but nothing else does: this
                // is the only time the key is ever displayed.
                Action::Submit | Action::Accept | Action::Approve | Action::Cancel => {
                    self.close_recovery();
                }
                _ => {}
            },
        }
    }

    /// Close the recovery panel and give the keyboard back to the transcript.
    fn close_recovery(&mut self) {
        self.close_modal();
        self.mode = Mode::Normal;
    }

    /// Route a key to the command palette.
    fn palette_action(&mut self, action: Action) {
        let Some(Modal::Palette(palette)) = &mut self.modal else {
            return;
        };

        match action {
            Action::Insert(ch) => palette.push(ch),
            Action::Backspace => palette.pop(),
            // Arrows come through as caret movement, because the palette is typed into
            // from insert mode; they are the only way to walk the list while the letters
            // are all going into the query.
            Action::CaretUp | Action::ScrollUp(_) => palette.up(),
            Action::CaretDown | Action::ScrollDown(_) => palette.down(),
            Action::Cancel | Action::CommandPalette => self.close_palette(),
            Action::Submit | Action::Accept => self.run_chosen_command(),
            _ => {}
        }
    }

    /// Run the highlighted command, having first closed the palette.
    ///
    /// Closing first is not tidiness: the command is dispatched back through
    /// `apply_action`, which hands every key to the open overlay, so leaving the palette
    /// open would feed the command straight back into it and do nothing.
    fn run_chosen_command(&mut self) {
        let Some(action) = self
            .palette()
            .and_then(|p| p.chosen())
            .map(|c| c.action.clone())
        else {
            self.status = Some("no command selected".into());
            self.close_palette();
            return;
        };
        self.close_palette();
        self.apply_action(action);
    }

    fn close_palette(&mut self) {
        self.close_modal();
        self.mode = Mode::Normal;
    }

    /// Route a key to whichever overlay is open.
    ///
    /// Every arm consumes the key. An overlay that let one through is how a picker ends
    /// up splitting a pane behind itself, and the thread picker's `_ => {}` did exactly
    /// that: `:` fell past it into the main keymap and opened the palette on top.
    fn modal_action(&mut self, action: Action) {
        match &mut self.modal {
            Some(Modal::Verification(_)) => self.verification_action(action),
            Some(Modal::Recovery(_)) => self.recovery_action(action),
            Some(Modal::Emoji(_)) => self.emoji_action(action),
            Some(Modal::Palette(_)) => self.palette_action(action),
            Some(Modal::Threads(picker)) => match action {
                Action::ScrollUp(_) | Action::SelectOlder => {
                    picker.selected = picker.selected.saturating_sub(1);
                }
                Action::ScrollDown(_) | Action::SelectNewer => {
                    picker.selected =
                        (picker.selected + 1).min(picker.threads.len().saturating_sub(1));
                }
                Action::Accept => self.open_selected_thread(),
                Action::Cancel | Action::OpenThreads => self.close_modal(),
                // Everything else is swallowed. The picker owns navigation while it is
                // open, so the j/k that scroll a transcript walk the list instead of
                // doing both at once -- and nothing else acts underneath it.
                _ => {}
            },
            // The key overlay used to be drawn without appearing in any dispatch chain
            // at all, so `D` still armed a redaction and `Enter` still sent, behind a
            // panel covering the transcript they were acting on.
            Some(Modal::Help) => match action {
                Action::Cancel | Action::ToggleHelp => self.close_modal(),
                _ => {}
            },
            None => {}
        }
    }

    pub fn apply_action(&mut self, action: Action) {
        // An open overlay owns every key. There is one of them by construction, so this
        // is a single check rather than a chain of five whose order had to be reasoned
        // about -- and, in two cases, was reasoned about wrongly. See [`Modal`].
        if self.modal.is_some() {
            self.modal_action(action);
            return;
        }

        // The mention picker is a filter on the composer, not a replacement for it, so
        // it takes only the three keys that mean something to a list and lets every
        // editing key through to the buffer underneath. The popup is then recomputed
        // from that buffer at the end of this function.
        //
        // Only while it has something to show. An armed `@` that matches nobody draws
        // nothing, and a popup nobody can see must not swallow the return key -- that
        // way lies a message that will not send and no way to find out why.
        if self
            .mentions
            .as_ref()
            .is_some_and(|p| !p.matches.is_empty())
        {
            match action {
                // Arrow keys only. The wheel belongs to the transcript: a list of four
                // names is not what someone reaching for the mouse means to scroll.
                Action::CaretUp => {
                    if let Some(p) = &mut self.mentions {
                        p.selected = p.selected.saturating_sub(1);
                    }
                    self.needs_redraw = true;
                    return;
                }
                Action::CaretDown => {
                    if let Some(p) = &mut self.mentions {
                        p.selected = (p.selected + 1).min(p.matches.len().saturating_sub(1));
                    }
                    self.needs_redraw = true;
                    return;
                }
                // Enter completes rather than sends, which is what every client with an
                // autocomplete does; Esc first is how you send the text as written.
                Action::Submit | Action::Complete => {
                    self.accept_mention();
                    return;
                }
                Action::Cancel => {
                    // Only the popup. `keymap` has already set Normal on the way in, and
                    // dropping out of the composer as well would punish a user who just
                    // wanted the list gone.
                    self.mentions = None;
                    self.mode = Mode::Insert;
                    self.needs_redraw = true;
                    return;
                }
                _ => {}
            }
        }

        match action {
            Action::None => {}
            Action::Quit => {
                // Quitting without this leaves the room showing a typing indicator until
                // the server expires it.
                self.stop_typing();
                self.should_quit = true;
            }

            Action::EnterInsert => self.mode = Mode::Insert,

            Action::SelectOlder => self.select_by(-1),
            Action::SelectNewer => self.select_by(1),
            Action::Reply => self.begin_reply(),
            Action::EditMessage => self.begin_edit(),
            Action::RedactMessage => self.redact_selected(),
            Action::OpenThreads => self.open_thread_picker(),
            Action::EmojiIntoComposer => self.open_emoji_for_composer(),
            Action::ReactToSelected => self.open_emoji_for_reaction(),
            Action::Accept => self.accept_selection(),
            Action::Cancel => {
                self.close_modal();
                self.cancel_pending();
                self.stop_typing();
                self.mode = Mode::Normal;
            }

            Action::Insert(c) => {
                if let Some(composer) = self.composer_mut() {
                    composer.insert(c);
                }
                self.note_typing();
            }
            Action::Backspace => {
                if let Some(composer) = self.composer_mut() {
                    composer.backspace();
                }
                self.note_typing();
            }
            Action::Delete => {
                if let Some(composer) = self.composer_mut() {
                    composer.delete();
                }
                self.note_typing();
            }
            Action::DeleteWord => {
                if let Some(composer) = self.composer_mut() {
                    composer.delete_word();
                }
                self.note_typing();
            }
            Action::DeleteToLineStart => {
                if let Some(composer) = self.composer_mut() {
                    composer.delete_to_line_start();
                }
                self.note_typing();
            }
            Action::Newline => {
                if let Some(composer) = self.composer_mut() {
                    composer.insert_newline();
                }
                self.note_typing();
            }
            Action::CaretLeft => {
                if let Some(composer) = self.composer_mut() {
                    composer.left();
                }
            }
            Action::CaretRight => {
                if let Some(composer) = self.composer_mut() {
                    composer.right();
                }
            }
            Action::CaretWordLeft => {
                if let Some(composer) = self.composer_mut() {
                    composer.word_left();
                }
            }
            Action::CaretWordRight => {
                if let Some(composer) = self.composer_mut() {
                    composer.word_right();
                }
            }
            Action::CaretHome => {
                if let Some(composer) = self.composer_mut() {
                    composer.home();
                }
            }
            Action::CaretEnd => {
                if let Some(composer) = self.composer_mut() {
                    composer.end();
                }
            }
            Action::CaretUp => self.composer_up(),
            Action::CaretDown => self.composer_down(),
            // Reaching here means no completion was on offer, so tab does nothing rather
            // than putting a tab character into a chat message.
            Action::Complete => {}
            Action::Submit => self.submit(),

            Action::Split(dir) => self.split(dir),
            Action::ClosePane => self.close_pane(),
            Action::FocusPane(dir) => self.focus_pane(dir),
            Action::ResizePane(dir) => self.resize_pane(dir),
            Action::ZoomPane => {
                if let Some(tiling) = self.focused_tiling_mut() {
                    tiling.toggle_zoom();
                }
                self.touch_layout();
            }
            Action::NewThread => self.start_thread(),

            Action::OpenRecovery => {
                // What the key opens depends entirely on where the account stands, and
                // offering the wrong one is worse than offering nothing: asking for a
                // key that was never created, or quietly replacing one that works.
                self.open_modal(Modal::Recovery(match self.recovery {
                    RecoveryState::Disabled => RecoveryPanel::OfferEnable,
                    RecoveryState::Enabled => RecoveryPanel::ConfirmReset,
                    RecoveryState::Incomplete | RecoveryState::Unknown => RecoveryPanel::AskKey {
                        key: String::new(),
                        submitted: false,
                        error: None,
                    },
                }));
                self.mode = Mode::Insert;
            }

            Action::StartVerification => {
                if self.device_verified == Some(true) {
                    self.status = Some("this device is already verified".into());
                } else {
                    self.status = Some("asking your other devices to verify this one…".into());
                    self.queue(Command::StartVerification);
                }
            }

            Action::NextTab => {
                if let Some(w) = self.workspaces.focused_mut() {
                    w.next_tab();
                }
                self.focus_moved();
            }
            Action::PrevTab => {
                if let Some(w) = self.workspaces.focused_mut() {
                    w.prev_tab();
                }
                self.focus_moved();
            }

            Action::ScrollUp(n) => self.scroll_by(-(n as i32)),
            Action::ScrollDown(n) => self.scroll_by(n as i32),
            Action::ScrollTop => self.set_scroll(u16::MAX),
            Action::ScrollBottom => self.set_scroll(0),

            Action::ToggleCard => self.toggle_card(),

            Action::Approve => self.resolve_prompt(true),
            Action::Deny => self.resolve_prompt(false),

            Action::ToggleHelp => self.open_modal(Modal::Help),
            Action::Redraw => self.needs_redraw = true,

            Action::NextWorkspace => {
                self.workspaces.next();
                self.focus_moved();
            }
            Action::PrevWorkspace => {
                self.workspaces.prev();
                self.focus_moved();
            }

            Action::FuzzyJump => {
                // Jumping to a room or thread by name earns its keep across many
                // Spaces; with a handful, `<prefix> w` and `<prefix> n` already reach
                // everything. Deferred to M6 with the rest of the convenience work.
                self.status = Some("fuzzy jump is not implemented yet".into());
            }
            Action::CommandPalette => {
                self.open_modal(Modal::Palette(Palette::new()));
                // The palette is a search box, so keys have to arrive as characters
                // rather than as the commands they mean in normal mode.
                self.mode = Mode::Insert;
            }
        }

        // The popup follows the buffer rather than the keystrokes, so it is recomputed
        // once here from whatever the edit left behind. Doing it per arm would mean
        // fourteen call sites and one of them eventually forgotten.
        if touches_composer(action) {
            self.refilter_mentions();
        }
    }

    fn submit(&mut self) {
        // Sending is the clearest possible "finished typing", and it must not wait for
        // the idle timer: the agent sees the message and a live typing notice at once.
        self.stop_typing();
        let Some(view) = self.focused_view() else {
            self.status = Some("no pane focused".into());
            return;
        };
        // `take` clears the buffer and records the message in this view's history.
        let Some(body) = self.composers.entry(view.clone()).or_default().take() else {
            return;
        };
        // Read back off the finished text rather than tracked while typing, so a name
        // typed out in full mentions its owner exactly like one picked from the list,
        // and one deleted afterwards mentions nobody.
        let mentions = self.mentioned_in(&view.room_id, &body);

        let command = match self.composing.take() {
            Some(Pending::Reply(in_reply_to)) => Command::SendReply {
                view,
                in_reply_to,
                body,
                mentions,
            },
            Some(Pending::Edit(event_id)) => Command::Edit {
                view,
                event_id,
                body,
                mentions,
            },
            None => Command::SendMessage {
                view,
                body,
                mentions,
            },
        };
        self.queue(command);
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
        self.touch_layout();
    }

    fn close_pane(&mut self) {
        // A tab with no panes shows nothing and offers no way back, so the last one
        // stays. Closing the tab itself is the operation the user wants there.
        let remaining = self
            .workspaces
            .focused()
            .and_then(|w| w.focused_tab())
            .map_or(0, |t| t.panes.len());
        if remaining <= 1 {
            self.status = Some("the last pane stays open".into());
            return;
        }

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
        self.touch_layout();
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
        self.focus_moved();
    }

    fn resize_pane(&mut self, dir: Dir) {
        if let Some(tiling) = self.focused_tiling_mut() {
            tiling.resize(dir, RESIZE_STEP);
        }
        self.touch_layout();
    }

    /// Take hold of the split border under the pointer, if there is one.
    ///
    /// Returns whether a drag began, so the caller knows not to treat the same press as
    /// a click into a pane. Grabbing a border does not move focus: the pane you are
    /// reading should not change because you widened the one beside it.
    pub fn begin_drag(&mut self, column: u16, row: u16) -> bool {
        let Some(room_id) = self
            .workspaces
            .focused()
            .and_then(|w| w.focused_tab())
            .map(|t| t.room_id.clone())
        else {
            return false;
        };
        let Some(handle) = self
            .tilings
            .get(&room_id)
            .and_then(|t| t.split_at(column, row))
        else {
            return false;
        };
        self.dragging = Some((room_id, handle));
        true
    }

    /// Move a held border to the pointer.
    pub fn drag_to(&mut self, column: u16, row: u16) {
        let Some((room_id, handle)) = &self.dragging else {
            return;
        };
        let Some(tiling) = self.tilings.get_mut(room_id) else {
            return;
        };
        if tiling.drag(handle, column, row) {
            // Panes have moved under text ratatui believes is already correct, and the
            // same stale-cell problem that follows a focus change follows this.
            self.needs_redraw = true;
            self.layout_dirty = true;
        }
    }

    /// Let go of the border, if one was held.
    pub fn end_drag(&mut self) {
        self.dragging = None;
    }

    /// Whether a border is currently being dragged.
    pub fn is_dragging(&self) -> bool {
        self.dragging.is_some()
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
        self.focus_moved();
    }

    /// The user moved focus: repaint, mark the new view seen, and stream it.
    ///
    /// The repaint is not cosmetic. ratatui rewrites only the cells it believes have
    /// changed, and a glyph that paints wider than it was measured leaves that belief
    /// out of step with the screen. Switching to a shorter transcript then leaves the
    /// previous room's text visible underneath it.
    fn focus_moved(&mut self) {
        // The notice belongs to the room being left, so it has to go before the focus
        // does; afterwards there is nothing left pointing at the old room.
        self.stop_typing();
        // The picker belongs to one composer in one pane. Carried across, it would offer
        // the last pane's half-typed name over this pane's transcript.
        self.mentions = None;
        self.needs_redraw = true;
        // Where the user is looking is part of the layout, and it is also the signal
        // that they have taken over from the restore: from here on, moving them would
        // be the client fighting them.
        self.restoring_focus = false;
        self.touch_layout();
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
                    self.focus_moved();
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
                    self.focus_moved();
                }
            }
            return true;
        }

        false
    }

    /// Ask the worker to stream every view on show, not just the focused one.
    ///
    /// Unfocused panes render their own transcript, so they need their own timeline.
    /// Streaming only the focused view is what left the room pane blank the moment a
    /// thread took focus.
    pub fn open_focused_view(&mut self) {
        let views: Vec<View> = self
            .workspaces
            .focused()
            .and_then(|w| w.focused_tab())
            .map(|tab| {
                tab.panes
                    .iter()
                    .map(|pane| match pane.kind.thread_root() {
                        Some(root) => View::thread(pane.kind.room_id(), root),
                        None => View::room(pane.kind.room_id()),
                    })
                    .collect()
            })
            .unwrap_or_default();

        for view in views {
            if self.timelines.contains_key(&view) {
                continue;
            }
            self.queue(Command::OpenView(view.clone()));
            // A live timeline starts with only what sync delivered, which for a room
            // opened at launch is usually nothing. Without this first page the pane is
            // simply empty and the client looks broken. The worker handles commands in
            // order and `OpenView` is awaited, so the timeline exists by the time this
            // is processed.
            self.paginating.insert(view.clone());
            self.queue(Command::Paginate { view, count: 0 });
        }

        self.ask_for_members();
    }

    /// Ask who is in the focused room, once.
    ///
    /// Asked on focus rather than when `@` is typed so the picker has a list to show the
    /// moment it opens. A roster that arrives a round trip after the popup does is a
    /// popup that appears empty and then jumps.
    fn ask_for_members(&mut self) {
        let Some(room_id) = self
            .workspaces
            .focused()
            .and_then(|w| w.focused_tab())
            .map(|tab| tab.room_id.clone())
        else {
            return;
        };
        // Tracked separately from `members` because a room can legitimately answer with
        // nobody, and an empty answer must not look like an unasked question.
        if self.members_asked.insert(room_id.clone()) {
            self.queue(Command::ListMembers { room_id });
        }
    }

    /// Recompute the mention picker from the composer, opening or closing it as needed.
    ///
    /// Called after every composer edit rather than driven by its own keystrokes: the
    /// buffer is the truth about what is being typed, and deriving the query from it is
    /// what makes the popup survive a backspace, a caret move, or a pasted line.
    fn refilter_mentions(&mut self) {
        let Some(view) = self.focused_view() else {
            self.mentions = None;
            return;
        };
        let Some((start, query)) = self
            .composers
            .get(&view)
            .and_then(|c| c.mention_query())
            .map(|(start, query)| (start, query.to_owned()))
        else {
            self.mentions = None;
            return;
        };

        let members = self.members.get(&view.room_id).map(Vec::as_slice);
        let matches = rank_members(members.unwrap_or_default(), &query);
        // An armed `@` with nothing behind it draws no popup, but stays armed: the next
        // character may well match, and closing here would mean the picker never opens
        // for a room whose roster arrives late.
        let selected = match &self.mentions {
            // Keep the highlight where the user put it while the query is unchanged.
            Some(open) if open.start == start && open.matches == matches => {
                open.selected.min(matches.len().saturating_sub(1))
            }
            _ => 0,
        };
        self.mentions = Some(MentionPicker {
            start,
            matches,
            selected,
        });
        self.needs_redraw = true;
    }

    /// Members of `room_id` whose name is written in `body`, as user IDs.
    ///
    /// Read off the finished message rather than remembered from the picker, so that a
    /// name typed out in full counts and one deleted afterwards does not. The cost is
    /// that writing *about* someone mentions them, which is how every other client
    /// behaves and which `m.mentions` makes no worse than a notification.
    fn mentioned_in(&self, room_id: &str, body: &str) -> Vec<String> {
        let Some(members) = self.members.get(room_id) else {
            return Vec::new();
        };
        let mut out: Vec<String> = Vec::new();
        for word in mention_words(body) {
            if let Some(member) = resolve_mention(members, word) {
                if !out.contains(&member.user_id) {
                    out.push(member.user_id.clone());
                }
            }
        }
        out
    }

    /// Put the highlighted member into the composer, replacing what was typed.
    fn accept_mention(&mut self) {
        let Some(picker) = self.mentions.take() else {
            return;
        };
        let Some(view) = self.focused_view() else {
            return;
        };
        let Some(member) = picker
            .matches
            .get(picker.selected)
            .and_then(|&i| self.members.get(&view.room_id).and_then(|m| m.get(i)))
        else {
            return;
        };

        let text = format!("{} ", mention_text(member, self.members_of(&view.room_id)));
        let start = picker.start;
        if let Some(composer) = self.composers.get_mut(&view) {
            composer.replace_mention(start, &text);
        }
        self.needs_redraw = true;
    }

    fn members_of(&self, room_id: &str) -> &[MemberSummary] {
        self.members.get(room_id).map_or(&[], Vec::as_slice)
    }

    /// The members the open picker is offering, in match order.
    pub fn mention_matches(&self) -> Vec<&MemberSummary> {
        let Some(picker) = &self.mentions else {
            return Vec::new();
        };
        let Some(view) = self.focused_view() else {
            return Vec::new();
        };
        let members = self.members_of(&view.room_id);
        picker
            .matches
            .iter()
            .filter_map(|&i| members.get(i))
            .collect()
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
            let AgentPayload::Structured { event, .. } = &message.agent else {
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
            let AgentPayload::Structured { event, .. } = &message.agent else {
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

/// Whether an action can have changed the composer's text or caret.
///
/// The mention picker is recomputed after exactly these, and after nothing else: a
/// pane split or a scroll leaves the buffer alone, and re-deriving the popup from an
/// unchanged buffer would reopen a picker the user had just dismissed.
fn touches_composer(action: Action) -> bool {
    matches!(
        action,
        Action::Insert(_)
            | Action::Backspace
            | Action::Delete
            | Action::DeleteWord
            | Action::DeleteToLineStart
            | Action::Newline
            | Action::CaretLeft
            | Action::CaretRight
            | Action::CaretWordLeft
            | Action::CaretWordRight
            | Action::CaretHome
            | Action::CaretEnd
            | Action::CaretUp
            | Action::CaretDown
    )
}

/// The text heddle writes into the message for a mention.
///
/// The localpart, not the display name, and never the raw display name of someone whose
/// name is shared. Two reasons, both about the message being readable back:
///
/// - A display name may contain spaces, and a mention that contains a space cannot be
///   found again by [`mention_words`], so it would be offered by the picker and then
///   silently fail to mention anyone.
/// - Two members can show the same name. `@alex` would then name nobody in particular,
///   and the reader has no way to tell which was meant.
///
/// So an unambiguous member gets `@localpart` and everyone else gets their full ID.
/// `m.mentions` carries the authoritative user ID either way; this is what a human sees.
fn mention_text(member: &MemberSummary, room: &[MemberSummary]) -> String {
    let own = localpart(&member.user_id);
    let shared = room
        .iter()
        .filter(|other| localpart(&other.user_id) == own)
        .count()
        > 1;
    if shared {
        // Already carries its own sigil.
        member.user_id.clone()
    } else {
        format!("@{own}")
    }
}

/// The localpart of a user ID, without its leading sigil.
fn localpart(user_id: &str) -> &str {
    user_id
        .strip_prefix('@')
        .unwrap_or(user_id)
        .split(':')
        .next()
        .unwrap_or_default()
}

/// Every `@word` in a message, without its sigil.
///
/// Word here means what [`Composer::mention_query`] means by it, so that what the picker
/// wrote can be read back: a run starting at a word boundary and ending at whitespace.
///
/// Trailing punctuation is trimmed, because "thanks @bob!" mentions bob and so does
/// "ask @bob." — including the full-stop case, which also leaves a trailing dot off a
/// homeserver name without eating the dots inside it. Underscore and hyphen survive:
/// they end no sentence and they are ordinary in a localpart.
fn mention_words(body: &str) -> Vec<&str> {
    let trailing = |c: char| c.is_ascii_punctuation() && c != '_' && c != '-';
    body.split_whitespace()
        .filter_map(|word| word.strip_prefix('@'))
        .map(|word| word.trim_end_matches(trailing))
        .filter(|word| !word.is_empty())
        .collect()
}

/// Which member, if any, a written `@word` names.
///
/// Tried as a full user ID, then a localpart, then a display name. Each step only
/// answers when it names exactly one member: two people can share a localpart across
/// homeservers and two more can share a display name, and picking the first of them
/// would put a notification in front of somebody who was never addressed. A name that
/// names two people names neither, and the message goes out mentioning nobody rather
/// than mentioning the wrong person.
fn resolve_mention<'a>(members: &'a [MemberSummary], word: &str) -> Option<&'a MemberSummary> {
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);

    only(members, |m| eq(m.user_id.trim_start_matches('@'), word))
        .or_else(|| only(members, |m| eq(localpart(&m.user_id), word)))
        .or_else(|| only(members, |m| !m.ambiguous && eq(&m.display_name, word)))
}

/// The one member matching `f`, or `None` if none or several do.
fn only(members: &[MemberSummary], f: impl Fn(&MemberSummary) -> bool) -> Option<&MemberSummary> {
    let mut hits = members.iter().filter(|m| f(m));
    match (hits.next(), hits.next()) {
        (Some(one), None) => Some(one),
        _ => None,
    }
}

/// Members matching `query`, best first, as indices into `members`.
///
/// An empty query offers everyone, in the order the worker sorted them. Otherwise both
/// the display name and the localpart are scored and the better of the two wins, so
/// `@qui` finds "Quintin" and `@wri` finds a bot whose display name is "Retinue".
fn rank_members(members: &[MemberSummary], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..members.len()).collect();
    }

    let mut scored: Vec<_> = members
        .iter()
        .enumerate()
        .filter_map(|(i, member)| {
            let by_name = crate::palette::rank(&member.display_name, query);
            let by_id = crate::palette::rank(localpart(&member.user_id), query);
            let best = match (by_name, by_id) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            }?;
            Some((best, i))
        })
        .collect();
    // Stable, so members that score the same keep the worker's alphabetical order.
    scored.sort_by_key(|(score, _)| *score);
    scored.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use heddle_agent::{AgentEvent, Approval, Kind, Tool, ToolStatus};
    use heddle_matrix::{AgentPayload, EntryKind, Message};

    fn members() -> Vec<MemberSummary> {
        vec![
            MemberSummary {
                user_id: "@quintin:example.org".into(),
                display_name: "Quintin".into(),
                ambiguous: false,
            },
            MemberSummary {
                user_id: "@wright:example.org".into(),
                display_name: "Retinue".into(),
                ambiguous: false,
            },
            MemberSummary {
                user_id: "@alex:example.org".into(),
                display_name: "Alex".into(),
                ambiguous: true,
            },
            MemberSummary {
                user_id: "@alex:other.example".into(),
                display_name: "Alex".into(),
                ambiguous: true,
            },
        ]
    }

    #[test]
    fn a_mention_is_written_as_a_localpart() {
        // Not the display name: it may contain a space, and a mention with a space in it
        // cannot be found again when the message is read back.
        let m = members();
        assert_eq!(mention_text(&m[0], &m), "@quintin");
    }

    #[test]
    fn a_shared_localpart_is_written_out_in_full() {
        // Two Alexes on different homeservers. `@alex` would name neither of them.
        let m = members();
        assert_eq!(mention_text(&m[2], &m), "@alex:example.org");
        assert_eq!(mention_text(&m[3], &m), "@alex:other.example");
    }

    #[test]
    fn a_written_mention_resolves_to_a_user_id() {
        let m = members();
        assert_eq!(
            resolve_mention(&m, "quintin").map(|x| x.user_id.as_str()),
            Some("@quintin:example.org")
        );
        // By display name too, which is what someone typing without the picker does.
        assert_eq!(
            resolve_mention(&m, "Retinue").map(|x| x.user_id.as_str()),
            Some("@wright:example.org")
        );
    }

    #[test]
    fn an_ambiguous_display_name_resolves_to_nobody() {
        // Guessing between two people called Alex would notify the wrong one.
        assert!(resolve_mention(&members(), "Alex").is_none());
        // The full ID still works, because it names exactly one of them.
        assert_eq!(
            resolve_mention(&members(), "alex:other.example").map(|x| x.user_id.as_str()),
            Some("@alex:other.example")
        );
    }

    #[test]
    fn mentions_are_read_out_of_a_finished_message() {
        let mut app = app();
        app.members.insert("!r:x".into(), members());
        assert_eq!(
            app.mentioned_in("!r:x", "morning @quintin and @wright, ready?"),
            vec![
                "@quintin:example.org".to_owned(),
                "@wright:example.org".to_owned()
            ]
        );
    }

    #[test]
    fn a_mention_at_the_end_of_a_sentence_still_counts() {
        let mut app = app();
        app.members.insert("!r:x".into(), members());
        assert_eq!(
            app.mentioned_in("!r:x", "please look at this, @quintin."),
            vec!["@quintin:example.org".to_owned()]
        );
    }

    #[test]
    fn an_email_address_mentions_nobody() {
        let mut app = app();
        app.members.insert("!r:x".into(), members());
        assert!(app
            .mentioned_in("!r:x", "write to quintin@example.org")
            .is_empty());
    }

    #[test]
    fn the_same_person_is_mentioned_once() {
        // m.mentions is a set; sending a duplicate would be sending a malformed event.
        let mut app = app();
        app.members.insert("!r:x".into(), members());
        assert_eq!(
            app.mentioned_in("!r:x", "@quintin @quintin @quintin:example.org"),
            vec!["@quintin:example.org".to_owned()]
        );
    }

    #[test]
    fn a_room_with_no_roster_mentions_nobody() {
        // Rather than inventing a user ID from the text, which would be a mention the
        // server rejects and a send that fails.
        assert!(app().mentioned_in("!r:x", "hello @quintin").is_empty());
    }

    #[test]
    fn ranking_finds_a_member_by_name_or_by_id() {
        let m = members();
        // "wri" is nowhere in the display name "Retinue", but it starts the localpart.
        assert_eq!(rank_members(&m, "wri"), vec![1]);
        // And the other way about.
        assert_eq!(rank_members(&m, "retin"), vec![1]);
    }

    #[test]
    fn ranking_with_no_query_offers_the_whole_room() {
        assert_eq!(rank_members(&members(), ""), vec![0, 1, 2, 3]);
    }

    fn app() -> App {
        let mut app = App::new(Config::default(), Layout::default());
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

    /// An app with a focused room and a known roster.
    fn app_with_members() -> App {
        let mut app = app();
        app.apply_worker_event(WorkerEvent::Members {
            room_id: "!r:x".into(),
            members: members(),
        });
        app.mode = Mode::Insert;
        app
    }

    #[test]
    fn the_room_roster_is_asked_for_when_a_room_is_focused() {
        let mut app = App::new(Config::default(), Layout::default());
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
        assert!(
            app.take_commands()
                .iter()
                .any(|c| matches!(c, Command::ListMembers { room_id } if room_id == "!r:x")),
            "the picker needs a roster before the user types @, not after"
        );
    }

    #[test]
    fn the_roster_is_only_asked_for_once() {
        let mut app = app_with_members();
        app.open_focused_view();
        assert!(!app
            .take_commands()
            .iter()
            .any(|c| matches!(c, Command::ListMembers { .. })));
    }

    #[test]
    fn typing_an_at_opens_the_picker_and_a_space_closes_it() {
        let mut app = app_with_members();
        type_into(&mut app, "hi @");
        assert!(app.mentions.is_some(), "@ arms the picker");
        assert_eq!(app.mention_matches().len(), 4, "and offers the whole room");

        type_into(&mut app, "qu");
        assert_eq!(
            app.mention_matches().first().map(|m| m.user_id.as_str()),
            Some("@quintin:example.org")
        );

        type_into(&mut app, "x ");
        assert!(app.mentions.is_none(), "a space ends the mention");
    }

    #[test]
    fn tab_completes_the_highlighted_member() {
        let mut app = app_with_members();
        type_into(&mut app, "morning @qu");
        app.apply_action(Action::Complete);
        assert_eq!(
            app.composer().map(Composer::text),
            Some("morning @quintin ")
        );
        assert!(app.mentions.is_none(), "completing closes the picker");
    }

    #[test]
    fn enter_completes_rather_than_sending_while_the_picker_is_open() {
        let mut app = app_with_members();
        type_into(&mut app, "@qu");
        app.apply_action(Action::Submit);
        assert_eq!(app.composer().map(Composer::text), Some("@quintin "));
        assert!(
            app.take_commands().is_empty(),
            "the half-typed name must not go out as a message"
        );
    }

    #[test]
    fn escape_dismisses_the_picker_without_leaving_the_composer() {
        let mut app = app_with_members();
        type_into(&mut app, "@qu");
        // `keymap` sets Normal on the way in; the handler has to put it back.
        app.mode = Mode::Normal;
        app.apply_action(Action::Cancel);
        assert!(app.mentions.is_none());
        assert_eq!(
            app.mode,
            Mode::Insert,
            "escape closed the popup, not the composer"
        );
        assert_eq!(
            app.composer().map(Composer::text),
            Some("@qu"),
            "the text survives"
        );

        // And now enter sends, because there is no completion in the way.
        app.apply_action(Action::Submit);
        assert!(app
            .take_commands()
            .iter()
            .any(|c| matches!(c, Command::SendMessage { .. })));
    }

    #[test]
    fn up_and_down_move_the_highlight_instead_of_the_caret() {
        let mut app = app_with_members();
        type_into(&mut app, "@");
        app.apply_action(Action::CaretDown);
        assert_eq!(app.mentions.as_ref().map(|m| m.selected), Some(1));
        app.apply_action(Action::CaretUp);
        assert_eq!(app.mentions.as_ref().map(|m| m.selected), Some(0));
    }

    #[test]
    fn a_sent_message_carries_the_mention_on_the_wire() {
        let mut app = app_with_members();
        type_into(&mut app, "@qu");
        app.apply_action(Action::Complete);
        type_into(&mut app, "any news?");
        app.apply_action(Action::Submit);

        let commands = app.take_commands();
        let sent = commands
            .iter()
            .find_map(|c| match c {
                Command::SendMessage { body, mentions, .. } => Some((body, mentions)),
                _ => None,
            })
            .expect("a message went out");
        assert_eq!(sent.0, "@quintin any news?");
        // The part that actually notifies. Without it the text is decoration.
        assert_eq!(sent.1, &vec!["@quintin:example.org".to_owned()]);
    }

    #[test]
    fn an_at_that_matches_nobody_does_not_swallow_the_return_key() {
        // The popup draws nothing when it has no matches, and a popup nobody can see
        // must not eat the send. Otherwise the message simply refuses to go, silently.
        let mut app = app_with_members();
        type_into(&mut app, "email me @ 5pm");
        app.apply_action(Action::Submit);
        assert!(
            app.take_commands()
                .iter()
                .any(|c| matches!(c, Command::SendMessage { .. })),
            "the message has to send even with an armed but empty picker"
        );
    }

    #[test]
    fn the_mouse_wheel_scrolls_the_transcript_not_the_picker() {
        let mut app = app_with_members();
        type_into(&mut app, "@");
        app.apply_action(Action::ScrollUp(3));
        assert_eq!(
            app.mentions.as_ref().map(|m| m.selected),
            Some(0),
            "reaching for the mouse does not mean picking a name"
        );
    }

    #[test]
    fn moving_pane_closes_the_picker() {
        let mut app = app_with_members();
        type_into(&mut app, "@qu");
        assert!(app.mentions.is_some());
        app.open_thread_pane("$root".into(), "a thread".into());
        assert!(
            app.mentions.is_none(),
            "a picker left open would offer the last pane's name over this one"
        );
    }

    #[test]
    fn the_picker_works_in_a_thread_pane_too() {
        // A thread can have more than two participants, and its roster is the room's.
        let mut app = app_with_members();
        app.open_thread_pane("$root".into(), "a thread".into());
        app.mode = Mode::Insert;
        type_into(&mut app, "@wri");
        assert_eq!(
            app.mention_matches().first().map(|m| m.user_id.as_str()),
            Some("@wright:example.org"),
            "the mention picker is not special-cased by pane kind"
        );
    }

    fn agent_entry(event_id: &str, event: AgentEvent) -> Entry {
        Entry {
            id: event_id.into(),
            event_id: Some(event_id.into()),
            kind: EntryKind::Message(Message {
                shield: Shield::None,
                sender: "@hermes:x".into(),
                sender_display: "hermes".into(),
                body: "chrome".into(),
                timestamp: 0,
                is_own: false,
                is_edited: false,
                thread_root: Some("$root".into()),
                thread_replies: None,
                reactions: Vec::new(),
                agent: AgentPayload::Structured {
                    adapter: "test",
                    event: Box::new(event),
                },
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
        let mut app = App::new(Config::default(), Layout::default());
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
        assert_eq!(tab.unread, Unread::new(3, 1));
        assert_eq!(
            app.workspaces.focused().expect("workspace").tabs.len(),
            1,
            "re-sending the room list must not clone tabs"
        );
    }

    /// Type into the focused view's composer.
    fn type_into(app: &mut App, text: &str) {
        app.mode = Mode::Insert;
        for c in text.chars() {
            if c == '\n' {
                app.apply_action(Action::Newline);
            } else {
                app.apply_action(Action::Insert(c));
            }
        }
    }

    #[test]
    fn submitting_sends_and_clears_the_composer() {
        let mut app = app();
        type_into(&mut app, "  hello  ");
        app.apply_action(Action::Submit);

        assert!(app.composer().expect("composer").text().is_empty());
        let commands = app.take_commands();
        match commands.as_slice() {
            [Command::SendMessage { view, body, .. }] => {
                assert_eq!(view, &View::room("!r:x"));
                assert_eq!(body, "hello");
            }
            other => panic!("expected one SendMessage, got {other:?}"),
        }
    }

    #[test]
    fn submitting_an_empty_composer_sends_nothing() {
        let mut app = app();
        type_into(&mut app, "   \n ");
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

    // --------------------------------------------------------------- mouse resize

    const PANE_AREA: ratatui::layout::Rect = ratatui::layout::Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 24,
    };

    /// An app with two panes side by side, laid out as the renderer would leave them.
    fn split_app() -> (App, u16) {
        let mut app = app();
        app.apply_action(Action::Split(Dir::Right));
        let tiling = app.tilings.get_mut("!r:x").expect("tiling");
        let placements = tiling.layout(PANE_AREA);
        let seam = placements[0].rect.right();
        (app, seam)
    }

    #[test]
    fn dragging_a_border_resizes_without_moving_focus() {
        let (mut app, seam) = split_app();
        let focused_before = app
            .tilings
            .get("!r:x")
            .and_then(heddle_layout::Tiling::focused);
        let width_before = app
            .tilings
            .get_mut("!r:x")
            .expect("tiling")
            .layout(PANE_AREA)[0]
            .rect
            .width;

        assert!(app.begin_drag(seam, 5), "the seam must be grabbable");
        app.drag_to(PANE_AREA.width / 4, 5);
        app.end_drag();

        let width_after = app
            .tilings
            .get_mut("!r:x")
            .expect("tiling")
            .layout(PANE_AREA)[0]
            .rect
            .width;
        assert!(width_after < width_before);
        assert_eq!(
            app.tilings
                .get("!r:x")
                .and_then(heddle_layout::Tiling::focused),
            focused_before,
            "widening a pane is not a request to read it"
        );
        assert!(app.layout_dirty, "a resize is worth remembering");
        assert!(!app.is_dragging(), "the border must be let go of");
    }

    #[test]
    fn a_press_in_a_transcript_is_not_a_resize() {
        let (mut app, _) = split_app();
        assert!(!app.begin_drag(PANE_AREA.width / 4, PANE_AREA.height / 2));
        assert!(!app.is_dragging());
    }

    #[test]
    fn moving_the_mouse_without_a_border_held_changes_nothing() {
        // Every drag event arrives whether or not the press that started it grabbed
        // anything, so this is the common case, not an edge one.
        let (mut app, _) = split_app();
        app.layout_dirty = false;
        app.drag_to(10, 10);
        assert!(!app.layout_dirty);
    }

    #[test]
    fn a_drag_survives_the_pointer_leaving_the_pane() {
        // Terminals keep reporting drag events past the edge of the split, and the
        // ratio is clamped rather than the drag being dropped.
        let (mut app, seam) = split_app();
        assert!(app.begin_drag(seam, 5));
        app.drag_to(0, 0);
        app.drag_to(PANE_AREA.width * 3, PANE_AREA.height * 3);

        let placements = app
            .tilings
            .get_mut("!r:x")
            .expect("tiling")
            .layout(PANE_AREA);
        assert!(placements.iter().all(|p| p.rect.width > 0));
        assert!(app.is_dragging(), "the border is still held");
    }

    // ------------------------------------------------------------------- palette

    fn type_query(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.apply_action(Action::Insert(ch));
        }
    }

    #[test]
    fn the_palette_runs_the_command_it_is_showing() {
        let mut app = app();
        assert_eq!(
            app.workspaces
                .focused()
                .expect("w")
                .focused_tab()
                .expect("t")
                .panes
                .len(),
            1
        );

        app.apply_action(Action::CommandPalette);
        assert!(app.palette().is_some());
        assert_eq!(app.mode, Mode::Insert, "the palette is typed into");

        type_query(&mut app, "split r");
        assert_eq!(
            app.palette()
                .cloned()
                .as_ref()
                .and_then(|p| p.chosen())
                .expect("match")
                .name,
            "split right"
        );

        app.apply_action(Action::Submit);
        assert!(
            app.palette().is_none(),
            "running a command closes the palette"
        );
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            app.workspaces
                .focused()
                .expect("w")
                .focused_tab()
                .expect("t")
                .panes
                .len(),
            2,
            "the command must actually run, not merely be selected"
        );
    }

    #[test]
    fn the_palette_swallows_keys_that_mean_something_else() {
        // `q` is quit and `j` selects a message. While the palette is open they are
        // letters being typed into a search box and nothing more.
        let mut app = app();
        app.apply_action(Action::CommandPalette);
        type_query(&mut app, "qj");

        assert!(!app.should_quit);
        assert_eq!(app.palette().expect("open").query, "qj");
    }

    #[test]
    fn escape_closes_the_palette_without_running_anything() {
        let mut app = app();
        app.apply_action(Action::CommandPalette);
        type_query(&mut app, "quit");
        app.apply_action(Action::Cancel);

        assert!(app.palette().is_none());
        assert_eq!(app.mode, Mode::Normal);
        assert!(
            !app.should_quit,
            "cancelling must not run the highlighted row"
        );
    }

    #[test]
    fn submitting_a_query_that_matches_nothing_does_nothing() {
        let mut app = app();
        app.apply_action(Action::CommandPalette);
        type_query(&mut app, "xyzzy");
        app.apply_action(Action::Submit);

        assert!(app.palette().is_none());
        assert!(!app.should_quit);
        assert_eq!(app.status.as_deref(), Some("no command selected"));
    }

    #[test]
    fn the_arrows_walk_the_list_while_the_letters_go_to_the_query() {
        let mut app = app();
        app.apply_action(Action::CommandPalette);
        app.apply_action(Action::CaretDown);
        assert_eq!(app.palette().expect("open").selected, 1);
        assert!(app.palette().expect("open").query.is_empty());

        app.apply_action(Action::CaretUp);
        assert_eq!(app.palette().expect("open").selected, 0);
    }

    #[test]
    fn backspace_edits_the_query_rather_than_the_composer() {
        let mut app = app();
        app.apply_action(Action::CommandPalette);
        type_query(&mut app, "zoom");
        app.apply_action(Action::Backspace);
        assert_eq!(app.palette().expect("open").query, "zoo");
        assert!(app.composer().is_none_or(|c| c.text().is_empty()));
    }

    #[test]
    fn a_command_that_opens_another_overlay_hands_over_cleanly() {
        // The palette closes before dispatching, or the command would be fed straight
        // back into the palette by the overlay check at the top of `apply_action`.
        let mut app = app();
        app.apply_action(Action::CommandPalette);
        type_query(&mut app, "thread picker");
        app.apply_action(Action::Submit);

        assert!(app.palette().is_none());
        assert!(
            app.threads().is_some(),
            "the thread picker should have opened"
        );
    }

    // ------------------------------------------------------------ layout persistence

    fn summary(room_id: &str, name: &str) -> RoomSummary {
        RoomSummary {
            room_id: room_id.into(),
            display_name: name.into(),
            is_space: false,
            parents: Vec::new(),
            is_direct: false,
            is_encrypted: false,
            notification_count: 0,
            highlight_count: 0,
        }
    }

    /// An app whose room has been split into a room pane and a thread pane.
    fn arranged() -> App {
        let mut app = app();
        app.open_thread_pane("$thread".into(), "a thread".into());
        assert_eq!(
            app.workspaces
                .focused()
                .expect("w")
                .focused_tab()
                .expect("t")
                .panes
                .len(),
            2
        );
        app
    }

    #[test]
    fn a_relaunch_comes_back_to_the_panes_that_were_open() {
        let saved = arranged().layout();

        let mut fresh = App::new(Config::default(), saved);
        fresh.apply_worker_event(WorkerEvent::Rooms(vec![summary("!r:x", "#backend")]));

        let tab = fresh
            .workspaces
            .focused()
            .expect("workspace")
            .focused_tab()
            .expect("tab");
        assert_eq!(tab.panes.len(), 2, "the thread pane must come back");
        assert_eq!(
            tab.panes[1].kind,
            PaneKind::Thread {
                room_id: "!r:x".into(),
                root: "$thread".into()
            }
        );
        // And its timeline is requested, or the pane would come back as an empty box.
        let commands = fresh.take_commands();
        assert!(commands.iter().any(
            |c| matches!(c, Command::OpenView(v) if v.thread_root.as_deref() == Some("$thread"))
        ));
    }

    #[test]
    fn a_relaunch_returns_to_the_room_that_was_focused() {
        let mut app = app_with_two_rooms();
        app.apply_action(Action::NextTab);
        assert_eq!(app.focused_view(), Some(View::room("!b:x")));
        let saved = app.layout();

        let mut fresh = App::new(Config::default(), saved);
        fresh.apply_worker_event(WorkerEvent::Rooms(vec![
            summary("!a:x", "Commons"),
            summary("!b:x", "smith"),
        ]));
        assert_eq!(fresh.focused_view(), Some(View::room("!b:x")));
    }

    #[test]
    fn a_workspace_that_has_not_synced_yet_is_waited_for() {
        // Sync sends rooms over several responses. Giving up after the first one would
        // leave the user in whichever workspace happened to arrive first.
        let mut app = App::new(Config::default(), Layout::default());
        app.apply_worker_event(WorkerEvent::Rooms(vec![
            RoomSummary {
                room_id: "!space:x".into(),
                display_name: "hermes".into(),
                is_space: true,
                ..summary("", "")
            },
            RoomSummary {
                parents: vec!["!space:x".into()],
                ..summary("!inspace:x", "backend")
            },
            summary("!orphan:x", "elsewhere"),
        ]));
        let index = app
            .workspaces
            .items
            .iter()
            .position(|w| w.id == "!space:x")
            .expect("space workspace");
        app.workspaces.focus(index);
        let saved = app.layout();
        assert_eq!(saved.workspace.as_deref(), Some("!space:x"));

        let mut fresh = App::new(Config::default(), saved);
        // First batch: only the orphan room. The saved workspace does not exist yet.
        fresh.apply_worker_event(WorkerEvent::Rooms(vec![summary("!orphan:x", "elsewhere")]));
        assert_eq!(fresh.workspaces.focused().expect("w").id, ORPHAN_WORKSPACE);

        // Second batch brings the Space, and focus follows.
        fresh.apply_worker_event(WorkerEvent::Rooms(vec![
            RoomSummary {
                room_id: "!space:x".into(),
                display_name: "hermes".into(),
                is_space: true,
                ..summary("", "")
            },
            RoomSummary {
                parents: vec!["!space:x".into()],
                ..summary("!inspace:x", "backend")
            },
        ]));
        assert_eq!(fresh.workspaces.focused().expect("w").id, "!space:x");
    }

    #[test]
    fn moving_first_beats_the_restore_to_it() {
        // A late room must not yank the view out from under someone who has already
        // started reading somewhere else.
        let mut app = app_with_two_rooms();
        app.apply_action(Action::NextTab);
        let saved = app.layout();
        assert_eq!(saved.tabs[ORPHAN_WORKSPACE], "!b:x");

        let mut fresh = App::new(Config::default(), saved);
        fresh.apply_worker_event(WorkerEvent::Rooms(vec![summary("!a:x", "Commons")]));
        // The user picks a room before the rest of the list lands.
        fresh.apply_action(Action::NextTab);
        let chosen = fresh.focused_view();

        fresh.apply_worker_event(WorkerEvent::Rooms(vec![summary("!b:x", "smith")]));
        assert_eq!(
            fresh.focused_view(),
            chosen,
            "the restore must yield to the user, not compete with them"
        );
    }

    #[test]
    fn rearranging_marks_the_layout_for_saving() {
        let mut app = app();
        app.layout_dirty = false;
        app.apply_action(Action::Split(Dir::Right));
        assert!(app.layout_dirty);

        app.layout_dirty = false;
        app.apply_action(Action::ResizePane(Dir::Left));
        assert!(app.layout_dirty);

        app.layout_dirty = false;
        app.apply_action(Action::ZoomPane);
        assert!(app.layout_dirty);
    }

    #[test]
    fn a_saved_pane_for_a_room_that_is_gone_costs_nothing() {
        // Left the room, or it was upgraded: the tab never appears, and the entry is
        // simply never claimed.
        let saved = arranged().layout();
        let mut fresh = App::new(Config::default(), saved);
        fresh.apply_worker_event(WorkerEvent::Rooms(vec![summary(
            "!other:x",
            "somewhere else",
        )]));

        let tab = fresh
            .workspaces
            .focused()
            .expect("workspace")
            .focused_tab()
            .expect("tab");
        assert_eq!(tab.room_id, "!other:x");
        assert_eq!(tab.panes.len(), 1);
    }

    /// Two rooms, so there is a tab to switch *to*.
    /// Panes in the focused tab.
    fn pane_count(app: &App) -> usize {
        app.workspaces
            .focused()
            .and_then(|w| w.focused_tab())
            .map_or(0, |t| t.panes.len())
    }

    fn app_with_two_rooms() -> App {
        let mut app = App::new(Config::default(), Layout::default());
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
    fn drafts_survive_switching_room() {
        let mut app = app_with_two_rooms();
        type_into(&mut app, "half a thought");

        app.apply_action(Action::NextTab);
        assert_eq!(
            app.composer().map(Composer::text).unwrap_or(""),
            "",
            "the other room starts blank"
        );

        type_into(&mut app, "different room");
        app.apply_action(Action::PrevTab);
        assert_eq!(
            app.composer().expect("composer").text(),
            "half a thought",
            "coming back must restore the draft, not discard it"
        );
    }

    #[test]
    fn history_is_per_room() {
        let mut app = app_with_two_rooms();
        type_into(&mut app, "said in the first room");
        app.apply_action(Action::Submit);

        app.apply_action(Action::NextTab);
        app.apply_action(Action::CaretUp);
        assert_eq!(
            app.composer().map(Composer::text).unwrap_or(""),
            "",
            "another room's history must not leak in"
        );
    }

    #[test]
    fn up_recalls_the_last_message_once_the_composer_is_flat() {
        let mut app = app();
        type_into(&mut app, "first thing");
        app.apply_action(Action::Submit);
        let _ = app.take_commands();

        app.apply_action(Action::CaretUp);
        assert_eq!(app.composer().expect("composer").text(), "first thing");

        // Down returns to the empty draft that was parked.
        app.apply_action(Action::CaretDown);
        assert_eq!(app.composer().expect("composer").text(), "");
    }

    #[test]
    fn up_moves_between_lines_before_it_touches_history() {
        let mut app = app();
        type_into(&mut app, "sent");
        app.apply_action(Action::Submit);
        let _ = app.take_commands();

        type_into(&mut app, "one\ntwo");
        app.apply_action(Action::CaretUp);
        assert_eq!(
            app.composer().expect("composer").text(),
            "one\ntwo",
            "moving within the message must not recall history"
        );
        assert_eq!(app.composer().expect("composer").wrapped(200).caret.0, 0);
    }

    #[test]
    fn selection_ignores_notices_and_stops_at_the_newest_message() {
        // Notices carry an event id and render a row. Membership changes and redactions
        // arrive after the message that provoked them, so a selection that accepted them
        // walked off the end of the conversation onto rows nothing can be done with.
        let mut app = app();
        app.apply_worker_event(WorkerEvent::Timeline {
            view: View::room("!r:x"),
            entries: vec![
                their_message("$theirs", "hello there"),
                my_message("$mine", "my own words"),
                Entry {
                    id: "$notice".into(),
                    event_id: Some("$notice".into()),
                    kind: EntryKind::Notice("· m.room.member".into()),
                },
            ],
        });
        let _ = app.take_commands();

        app.apply_action(Action::SelectNewer);
        assert_eq!(app.selected_event(), Some("$mine"));

        // Pressing on must not step onto the notice, nor off the end.
        app.apply_action(Action::SelectNewer);
        app.apply_action(Action::SelectNewer);
        assert_eq!(app.selected_event(), Some("$mine"));
    }

    #[test]
    fn selecting_the_newest_message_pins_the_transcript_to_the_bottom() {
        let mut app = app_with_messages();
        app.rendered_lines = 200;
        app.viewport_height = 20;
        let view = app.focused_view().expect("view");
        app.scroll.insert(view.clone(), 120);

        app.apply_action(Action::SelectNewer);

        assert_eq!(app.selected_event(), Some("$mine"));
        assert_eq!(
            *app.scroll.get(&view).unwrap_or(&0),
            0,
            "the newest message means the bottom of the pane, not merely on screen"
        );
    }

    #[test]
    fn selecting_the_oldest_message_scrolls_to_the_top_and_asks_for_more() {
        let mut app = app_with_messages();
        app.rendered_lines = 200;
        app.viewport_height = 20;
        let view = app.focused_view().expect("view");

        app.apply_action(Action::SelectOlder);
        app.apply_action(Action::SelectOlder);

        assert_eq!(app.selected_event(), Some("$theirs"));
        assert_eq!(*app.scroll.get(&view).unwrap_or(&0), app.max_scroll());
        assert!(
            app.take_commands()
                .iter()
                .any(|c| matches!(c, Command::Paginate { view: v, .. } if v == &view)),
            "reaching the oldest loaded message is a request for history"
        );
    }

    #[test]
    fn a_room_with_a_parent_space_lands_in_that_workspace() {
        let mut app = App::new(Config::default(), Layout::default());
        app.apply_worker_event(WorkerEvent::Rooms(vec![
            RoomSummary {
                room_id: "!space:x".into(),
                display_name: "SynthSwarm".into(),
                is_space: true,
                parents: Vec::new(),
                is_direct: false,
                is_encrypted: false,
                notification_count: 0,
                highlight_count: 0,
            },
            RoomSummary {
                room_id: "!commons:x".into(),
                display_name: "Commons".into(),
                is_space: false,
                parents: vec!["!space:x".into()],
                is_direct: false,
                is_encrypted: false,
                notification_count: 0,
                highlight_count: 0,
            },
        ]));

        let workspace = app
            .workspaces
            .items
            .iter()
            .find(|w| w.id == "!space:x")
            .expect("the Space must become a workspace");
        assert_eq!(
            workspace.title, "SynthSwarm",
            "the workspace is named after the Space, not after a room inside it"
        );
        assert!(
            workspace.tabs.iter().any(|t| t.room_id == "!commons:x"),
            "a room listing the Space as a parent belongs to its workspace"
        );
        assert!(
            !app.workspaces
                .items
                .iter()
                .any(|w| w.id == ORPHAN_WORKSPACE),
            "nothing is left over for the orphan workspace"
        );
    }

    #[test]
    fn the_prefix_cycles_workspaces_and_follows_the_focus() {
        let mut app = App::new(Config::default(), Layout::default());
        app.apply_worker_event(WorkerEvent::Rooms(vec![
            RoomSummary {
                room_id: "!space:x".into(),
                display_name: "SynthSwarm".into(),
                is_space: true,
                parents: Vec::new(),
                is_direct: false,
                is_encrypted: false,
                notification_count: 0,
                highlight_count: 0,
            },
            RoomSummary {
                room_id: "!commons:x".into(),
                display_name: "Commons".into(),
                is_space: false,
                parents: vec!["!space:x".into()],
                is_direct: false,
                is_encrypted: false,
                notification_count: 0,
                highlight_count: 0,
            },
            RoomSummary {
                room_id: "!loose:x".into(),
                display_name: "#loose".into(),
                is_space: false,
                parents: Vec::new(),
                is_direct: false,
                is_encrypted: false,
                notification_count: 0,
                highlight_count: 0,
            },
        ]));
        let first = app.workspaces.focused().map(|w| w.id.clone());

        app.apply_action(Action::NextWorkspace);
        let second = app.workspaces.focused().map(|w| w.id.clone());
        assert_ne!(first, second, "the focus must actually move");

        // Two workspaces, so one more wraps back. Switching must also stream the newly
        // focused room, or the pane it lands on stays blank.
        app.apply_action(Action::NextWorkspace);
        assert_eq!(app.workspaces.focused().map(|w| w.id.clone()), first);

        app.apply_action(Action::PrevWorkspace);
        assert_eq!(app.workspaces.focused().map(|w| w.id.clone()), second);
        assert!(
            app.take_commands()
                .iter()
                .any(|c| matches!(c, Command::OpenView(_))),
            "moving to a workspace opens the view it focuses"
        );
    }

    #[test]
    fn the_emoji_picker_reacts_to_the_selected_message() {
        let mut app = app_with_messages();
        app.apply_action(Action::SelectNewer);
        let view = app.focused_view().expect("view");
        let _ = app.take_commands();

        app.apply_action(Action::ReactToSelected);
        assert!(app.emoji().is_some(), "the picker must open");

        for c in "rocket".chars() {
            app.apply_action(Action::Insert(c));
        }
        app.apply_action(Action::Accept);

        assert!(app.emoji().is_none(), "accepting closes the picker");
        match app.take_commands().as_slice() {
            [Command::ToggleReaction {
                view: v,
                event_id,
                key,
            }] => {
                assert_eq!(v, &view);
                assert_eq!(event_id, "$mine");
                assert_eq!(key, "🚀");
            }
            other => panic!("expected one ToggleReaction, got {other:?}"),
        }
    }

    #[test]
    fn reacting_needs_a_selection() {
        let mut app = app_with_messages();
        app.apply_action(Action::ReactToSelected);

        assert!(app.emoji().is_none());
        assert!(app.take_commands().is_empty());
        assert!(app.status.is_some(), "and says why");
    }

    #[test]
    fn the_emoji_picker_inserts_into_the_composer_at_the_caret() {
        let mut app = app_with_messages();
        app.apply_action(Action::EnterInsert);
        for c in "ship it ".chars() {
            app.apply_action(Action::Insert(c));
        }

        app.apply_action(Action::EmojiIntoComposer);
        for c in "rocket".chars() {
            app.apply_action(Action::Insert(c));
        }
        app.apply_action(Action::Submit);

        assert!(app.emoji().is_none());
        assert_eq!(app.composer().expect("composer").text(), "ship it 🚀");
        assert_eq!(
            app.mode,
            Mode::Insert,
            "choosing an emoji is part of writing the message, so typing continues"
        );
        assert!(
            app.take_commands().is_empty(),
            "accepting into the composer must not also send the message"
        );
    }

    #[test]
    fn the_emoji_picker_swallows_the_keys_underneath_it() {
        // Without this the search box doubles as a command stream: typing "e" to find
        // "eyes" would also be editing a message underneath.
        let mut app = app_with_messages();
        app.apply_action(Action::SelectNewer);
        let before = app.workspaces.focused().map(|w| w.id.clone());
        let _ = app.take_commands();

        app.apply_action(Action::EmojiIntoComposer);
        app.apply_action(Action::NextWorkspace);
        app.apply_action(Action::RedactMessage);

        assert!(app.emoji().is_some(), "the picker stays open");
        assert_eq!(app.workspaces.focused().map(|w| w.id.clone()), before);
        assert!(app.take_commands().is_empty());
    }

    #[test]
    fn escape_closes_the_emoji_picker_without_acting() {
        let mut app = app_with_messages();
        app.apply_action(Action::SelectNewer);
        let _ = app.take_commands();

        app.apply_action(Action::ReactToSelected);
        app.apply_action(Action::Cancel);

        assert!(app.emoji().is_none());
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.take_commands().is_empty());
    }

    #[test]
    fn typing_is_announced_once_and_refreshed_while_it_continues() {
        let mut app = app_with_messages();
        let _ = app.take_commands();

        app.apply_action(Action::EnterInsert);
        app.apply_action(Action::Insert('h'));
        app.tick_typing(1_000);
        match app.take_commands().as_slice() {
            [Command::SendTyping { room_id, typing }] => {
                assert_eq!(room_id, "!r:x");
                assert!(*typing);
            }
            other => panic!("expected one SendTyping, got {other:?}"),
        }

        // More typing inside the refresh window says nothing further: a notice per
        // keystroke would be a request per character.
        app.apply_action(Action::Insert('i'));
        app.tick_typing(1_500);
        assert!(app.take_commands().is_empty());

        // Past it, the notice is re-asserted, because the server lets it lapse.
        app.apply_action(Action::Insert('!'));
        app.tick_typing(4_100);
        assert!(matches!(
            app.take_commands().as_slice(),
            [Command::SendTyping { typing: true, .. }]
        ));
    }

    #[test]
    fn typing_stops_by_itself_once_the_keyboard_goes_quiet() {
        let mut app = app_with_messages();
        app.apply_action(Action::EnterInsert);
        app.apply_action(Action::Insert('h'));
        app.tick_typing(1_000);
        let _ = app.take_commands();

        app.tick_typing(1_000 + TYPING_IDLE_MS - 1);
        assert!(app.take_commands().is_empty(), "not yet idle");

        app.tick_typing(1_000 + TYPING_IDLE_MS);
        assert!(matches!(
            app.take_commands().as_slice(),
            [Command::SendTyping { typing: false, .. }]
        ));

        // And having said so once, it does not keep saying it.
        app.tick_typing(99_000);
        assert!(app.take_commands().is_empty());
    }

    #[test]
    fn sending_stops_typing_without_waiting_for_the_timer() {
        // The agent would otherwise see the message arrive while we still claim to be
        // typing it.
        let mut app = app_with_messages();
        app.apply_action(Action::EnterInsert);
        app.apply_action(Action::Insert('h'));
        app.tick_typing(1_000);
        let _ = app.take_commands();

        app.apply_action(Action::Submit);

        let commands = app.take_commands();
        let stop = commands
            .iter()
            .position(|c| matches!(c, Command::SendTyping { typing: false, .. }))
            .expect("sending must stop the notice");
        let sent = commands
            .iter()
            .position(|c| matches!(c, Command::SendMessage { .. }))
            .expect("and still send the message");
        assert!(stop < sent, "the stop goes first");
    }

    #[test]
    fn leaving_the_composer_or_the_room_stops_typing() {
        for action in [Action::Cancel, Action::NextWorkspace] {
            let mut app = app_with_messages();
            app.apply_action(Action::EnterInsert);
            app.apply_action(Action::Insert('h'));
            app.tick_typing(1_000);
            let _ = app.take_commands();

            app.apply_action(action.clone());

            assert!(
                app.take_commands()
                    .iter()
                    .any(|c| matches!(c, Command::SendTyping { typing: false, .. })),
                "{action:?} must stop the notice"
            );
        }
    }

    #[test]
    fn moving_the_caret_is_not_typing() {
        // Reading back what you wrote should not re-announce anything.
        let mut app = app_with_messages();
        app.apply_action(Action::EnterInsert);
        app.apply_action(Action::Insert('h'));
        app.tick_typing(1_000);
        let _ = app.take_commands();

        app.apply_action(Action::CaretLeft);
        app.apply_action(Action::CaretRight);
        app.tick_typing(1_100);
        assert!(app.take_commands().is_empty());
    }

    #[test]
    fn starting_a_thread_opens_a_pane_on_a_message_with_no_replies() {
        // The whole point: the selected message has no thread yet. Requiring one would
        // mean threads could only ever be opened, never started.
        let mut app = app_with_messages();
        app.apply_action(Action::SelectNewer);
        let _ = app.take_commands();

        app.apply_action(Action::NewThread);

        assert_eq!(
            app.focused_view(),
            Some(View::thread("!r:x", "$mine")),
            "focus moves into the new thread, ready to type"
        );
        assert!(
            app.take_commands()
                .iter()
                .any(|c| matches!(c, Command::OpenView(v) if v == &View::thread("!r:x", "$mine"))),
            "and the thread's timeline is streamed"
        );
    }

    #[test]
    fn starting_a_thread_sends_nothing_by_itself() {
        // Opening the pane must not post anything: an abandoned thread pane should leave
        // no trace in the room.
        let mut app = app_with_messages();
        app.apply_action(Action::SelectNewer);
        let _ = app.take_commands();

        app.apply_action(Action::NewThread);

        assert!(
            !app.take_commands()
                .iter()
                .any(|c| matches!(c, Command::SendMessage { .. } | Command::SendReply { .. })),
            "nothing is sent until the user types something"
        );
    }

    #[test]
    fn starting_a_thread_twice_focuses_the_pane_already_open() {
        let mut app = app_with_messages();
        app.apply_action(Action::SelectNewer);
        app.apply_action(Action::NewThread);
        let panes = app
            .workspaces
            .focused()
            .and_then(|w| w.focused_tab())
            .map_or(0, |t| t.panes.len());

        app.apply_action(Action::NewThread);

        assert_eq!(
            app.workspaces
                .focused()
                .and_then(|w| w.focused_tab())
                .map_or(0, |t| t.panes.len()),
            panes,
            "a second attempt must not grow another pane onto the same thread"
        );
    }

    #[test]
    fn a_thread_cannot_be_started_inside_a_thread() {
        let mut app = app_with_messages();
        app.apply_action(Action::SelectNewer);
        app.apply_action(Action::NewThread);
        let _ = app.take_commands();

        app.apply_action(Action::NewThread);

        assert_eq!(app.focused_view(), Some(View::thread("!r:x", "$mine")));
        assert!(app.status.is_some(), "and says why");
    }

    #[test]
    fn starting_a_thread_needs_a_selection() {
        let mut app = app_with_messages();
        app.apply_action(Action::NewThread);

        assert_eq!(app.focused_view(), Some(View::room("!r:x")));
        assert!(app.status.is_some());
    }

    fn comparing() -> Verification {
        Verification::Compare {
            other_device: "Element X Android".into(),
            emoji: vec![("🐶".into(), "Dog".into()), ("🎂".into(), "Cake".into())],
        }
    }

    #[test]
    fn accepting_a_request_does_not_confirm_the_keys() {
        let mut app = app();
        app.apply_worker_event(WorkerEvent::Verification(Verification::Requested {
            other_device: "phone".into(),
        }));
        app.apply_action(Action::Approve);

        // Agreeing to compare is not agreeing that they matched.
        assert!(matches!(
            app.take_commands().as_slice(),
            [Command::AcceptVerification]
        ));
    }

    #[test]
    fn yes_at_the_emoji_confirms_and_no_reports_a_mismatch() {
        let mut confirming = app();
        confirming.apply_worker_event(WorkerEvent::Verification(comparing()));
        confirming.apply_action(Action::Approve);
        assert!(matches!(
            confirming.take_commands().as_slice(),
            [Command::ConfirmVerification]
        ));

        let mut app = app();
        app.apply_worker_event(WorkerEvent::Verification(comparing()));
        app.apply_action(Action::Deny);
        assert!(
            matches!(
                app.take_commands().as_slice(),
                [Command::MismatchVerification]
            ),
            "`no` must report a mismatch, not withdraw quietly: the other side needs to \
             know its keys were rejected"
        );
    }

    #[test]
    fn the_verification_panel_swallows_everything_else() {
        let mut app = app();
        let before = format!("{:?}", app.workspaces);
        app.apply_worker_event(WorkerEvent::Verification(comparing()));

        // Every one of these would otherwise do something irreversible or confusing
        // underneath a prompt the user believes is a yes/no question.
        for action in [
            Action::Split(Dir::Right),
            Action::ClosePane,
            Action::NewThread,
            Action::Submit,
            Action::EnterInsert,
            Action::NextTab,
        ] {
            app.apply_action(action);
        }

        assert!(app.take_commands().is_empty(), "no command may escape");
        assert_eq!(
            format!("{:?}", app.workspaces),
            before,
            "the layout must be untouched"
        );
        assert!(matches!(
            app.verification().cloned(),
            Some(Verification::Compare { .. })
        ));
    }

    #[test]
    fn a_finished_verification_clears_the_panel() {
        let mut app = app();
        app.apply_worker_event(WorkerEvent::Verification(comparing()));
        app.apply_worker_event(WorkerEvent::Verification(Verification::Done));

        assert!(app.verification().is_none(), "good news needs no dialog");
        assert_eq!(app.device_verified, Some(true));
        assert!(app.status.is_some());
    }

    #[test]
    fn a_cancelled_verification_says_why() {
        let mut app = app();
        app.apply_worker_event(WorkerEvent::Verification(comparing()));
        app.apply_worker_event(WorkerEvent::Verification(Verification::Cancelled {
            reason: "m.mismatched_sas".into(),
        }));

        assert!(app.verification().is_none());
        assert!(
            app.status
                .as_deref()
                .is_some_and(|s| s.contains("m.mismatched_sas")),
            "the reason is the whole point: a mismatch is not the same as a withdrawal"
        );
    }

    #[test]
    fn verifying_an_already_verified_device_asks_for_nothing() {
        let mut app = app();
        app.apply_worker_event(WorkerEvent::DeviceVerified(Some(true)));
        app.apply_action(Action::StartVerification);

        assert!(app.take_commands().is_empty());
        assert!(app.status.is_some());
    }

    #[test]
    fn an_unverified_device_can_ask_to_be_verified() {
        let mut app = app();
        app.apply_worker_event(WorkerEvent::DeviceVerified(Some(false)));
        app.apply_action(Action::StartVerification);

        assert!(matches!(
            app.take_commands().as_slice(),
            [Command::StartVerification]
        ));
    }

    #[test]
    fn not_knowing_yet_does_not_block_verifying() {
        let mut app = app();
        // "unknown" is not "verified". Conflating them is what made heddle announce
        // "this device is already verified" and refuse to start the flow, for a device
        // the server held no signature for at all.
        app.apply_worker_event(WorkerEvent::DeviceVerified(None));
        app.apply_action(Action::StartVerification);

        assert!(matches!(
            app.take_commands().as_slice(),
            [Command::StartVerification]
        ));
    }

    #[test]
    fn an_unknown_answer_does_not_erase_a_known_one() {
        let mut app = app();
        app.apply_worker_event(WorkerEvent::DeviceVerified(Some(true)));
        app.apply_worker_event(WorkerEvent::DeviceVerified(None));

        assert_eq!(
            app.device_verified,
            Some(true),
            "a shield must not flicker off because the store went quiet"
        );
    }

    #[test]
    fn a_recovery_key_never_reaches_a_room() {
        let mut app = app();
        app.apply_worker_event(WorkerEvent::Recovery(RecoveryState::Incomplete));
        app.apply_action(Action::OpenRecovery);
        assert!(app.recovery_prompt().is_some());

        for c in "EsTc 1234".chars() {
            app.apply_action(Action::Insert(c));
        }
        // Every one of these sends, splits or switches. None may fire while a secret is
        // half-typed.
        app.apply_action(Action::NewThread);
        app.apply_action(Action::Reply);
        app.apply_action(Action::NextTab);

        assert!(
            app.take_commands().is_empty(),
            "nothing may escape the prompt before the key is submitted"
        );
        assert!(matches!(
            app.recovery_prompt(),
            Some(RecoveryPanel::AskKey { key, .. }) if key == "EsTc 1234"
        ));
    }

    #[test]
    fn submitting_the_key_sends_it_once_and_clears_it() {
        let mut app = app();
        app.apply_worker_event(WorkerEvent::Recovery(RecoveryState::Incomplete));
        app.apply_action(Action::OpenRecovery);
        for c in "secret".chars() {
            app.apply_action(Action::Insert(c));
        }
        app.apply_action(Action::Submit);

        match app.take_commands().as_slice() {
            [Command::RecoverWithKey(key)] => assert_eq!(key, "secret"),
            other => panic!("expected one recover command, got {other:?}"),
        }
        assert!(
            matches!(
                app.recovery_prompt(),
                Some(RecoveryPanel::AskKey { key, submitted: true, .. }) if key.is_empty()
            ),
            "the key must not be left sitting in the prompt after being sent"
        );
    }

    #[test]
    fn cancelling_drops_a_half_typed_key() {
        let mut app = app();
        app.apply_worker_event(WorkerEvent::Recovery(RecoveryState::Incomplete));
        app.apply_action(Action::OpenRecovery);
        for c in "half".chars() {
            app.apply_action(Action::Insert(c));
        }
        app.apply_action(Action::Cancel);

        assert!(app.recovery_prompt().is_none());
        assert!(app.take_commands().is_empty());
    }

    #[test]
    fn an_account_with_no_recovery_is_offered_it_rather_than_asked_for_a_key() {
        let mut app = app();
        app.apply_worker_event(WorkerEvent::Recovery(RecoveryState::Disabled));
        app.apply_action(Action::OpenRecovery);

        assert_eq!(
            app.recovery_prompt().cloned(),
            Some(RecoveryPanel::OfferEnable),
            "asking for a key that was never created sends the user hunting for nothing"
        );
        assert!(
            app.take_commands().is_empty(),
            "nothing happens until they agree"
        );

        app.apply_action(Action::Approve);
        assert!(matches!(
            app.take_commands().as_slice(),
            [Command::EnableRecovery]
        ));
    }

    #[test]
    fn replacing_a_working_recovery_key_needs_an_explicit_yes() {
        let mut app = app();
        app.apply_worker_event(WorkerEvent::Recovery(RecoveryState::Enabled));
        app.apply_action(Action::OpenRecovery);
        assert_eq!(
            app.recovery_prompt().cloned(),
            Some(RecoveryPanel::ConfirmReset)
        );

        // Enter is what a user presses to dismiss a dialog they have not read. It must
        // not be what destroys the key every other device is holding.
        app.apply_action(Action::Submit);
        assert!(app.take_commands().is_empty());
        assert_eq!(
            app.recovery_prompt().cloned(),
            Some(RecoveryPanel::ConfirmReset)
        );

        app.apply_action(Action::Approve);
        assert!(matches!(
            app.take_commands().as_slice(),
            [Command::ResetRecoveryKey]
        ));
    }

    #[test]
    fn declining_a_reset_leaves_the_key_alone() {
        let mut app = app();
        app.apply_worker_event(WorkerEvent::Recovery(RecoveryState::Enabled));
        app.apply_action(Action::OpenRecovery);
        app.apply_action(Action::Deny);

        assert!(app.recovery_prompt().is_none());
        assert!(app.take_commands().is_empty());
    }

    #[test]
    fn a_new_key_is_shown_and_waits_to_be_acknowledged() {
        let mut app = app();
        app.apply_worker_event(WorkerEvent::Recovery(RecoveryState::Disabled));
        app.apply_action(Action::OpenRecovery);
        app.apply_action(Action::Approve);
        let _ = app.take_commands();

        app.apply_worker_event(WorkerEvent::RecoveryKeyCreated("EsTc AAAA BBBB".into()));

        assert_eq!(
            app.recovery_prompt().cloned(),
            Some(RecoveryPanel::ShowKey {
                key: "EsTc AAAA BBBB".into()
            })
        );
        assert!(
            app.status.as_deref() != Some("EsTc AAAA BBBB"),
            "the key must not be copied into the status line, which outlives the panel"
        );

        app.apply_action(Action::Accept);
        assert!(app.recovery_prompt().is_none());
    }

    /// A plain message from someone else.
    fn their_message(event_id: &str, body: &str) -> Entry {
        Entry {
            id: event_id.into(),
            event_id: Some(event_id.into()),
            kind: EntryKind::Message(Message {
                shield: Shield::None,
                sender: "@someone:x".into(),
                sender_display: "someone".into(),
                body: body.into(),
                timestamp: 0,
                is_own: false,
                is_edited: false,
                thread_root: None,
                thread_replies: None,
                reactions: Vec::new(),
                agent: AgentPayload::None,
            }),
        }
    }

    /// A plain message from the local user.
    fn my_message(event_id: &str, body: &str) -> Entry {
        let mut entry = their_message(event_id, body);
        if let EntryKind::Message(m) = &mut entry.kind {
            m.is_own = true;
            m.sender_display = "quintin".into();
        }
        entry
    }

    /// An app with two messages loaded in the focused room.
    fn app_with_messages() -> App {
        let mut app = app();
        app.apply_worker_event(WorkerEvent::Timeline {
            view: View::room("!r:x"),
            entries: vec![
                their_message("$theirs", "hello there"),
                my_message("$mine", "my own words"),
            ],
        });
        let _ = app.take_commands();
        app
    }

    #[test]
    fn selection_falls_back_to_scrolling_when_there_is_nothing_to_select() {
        // An empty or still-loading room must not swallow the keypress and feel dead.
        let mut app = app();
        app.rendered_lines = 200;
        app.viewport_height = 20;
        let view = app.focused_view().expect("view");

        app.apply_action(Action::SelectOlder);
        assert_eq!(*app.scroll.get(&view).unwrap_or(&0), 1);
        assert_eq!(app.selected_event(), None);
    }

    #[test]
    fn selection_starts_at_the_newest_message_and_walks_back() {
        let mut app = app_with_messages();
        assert_eq!(app.selected_event(), None);

        app.apply_action(Action::SelectOlder);
        assert_eq!(
            app.selected_event(),
            Some("$mine"),
            "the first move selects the newest, which is what `up` means in a chat"
        );

        app.apply_action(Action::SelectOlder);
        assert_eq!(app.selected_event(), Some("$theirs"));

        app.apply_action(Action::SelectOlder);
        assert_eq!(
            app.selected_event(),
            Some("$theirs"),
            "selection must clamp at the oldest rather than wrap"
        );

        app.apply_action(Action::SelectNewer);
        assert_eq!(app.selected_event(), Some("$mine"));
    }

    #[test]
    fn replying_sends_a_reply_not_a_message() {
        let mut app = app_with_messages();
        app.apply_action(Action::SelectOlder);
        app.apply_action(Action::Reply);
        assert_eq!(app.composing, Some(Pending::Reply("$mine".into())));

        type_into(&mut app, "quite so");
        app.apply_action(Action::Submit);

        match app.take_commands().as_slice() {
            [Command::SendReply {
                in_reply_to, body, ..
            }] => {
                assert_eq!(in_reply_to, "$mine");
                assert_eq!(body, "quite so");
            }
            other => panic!("expected SendReply, got {other:?}"),
        }
        assert_eq!(app.composing, None, "the reply target must not persist");
    }

    #[test]
    fn editing_loads_the_message_and_sends_a_replacement() {
        let mut app = app_with_messages();
        app.apply_action(Action::SelectOlder);
        app.apply_action(Action::EditMessage);

        assert_eq!(
            app.composer().expect("composer").text(),
            "my own words",
            "editing must load the existing text, not start blank"
        );

        type_into(&mut app, "!");
        app.apply_action(Action::Submit);
        match app.take_commands().as_slice() {
            [Command::Edit { event_id, body, .. }] => {
                assert_eq!(event_id, "$mine");
                assert_eq!(body, "my own words!");
            }
            other => panic!("expected Edit, got {other:?}"),
        }
    }

    #[test]
    fn other_peoples_messages_cannot_be_edited() {
        let mut app = app_with_messages();
        app.apply_action(Action::SelectOlder);
        app.apply_action(Action::SelectOlder);
        assert_eq!(app.selected_event(), Some("$theirs"));

        app.apply_action(Action::EditMessage);
        assert_eq!(app.composing, None, "no edit may be armed");
        assert!(app.status.as_deref().is_some_and(|s| s.contains("own")));
        assert!(app.composer().map(Composer::text).unwrap_or("").is_empty());
    }

    #[test]
    fn redaction_needs_confirming() {
        let mut app = app_with_messages();
        app.apply_action(Action::SelectOlder);

        app.apply_action(Action::RedactMessage);
        assert!(
            app.take_commands().is_empty(),
            "one keypress must not delete anything"
        );

        app.apply_action(Action::RedactMessage);
        match app.take_commands().as_slice() {
            [Command::Redact { event_id, .. }] => assert_eq!(event_id, "$mine"),
            other => panic!("expected Redact, got {other:?}"),
        }
    }

    #[test]
    fn escape_disarms_a_redaction() {
        let mut app = app_with_messages();
        app.apply_action(Action::SelectOlder);
        app.apply_action(Action::RedactMessage);
        app.apply_action(Action::Cancel);

        app.apply_action(Action::RedactMessage);
        assert!(
            app.take_commands().is_empty(),
            "cancelling must reset the confirmation, not leave it armed"
        );
    }

    #[test]
    fn escape_abandons_a_reply_and_clears_the_draft() {
        let mut app = app_with_messages();
        app.apply_action(Action::SelectOlder);
        app.apply_action(Action::Reply);
        type_into(&mut app, "never mind");

        app.apply_action(Action::Cancel);
        assert_eq!(app.composing, None);
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.composer().map(Composer::text).unwrap_or("").is_empty());
    }

    #[test]
    fn the_thread_picker_asks_the_worker_and_opens_a_pane() {
        let mut app = app();
        app.apply_action(Action::OpenThreads);
        match app.take_commands().as_slice() {
            [Command::ListThreads { room_id }] => assert_eq!(room_id, "!r:x"),
            other => panic!("expected ListThreads, got {other:?}"),
        }
        assert!(app.threads().expect("picker").loading);

        app.apply_worker_event(WorkerEvent::Threads {
            room_id: "!r:x".into(),
            threads: vec![ThreadSummary {
                root_event_id: "$root".into(),
                sender_display: "hermes".into(),
                preview: "fix auth".into(),
                timestamp: 0,
            }],
        });
        assert!(!app.threads().expect("picker").loading);

        let panes_before = app
            .workspaces
            .focused()
            .and_then(|w| w.focused_tab())
            .map_or(0, |t| t.panes.len());

        app.apply_action(Action::Accept);
        assert!(app.threads().is_none(), "opening must close the picker");

        let tab = app
            .workspaces
            .focused()
            .expect("workspace")
            .focused_tab()
            .expect("tab");
        assert_eq!(tab.panes.len(), panes_before + 1);
        assert_eq!(
            app.focused_view(),
            Some(View::thread("!r:x", "$root")),
            "a thread opens as its own pane, focused"
        );
    }

    #[test]
    fn a_stale_thread_answer_is_ignored() {
        let mut app = app();
        app.apply_action(Action::OpenThreads);
        let _ = app.take_commands();

        app.apply_worker_event(WorkerEvent::Threads {
            room_id: "!somewhere-else:x".into(),
            threads: vec![ThreadSummary {
                root_event_id: "$x".into(),
                sender_display: "x".into(),
                preview: "x".into(),
                timestamp: 0,
            }],
        });
        let picker = app.threads().expect("picker");
        assert!(picker.threads.is_empty());
        assert!(
            picker.loading,
            "a late answer for another room must not land"
        );
    }

    /// A message that roots a thread.
    fn thread_root_message(event_id: &str, body: &str, replies: u32) -> Entry {
        let mut entry = their_message(event_id, body);
        if let EntryKind::Message(m) = &mut entry.kind {
            m.thread_replies = Some(replies);
        }
        entry
    }

    #[test]
    fn enter_opens_the_thread_on_the_selected_message() {
        let mut app = app();
        app.apply_worker_event(WorkerEvent::Timeline {
            view: View::room("!r:x"),
            entries: vec![thread_root_message("$root", "fix auth", 3)],
        });
        let _ = app.take_commands();

        app.apply_action(Action::SelectOlder);
        app.apply_action(Action::Accept);

        assert_eq!(
            app.focused_view(),
            Some(View::thread("!r:x", "$root")),
            "enter must open the thread as a focused pane"
        );
    }

    #[test]
    fn enter_on_a_message_with_no_thread_says_so() {
        let mut app = app_with_messages();
        app.apply_action(Action::SelectOlder);
        app.apply_action(Action::Accept);
        assert!(app
            .status
            .as_deref()
            .is_some_and(|s| s.contains("no thread")));
        assert_eq!(app.focused_view(), Some(View::room("!r:x")));
    }

    #[test]
    fn a_message_inside_a_thread_is_also_a_way_in() {
        let mut app = app();
        let mut reply = their_message("$reply", "a reply");
        if let EntryKind::Message(m) = &mut reply.kind {
            m.thread_root = Some("$root".into());
        }
        app.apply_worker_event(WorkerEvent::Timeline {
            view: View::room("!r:x"),
            entries: vec![reply],
        });
        let _ = app.take_commands();

        app.apply_action(Action::SelectOlder);
        app.apply_action(Action::Accept);
        assert_eq!(app.focused_view(), Some(View::thread("!r:x", "$root")));
    }

    #[test]
    fn the_first_thread_splits_beside_the_room_and_later_ones_stack() {
        let mut app = app();
        app.open_thread_pane("$a".into(), "first".into());
        app.open_thread_pane("$b".into(), "second".into());

        let tab = app
            .workspaces
            .focused()
            .expect("workspace")
            .focused_tab()
            .expect("tab");
        assert_eq!(tab.panes.len(), 3, "room plus two threads");

        // Splitting the room pane again would squeeze the transcript towards nothing
        // while the threads stayed wide, so the second thread stacks under the first.
        let room = tab
            .panes
            .iter()
            .find(|p| p.kind.thread_root().is_none())
            .expect("room pane");
        let tiling = app.tilings.get_mut("!r:x").expect("tiling");
        let placements = tiling.layout(ratatui::layout::Rect::new(0, 0, 100, 100));
        let room_rect = placements
            .iter()
            .find(|p| p.id == room.id)
            .expect("room placement")
            .rect;
        assert!(
            room_rect.width < 100,
            "the room pane gives up width to the first thread"
        );
        assert_eq!(
            room_rect.height, 100,
            "but keeps its full height, because later threads stack instead"
        );
    }

    #[test]
    fn reopening_a_thread_focuses_it_rather_than_duplicating_it() {
        let mut app = app();
        app.open_thread_pane("$a".into(), "first".into());
        let after_first = app
            .workspaces
            .focused()
            .and_then(|w| w.focused_tab())
            .map_or(0, |t| t.panes.len());

        app.open_thread_pane("$a".into(), "first".into());
        let after_second = app
            .workspaces
            .focused()
            .and_then(|w| w.focused_tab())
            .map_or(0, |t| t.panes.len());

        assert_eq!(
            after_first, after_second,
            "no second pane on the same thread"
        );
        assert_eq!(app.focused_view(), Some(View::thread("!r:x", "$a")));
    }

    #[test]
    fn the_last_pane_cannot_be_closed() {
        let mut app = app();
        app.apply_action(Action::ClosePane);
        let tab = app
            .workspaces
            .focused()
            .expect("workspace")
            .focused_tab()
            .expect("tab");
        assert_eq!(
            tab.panes.len(),
            1,
            "an empty tab shows nothing and offers no way back"
        );
    }

    #[test]
    fn closing_a_thread_pane_leaves_the_room_pane() {
        let mut app = app();
        app.open_thread_pane("$a".into(), "first".into());
        app.apply_action(Action::ClosePane);

        let tab = app
            .workspaces
            .focused()
            .expect("workspace")
            .focused_tab()
            .expect("tab");
        assert_eq!(tab.panes.len(), 1);
        assert_eq!(app.focused_view(), Some(View::room("!r:x")));
    }

    #[test]
    fn every_pane_on_show_gets_its_own_timeline() {
        // A background pane renders its own transcript, so it needs its own stream.
        // Streaming only the focused view is what left the room pane blank the moment a
        // thread took focus.
        let mut app = app();
        app.apply_worker_event(WorkerEvent::Timeline {
            view: View::room("!r:x"),
            entries: Vec::new(),
        });
        let _ = app.take_commands();

        app.open_thread_pane("$root".into(), "a thread".into());

        let commands = app.take_commands();
        assert!(
            commands
                .iter()
                .any(|c| matches!(c, Command::OpenView(v) if v == &View::thread("!r:x", "$root"))),
            "the new thread pane must be opened: {commands:?}"
        );

        // And the room pane, already streaming, must not be re-requested.
        assert!(
            !commands
                .iter()
                .any(|c| matches!(c, Command::OpenView(v) if v == &View::room("!r:x"))),
            "an already-streaming view must not be reopened"
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
        let mut app = App::new(Config::default(), Layout::default());
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
        assert!(!matches!(app.modal, Some(Modal::Help)));
        app.apply_action(Action::ToggleHelp);
        assert!(matches!(app.modal, Some(Modal::Help)));
        app.apply_action(Action::ToggleHelp);
        assert!(!matches!(app.modal, Some(Modal::Help)));

        app.apply_action(Action::ToggleHelp);
        app.apply_action(Action::Cancel);
        assert!(
            !matches!(app.modal, Some(Modal::Help)),
            "escape must close the overlay"
        );
    }

    #[test]
    fn a_second_overlay_cannot_open_over_the_first() {
        // The bug this makes impossible. The thread picker's match had a `_ => {}` arm,
        // so `:` fell through it into the main keymap and opened the palette -- leaving
        // both on screen, both drawn, with the palette checked first so the picker
        // underneath was unreachable and even its own escape was eaten.
        //
        // Now the picker owns the key and nothing happens, which is also the better
        // answer: the user gets the overlay they already asked for.
        let mut app = app_with_two_rooms();
        app.apply_action(Action::OpenThreads);
        assert!(app.threads().is_some());

        app.apply_action(Action::CommandPalette);

        assert!(app.palette().is_none(), "no palette opened over the picker");
        assert!(app.threads().is_some(), "and the picker is still reachable");
    }

    #[test]
    fn the_thread_picker_swallows_keys_that_would_act_behind_it() {
        // `_ => {}` used to let everything it did not recognise fall through to the
        // main keymap, so a picker could split the pane it was covering.
        let mut app = app_with_two_rooms();
        app.apply_action(Action::OpenThreads);
        let before = pane_count(&app);

        app.apply_action(Action::Split(heddle_layout::Dir::Right));

        assert_eq!(pane_count(&app), before, "the split must not have happened");
        assert!(app.threads().is_some(), "and the picker is still up");
    }

    #[test]
    fn the_key_overlay_swallows_keys_that_would_act_behind_it() {
        // The help overlay was drawn but appeared in no dispatch chain at all, so `D`
        // still armed a redaction and a split still split, behind a panel covering the
        // transcript they were acting on.
        let mut app = app_with_two_rooms();
        app.apply_action(Action::ToggleHelp);
        let before = pane_count(&app);

        app.apply_action(Action::Split(heddle_layout::Dir::Right));

        assert_eq!(pane_count(&app), before);
        assert!(matches!(app.modal, Some(Modal::Help)));
    }

    #[test]
    fn moving_focus_forces_a_full_repaint() {
        // ratatui diffs against what it believes is on screen. A glyph that paints
        // wider than it measured breaks that belief, and the old room's text is left
        // stranded under the new one.
        let mut app = app_with_two_rooms();
        app.needs_redraw = false;

        app.apply_action(Action::NextTab);
        assert!(app.needs_redraw, "switching tab must repaint");

        app.needs_redraw = false;
        app.bars.tab_row = 1;
        app.bars.tabs = vec![Hit {
            x0: 1,
            x1: 13,
            index: 0,
        }];
        app.click_bar(5, 1);
        assert!(app.needs_redraw, "clicking a tab must repaint");
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
