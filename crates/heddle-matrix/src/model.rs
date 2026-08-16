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
    /// Full fidelity, from the structured extension.
    Structured {
        /// Which adapter understood it.
        adapter: &'static str,
        event: Box<AgentEvent>,
    },
    /// Recovered from human-readable chrome. Lossy; surfaced with a `~` marker.
    Degraded {
        adapter: &'static str,
        /// Boxed to match `Structured`: three vectors inline would make every
        /// `EntryKind::Message` pay for the lossy path whether or not it used it.
        parsed: Box<fallback::Parsed>,
    },
    /// An ordinary message.
    None,
}

impl AgentPayload {
    pub fn is_agent(&self) -> bool {
        !matches!(self, Self::None)
    }

    pub fn is_degraded(&self) -> bool {
        matches!(self, Self::Degraded { .. })
    }

    /// Which agent integration claimed this message, if any did.
    pub fn adapter(&self) -> Option<&'static str> {
        match self {
            Self::Structured { adapter, .. } | Self::Degraded { adapter, .. } => Some(adapter),
            Self::None => None,
        }
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

/// How much the authenticity of a message can be trusted.
///
/// Taken from the SDK's own shield calculation rather than derived here. Deciding what
/// counts as trustworthy is exactly the judgement a client should not be improvising:
/// the rules cover unsigned devices, unverified identities, senders who changed their
/// keys after being verified, and events whose sender does not own the Megolm session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shield {
    /// Nothing to say.
    None,
    /// Worth noting but not alarming: authenticity cannot be fully established.
    Caution(ShieldReason),
    /// Actively suspicious.
    Warning(ShieldReason),
}

/// Why a shield is being shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShieldReason {
    /// Not enough information to check authenticity.
    Unknown,
    /// The sending device is not known to us at all.
    UnknownDevice,
    /// The sending device was never signed by its owner.
    UnsignedDevice,
    /// The sender's identity has not been verified by us.
    UnverifiedIdentity,
    /// The sender was verified before and their identity has since changed.
    IdentityChanged,
    /// The sender does not own the session the message came in on.
    MismatchedSender,
    /// Sent unencrypted into a room that is supposed to be encrypted.
    SentInClear,
}

impl ShieldReason {
    /// A short phrase for the transcript. Deliberately plain: "unverified" means
    /// something specific here and the user is owed the specific meaning.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Unknown => "authenticity unknown",
            Self::UnknownDevice => "sent from a device we do not know",
            Self::UnsignedDevice => "sent from an unverified device",
            Self::UnverifiedIdentity => "sender is not verified",
            Self::IdentityChanged => "sender's identity changed since you verified them",
            Self::MismatchedSender => "sender does not match the encrypting device",
            Self::SentInClear => "sent unencrypted in an encrypted room",
        }
    }
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
    /// Replies to this message, when it is itself a thread root.
    ///
    /// `Some(0)` is a real state: a thread whose replies have all been redacted still
    /// exists and can still be opened.
    pub thread_replies: Option<u32>,
    /// Reaction key to the number of senders who used it.
    pub reactions: Vec<(String, usize)>,
    /// Authenticity warning for this message, if any.
    pub shield: Shield,
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

/// One joined member of a room, as offered by the mention picker.
///
/// Only joined members: a mention of someone who has left notifies nobody, and offering
/// them would be offering a dead end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberSummary {
    pub user_id: String,
    /// The member's chosen name, or the localpart when they have not set one.
    pub display_name: String,
    /// Whether another joined member shows the same display name.
    ///
    /// Taken from the SDK's own disambiguation rather than recomputed, because deciding
    /// when two people look alike is exactly the judgement a client should not improvise.
    pub ambiguous: bool,
}

/// How far an interactive device verification has got.
///
/// Only one runs at a time. Verification is a conversation with a human at both ends,
/// and a client showing two sets of emoji at once is a client inviting the user to
/// confirm the wrong one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    /// Another of our devices asked to verify. Nothing has been agreed yet.
    Requested { other_device: String },
    /// Protocols are being negotiated. Nothing for the user to do.
    Negotiating { other_device: String },
    /// Both sides derived a short auth string. The user compares and answers.
    Compare {
        other_device: String,
        /// Symbol and its English description, in the order both sides must show them.
        emoji: Vec<(String, String)>,
    },
    /// We answered; the other side has not yet.
    WaitingForOther { other_device: String },
    /// Verified. The device is now signed by this account's identity.
    Done,
    /// Ended without verifying. Carries whatever the SDK said, which may be a plain
    /// withdrawal or a genuine mismatch.
    Cancelled { reason: String },
}

impl Verification {
    /// Whether this state is the end of the flow.
    pub fn is_finished(&self) -> bool {
        matches!(self, Self::Done | Self::Cancelled { .. })
    }
}

/// Whether this account's secrets can be recovered on a new device.
///
/// "Recovery" is secret storage plus a key backup: the cross-signing keys and the Megolm
/// keys, encrypted under a key only the user holds. Without it a fresh device can read
/// nothing sent before it existed, however well verified it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryState {
    /// Not asked yet.
    Unknown,
    /// Set up, and this device holds every secret.
    Enabled,
    /// Nothing is set up. History will not survive losing every device.
    Disabled,
    /// Set up, but this device is missing secrets: it needs the recovery key.
    Incomplete,
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
        /// User IDs to mention, for `m.mentions`.
        ///
        /// Carried separately from the body because a mention is not text. Since spec
        /// v1.7 the push rules fire on `m.mentions`, so an `@name` that only appears in
        /// the body notifies nobody and, more to the point, does not reach an agent
        /// waiting to be called.
        mentions: Vec<String>,
    },
    /// Send a message as a reply to an event.
    SendReply {
        view: View,
        in_reply_to: String,
        body: String,
        mentions: Vec<String>,
    },
    /// Replace an event's content. Only own, editable events.
    Edit {
        view: View,
        event_id: String,
        body: String,
        mentions: Vec<String>,
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
    /// Ask who is in the room, for the mention picker.
    ListMembers {
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
    /// Ask this account's other devices to verify this one.
    StartVerification,
    /// Accept a verification another device asked for.
    AcceptVerification,
    /// The emoji match. Signs the other device and, for a self-verification, gets this
    /// one signed in return.
    ConfirmVerification,
    /// The emoji do not match. Reported to the other side as a mismatch rather than a
    /// plain cancel, because the two mean very different things.
    MismatchVerification,
    /// Withdraw from a verification without judging it.
    CancelVerification,
    /// Unlock this account's secret storage with a recovery key, importing the secrets
    /// and, with them, the ability to read history from backup.
    RecoverWithKey(String),
    /// Set up secret storage and a key backup for an account that has neither.
    EnableRecovery,
    /// Replace the recovery key, leaving the old one useless.
    ResetRecoveryKey,
    Shutdown,
}

impl Command {
    /// The variant name, with nothing else attached.
    ///
    /// For logging. `Command` derives `Debug` for tests and for `WorkerEvent::Fatal`
    /// context, but several variants carry a decrypted message body, and a client that
    /// writes those into a log file has undone the point of encrypting them. Log this
    /// instead.
    pub fn kind(&self) -> &'static str {
        match self {
            Command::OpenView(..) => "OpenView",
            Command::CloseView(..) => "CloseView",
            Command::Paginate { .. } => "Paginate",
            Command::SendMessage { .. } => "SendMessage",
            Command::SendReply { .. } => "SendReply",
            Command::Edit { .. } => "Edit",
            Command::Redact { .. } => "Redact",
            Command::ListThreads { .. } => "ListThreads",
            Command::ListMembers { .. } => "ListMembers",
            Command::ToggleReaction { .. } => "ToggleReaction",
            Command::SendTyping { .. } => "SendTyping",
            Command::MarkRead { .. } => "MarkRead",
            Command::StartVerification => "StartVerification",
            Command::AcceptVerification => "AcceptVerification",
            Command::ConfirmVerification => "ConfirmVerification",
            Command::MismatchVerification => "MismatchVerification",
            Command::CancelVerification => "CancelVerification",
            Command::RecoverWithKey(..) => "RecoverWithKey",
            Command::EnableRecovery => "EnableRecovery",
            Command::ResetRecoveryKey => "ResetRecoveryKey",
            Command::Shutdown => "Shutdown",
        }
    }
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
    /// A room's joined members, for the mention picker.
    Members {
        room_id: String,
        members: Vec<MemberSummary>,
    },
    /// Non-fatal; shown in the status line.
    Warning(String),
    /// Fatal; the worker has stopped.
    Fatal(String),
    /// An interactive verification changed state.
    Verification(Verification),
    /// Whether this device has been signed by the account's own identity.
    ///
    /// `None` means the crypto layer cannot answer yet, which is not the same as "no".
    /// Drawing a warning shield at a user whose device is merely unexamined would train
    /// them to ignore the shield that matters.
    DeviceVerified(Option<bool>),
    /// Whether this account's secrets can be recovered, re-sent whenever it changes.
    Recovery(RecoveryState),
    /// A recovery attempt failed, with the reason. Distinct from a plain warning so the
    /// prompt can offer the key again rather than sitting on "unlocking…" for ever.
    RecoveryFailed(String),
    /// A new recovery key. The server keeps no copy and it cannot be shown again, so
    /// this reaches the user exactly once or not at all.
    RecoveryKeyCreated(String),
}
