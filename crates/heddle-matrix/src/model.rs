//! Plain data crossing the worker boundary.
//!
//! These types deliberately contain no `matrix_sdk::Client` and no SDK handles, so the
//! render thread cannot accidentally perform network or crypto work while drawing a
//! frame. Agent decoding also happens on the worker, keeping parsing off the hot path.

use heddle_agent::{fallback, AgentEvent};

/// Where the sync loop is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncState {
    #[default]
    Idle,
    /// First sync in progress; the room list is not yet meaningful.
    Initial,
    Running,
    /// Sync stopped and will be retried.
    Offline,
    Terminated,
}

/// A room as shown in the tab bar and switcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomSummary {
    pub room_id: String,
    pub display_name: String,
    /// `true` when this is a Space, which heddle maps to a workspace rather than a tab.
    pub is_space: bool,
    /// Space room IDs this room is a child of. Empty means the implicit `~` workspace.
    pub parents: Vec<String>,
    pub is_direct: bool,
    pub is_encrypted: bool,
    pub notification_count: u64,
    pub highlight_count: u64,
}

/// Whether an entry carried agent structure, and at what fidelity.
#[derive(Debug, Clone)]
pub enum AgentPayload {
    /// Full fidelity, from the `dev.hermes.agent.v1` extension.
    Structured(Box<AgentEvent>),
    /// Recovered from human-readable chrome. Lossy; surfaced with a `~` marker.
    Degraded(fallback::Parsed),
    /// An ordinary message.
    None,
}

impl AgentPayload {
    pub fn is_agent(&self) -> bool {
        !matches!(self, Self::None)
    }

    pub fn is_degraded(&self) -> bool {
        matches!(self, Self::Degraded(_))
    }
}

/// One rendered row of a timeline.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Stable across edits, so a streaming message keeps its position.
    pub id: String,
    pub event_id: Option<String>,
    pub kind: EntryKind,
}

#[derive(Debug, Clone)]
pub enum EntryKind {
    Message(Message),
    /// A day boundary.
    DateDivider(u64),
    /// The user's own read marker.
    ReadMarker,
    /// Start of the timeline; nothing older exists.
    TimelineStart,
    /// An encrypted event that could not be decrypted. Retried in the background.
    UnableToDecrypt,
    /// State changes, membership, redactions and anything else not worth a full row.
    Notice(String),
}

#[derive(Debug, Clone)]
pub struct Message {
    pub sender: String,
    pub sender_display: String,
    pub body: String,
    /// Milliseconds since the Unix epoch.
    pub timestamp: u64,
    pub is_own: bool,
    pub is_edited: bool,
    /// Event ID of the thread root, when this message is in a thread. This is the key
    /// that maps a message to a heddle pane.
    pub thread_root: Option<String>,
    /// Reaction key to the number of senders who used it.
    pub reactions: Vec<(String, usize)>,
    pub agent: AgentPayload,
}

/// Which timeline a pane is showing.
///
/// This is the key that ties a heddle pane to a Matrix timeline and, for threads, to a
/// Hermes agent session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct View {
    pub room_id: String,
    /// Event ID of the thread root. `None` means the room's main timeline.
    pub thread_root: Option<String>,
}

impl View {
    /// The room's main timeline, with threaded events hidden.
    pub fn room(room_id: impl Into<String>) -> Self {
        Self {
            room_id: room_id.into(),
            thread_root: None,
        }
    }

    /// One thread: a single agent session.
    pub fn thread(room_id: impl Into<String>, root: impl Into<String>) -> Self {
        Self {
            room_id: room_id.into(),
            thread_root: Some(root.into()),
        }
    }

    pub fn is_thread(&self) -> bool {
        self.thread_root.is_some()
    }
}

/// One thread root, as listed in the thread picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummary {
    /// Event ID of the root. This is the agent session key.
    pub root_event_id: String,
    pub sender_display: String,
    /// First line of the root message.
    pub preview: String,
    pub timestamp: u64,
}

/// Sent from the app to the worker.
#[derive(Debug, Clone)]
pub enum Command {
    /// Begin streaming a view's timeline.
    OpenView(View),
    /// Stop streaming it and drop the subscription.
    CloseView(View),
    /// Load older events.
    Paginate {
        view: View,
        count: u16,
    },
    /// Send a message. Thread views thread it automatically.
    SendMessage {
        view: View,
        body: String,
    },
    /// Send a message as a reply to an event.
    SendReply {
        view: View,
        in_reply_to: String,
        body: String,
    },
    /// Replace an event's content. Only own, editable events.
    Edit {
        view: View,
        event_id: String,
        body: String,
    },
    /// Redact an event.
    Redact {
        view: View,
        event_id: String,
    },
    /// Ask for the room's thread roots.
    ListThreads {
        room_id: String,
    },
    /// React to an event. Used for approvals and the model picker as well as ordinary
    /// reactions, since that is how Hermes drives them.
    ToggleReaction {
        view: View,
        event_id: String,
        key: String,
    },
    SendTyping {
        room_id: String,
        typing: bool,
    },
    MarkRead {
        view: View,
    },
    Shutdown,
}

/// Sent from the worker to the app.
#[derive(Debug, Clone)]
pub enum WorkerEvent {
    SyncState(SyncState),
    /// The full room list, re-sent whenever it changes.
    Rooms(Vec<RoomSummary>),
    /// A view's timeline, re-sent as a snapshot whenever it changes.
    Timeline {
        view: View,
        entries: Vec<Entry>,
    },
    /// Users typing in a room. An agent typing means `working`.
    Typing {
        room_id: String,
        users: Vec<String>,
    },
    /// A room's thread roots, newest first.
    Threads {
        room_id: String,
        threads: Vec<ThreadSummary>,
    },
    /// Non-fatal; shown in the status line.
    Warning(String),
    /// Fatal; the worker has stopped.
    Fatal(String),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    #[test]
    fn views_distinguish_room_from_thread() {
        let room = View::room("!r:x");
        let thread = View::thread("!r:x", "$root");
        assert_ne!(room, thread);
        assert!(!room.is_thread());
        assert!(thread.is_thread());
    }

    #[test]
    fn views_are_usable_as_map_keys() {
        use std::collections::HashMap;
        let mut map = HashMap::new();
        map.insert(View::thread("!r:x", "$a"), 1);
        map.insert(View::thread("!r:x", "$b"), 2);
        map.insert(View::room("!r:x"), 3);
        assert_eq!(map.len(), 3);
        assert_eq!(map.get(&View::thread("!r:x", "$a")), Some(&1));
    }

    #[test]
    fn agent_payload_reports_fidelity() {
        assert!(!AgentPayload::None.is_agent());
        assert!(!AgentPayload::None.is_degraded());
        let degraded = AgentPayload::Degraded(fallback::Parsed::default());
        assert!(degraded.is_agent());
        assert!(degraded.is_degraded());
    }
}
