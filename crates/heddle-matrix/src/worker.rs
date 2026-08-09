//! The Matrix worker.
//!
//! Owns the [`Client`], the [`SyncService`] and every open [`Timeline`]. The render
//! thread talks to it exclusively through a [`Handle`], so no frame can ever block on
//! the network or on crypto.
//!
//! Each heddle pane maps to one [`View`] here, and each view gets its own
//! `Timeline`. Thread panes use [`TimelineFocus::Thread`], which means sending into a
//! thread is just `Timeline::send` -- the SDK adds the `m.thread` relation, so Hermes
//! routes the message to the right agent session without heddle hand-rolling
//! relations.
//!
//! See `docs/SPEC.md` §4.3.

use crate::model::*;
use futures_util::{pin_mut, StreamExt};
use matrix_sdk::{
    ruma::{events::room::message::RoomMessageEventContent, EventId, RoomId},
    Client, Room,
};
use matrix_sdk_ui::{
    sync_service::{State as SyncServiceState, SyncService},
    timeline::{
        MsgLikeKind, RoomExt, Timeline, TimelineDetails, TimelineFocus, TimelineItemContent,
        VirtualTimelineItem,
    },
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// How many events to request per pagination step.
const PAGINATE_BATCH: u16 = 40;

/// Bound on the app -> worker channel. Commands are user-initiated, so this only needs
/// to absorb a burst of keypresses.
const COMMAND_BUFFER: usize = 64;

/// Bound on the worker -> app channel. Sized to absorb a sync burst without dropping
/// updates; the app drains it with a per-frame budget.
const EVENT_BUFFER: usize = 512;

/// How many rooms the sliding sync list holds.
const ROOM_LIST_PAGE: usize = 500;

/// The app-side handle to the worker.
pub struct Handle {
    commands: mpsc::Sender<Command>,
    events: mpsc::Receiver<WorkerEvent>,
    task: JoinHandle<()>,
}

impl Handle {
    /// Queue a command. Returns `false` once the worker has stopped.
    pub fn send(&self, command: Command) -> bool {
        match self.commands.try_send(command) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(c)) => {
                tracing::warn!(?c, "worker command buffer full; dropping");
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Drain up to `budget` events without awaiting.
    ///
    /// The budget is what stops a sync burst from starving input: the app takes what it
    /// can render this frame and leaves the rest queued.
    pub fn drain(&mut self, budget: usize) -> Vec<WorkerEvent> {
        let mut out = Vec::with_capacity(budget.min(32));
        while out.len() < budget {
            let Ok(event) = self.events.try_recv() else {
                break;
            };
            out.push(event);
        }
        out
    }

    /// Await the next event. Used when the app has nothing else to do.
    pub async fn next(&mut self) -> Option<WorkerEvent> {
        self.events.recv().await
    }

    /// Ask the worker to stop, and wait for it.
    pub async fn shutdown(self) {
        let _ = self.commands.send(Command::Shutdown).await;
        drop(self.commands);
        let _ = self.task.await;
    }
}

/// Spawn the worker for an authenticated client.
pub async fn spawn(client: Client) -> Result<Handle, matrix_sdk_ui::sync_service::Error> {
    let (command_tx, command_rx) = mpsc::channel(COMMAND_BUFFER);
    let (event_tx, event_rx) = mpsc::channel(EVENT_BUFFER);

    let sync_service = SyncService::builder(client.clone())
        .with_offline_mode()
        .build()
        .await?;

    let task = tokio::spawn(async move {
        let mut worker = Worker {
            client,
            sync_service,
            views: HashMap::new(),
            events: event_tx,
        };
        if let Err(e) = worker.run(command_rx).await {
            tracing::error!(error = %e, "matrix worker stopped");
            let _ = worker.events.send(WorkerEvent::Fatal(e.to_string())).await;
        }
    });

    Ok(Handle {
        commands: command_tx,
        events: event_rx,
        task,
    })
}

/// An open timeline plus the task forwarding its updates.
struct OpenView {
    timeline: Arc<Timeline>,
    forward: JoinHandle<()>,
}

impl Drop for OpenView {
    fn drop(&mut self) {
        self.forward.abort();
    }
}

struct Worker {
    client: Client,
    sync_service: SyncService,
    views: HashMap<View, OpenView>,
    events: mpsc::Sender<WorkerEvent>,
}

impl Worker {
    async fn run(&mut self, mut commands: mpsc::Receiver<Command>) -> anyhow::Result<()> {
        // Subscribe *before* starting. `SyncService::state()` hands out an eyeball
        // subscription that only yields transitions happening after it is created, so
        // starting first races the Idle -> Running edge. Losing that edge leaves the
        // status line stuck on "syncing…" for the whole session even though sync is
        // healthy and rooms are arriving.
        let mut sync_state = self.sync_service.state();

        self.sync_service.start().await;
        let _ = self
            .events
            .send(WorkerEvent::SyncState(SyncState::Initial))
            .await;

        let room_list = self.sync_service.room_list_service();

        // The room list only yields once a filter is set, so subscribe first and set an
        // all-pass filter immediately. heddle does its own grouping into workspaces, so
        // server-side filtering would only get in the way.
        let all_rooms = room_list.all_rooms().await?;
        let (room_stream, controller) = all_rooms.entries_with_dynamic_adapters(ROOM_LIST_PAGE);
        controller.set_filter(Box::new(|_| true));
        pin_mut!(room_stream);

        loop {
            tokio::select! {
                // Commands first: a user keypress must not queue behind a sync burst.
                biased;

                command = commands.recv() => {
                    match command {
                        None | Some(Command::Shutdown) => break,
                        Some(c) => {
                            if let Err(e) = self.handle(c).await {
                                tracing::warn!(error = %e, "command failed");
                                let _ = self.events
                                    .send(WorkerEvent::Warning(e.to_string()))
                                    .await;
                            }
                        }
                    }
                }

                Some(_diff) = room_stream.next() => {
                    // The diff tells us *that* the list changed. heddle rebuilds its own
                    // view rather than applying VectorDiffs, because it re-groups rooms
                    // into workspaces anyway.
                    let rooms = collect_rooms(&self.client).await;
                    let _ = self.events.send(WorkerEvent::Rooms(rooms)).await;
                }

                Some(state) = sync_state.next() => {
                    let mapped = match state {
                        SyncServiceState::Idle => SyncState::Idle,
                        SyncServiceState::Running => SyncState::Running,
                        SyncServiceState::Offline => SyncState::Offline,
                        SyncServiceState::Terminated => SyncState::Terminated,
                        SyncServiceState::Error(ref e) => {
                            tracing::warn!(error = %e, "sync service error");
                            SyncState::Offline
                        }
                    };
                    let _ = self.events.send(WorkerEvent::SyncState(mapped)).await;
                }
            }
        }

        self.views.clear();
        self.sync_service.stop().await;
        Ok(())
    }

    async fn handle(&mut self, command: Command) -> anyhow::Result<()> {
        match command {
            Command::Shutdown => {}

            Command::OpenView(view) => self.open(view).await?,

            Command::CloseView(view) => {
                self.views.remove(&view);
            }

            Command::Paginate { view, count } => {
                let timeline = self.timeline(&view)?;
                let count = if count == 0 { PAGINATE_BATCH } else { count };
                timeline.paginate_backwards(count).await?;
            }

            Command::SendMessage { view, body } => {
                // For a thread view the timeline is thread-focused, so `send` adds the
                // m.thread relation itself. That is what keeps Hermes' session routing
                // intact.
                let timeline = self.timeline(&view)?;
                timeline
                    .send(RoomMessageEventContent::text_markdown(&body).into())
                    .await?;
            }

            Command::SendReply {
                view,
                in_reply_to,
                body,
            } => {
                let timeline = self.timeline(&view)?;
                let event_id = EventId::parse(in_reply_to.as_str())?;
                timeline
                    .send_reply(
                        RoomMessageEventContent::text_markdown(&body).into(),
                        event_id.to_owned(),
                    )
                    .await?;
            }

            Command::Edit {
                view,
                event_id,
                body,
            } => {
                use matrix_sdk::room::edit::EditedContent;
                let timeline = self.timeline(&view)?;
                let event_id = EventId::parse(event_id.as_str())?;
                let item = timeline
                    .item_by_event_id(&event_id)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("event {event_id} not in timeline"))?;
                // The SDK rejects uneditable events, but failing here keeps the message
                // in the composer rather than losing it to a round trip.
                if !item.is_editable() {
                    anyhow::bail!("that message cannot be edited");
                }
                timeline
                    .edit(
                        &item.identifier(),
                        EditedContent::RoomMessage(
                            RoomMessageEventContent::text_markdown(&body).into(),
                        ),
                    )
                    .await?;
            }

            Command::Redact { view, event_id } => {
                let timeline = self.timeline(&view)?;
                let event_id = EventId::parse(event_id.as_str())?;
                let item = timeline
                    .item_by_event_id(&event_id)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("event {event_id} not in timeline"))?;
                timeline.redact(&item.identifier(), None).await?;
            }

            Command::ListThreads { room_id } => {
                let threads = collect_threads(&self.room(&room_id)?).await?;
                let _ = self
                    .events
                    .send(WorkerEvent::Threads { room_id, threads })
                    .await;
            }

            Command::ToggleReaction {
                view,
                event_id,
                key,
            } => {
                // Approvals and the model picker both ride on reactions, so this is a
                // hot path, not a nicety.
                let timeline = self.timeline(&view)?;
                let event_id = EventId::parse(event_id.as_str())?;
                let item = timeline
                    .item_by_event_id(&event_id)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("event {event_id} not in timeline"))?;
                timeline.toggle_reaction(&item.identifier(), &key).await?;
            }

            Command::SendTyping { room_id, typing } => {
                self.room(&room_id)?.typing_notice(typing).await?;
            }

            Command::MarkRead { view } => {
                use matrix_sdk::ruma::api::client::receipt::create_receipt::v3::ReceiptType;
                self.timeline(&view)?
                    .mark_as_read(ReceiptType::Read)
                    .await?;
            }
        }
        Ok(())
    }

    fn room(&self, room_id: &str) -> anyhow::Result<Room> {
        let id = RoomId::parse(room_id)?;
        self.client
            .get_room(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown room {room_id}"))
    }

    fn timeline(&self, view: &View) -> anyhow::Result<Arc<Timeline>> {
        self.views
            .get(view)
            .map(|v| v.timeline.clone())
            .ok_or_else(|| anyhow::anyhow!("view {view:?} is not open"))
    }

    /// Subscribe to a view's timeline and forward snapshots.
    async fn open(&mut self, view: View) -> anyhow::Result<()> {
        if self.views.contains_key(&view) {
            return Ok(());
        }

        let room = self.room(&view.room_id)?;
        let focus = match &view.thread_root {
            Some(root) => TimelineFocus::Thread {
                root_event_id: EventId::parse(root.as_str())?.to_owned(),
            },
            // Hide threaded events from the room timeline: each thread gets its own
            // pane, so showing them twice would double every agent turn.
            None => TimelineFocus::Live {
                hide_threaded_events: true,
            },
        };

        let timeline = Arc::new(room.timeline_builder().with_focus(focus).build().await?);

        let events = self.events.clone();
        let forward_timeline = timeline.clone();
        let forward_view = view.clone();

        let forward = tokio::spawn(async move {
            let (initial, stream) = forward_timeline.subscribe().await;
            pin_mut!(stream);

            let emit = |entries: Vec<Entry>| {
                let events = events.clone();
                let view = forward_view.clone();
                async move {
                    let _ = events.send(WorkerEvent::Timeline { view, entries }).await;
                }
            };

            emit(convert(initial.iter())).await;

            while stream.next().await.is_some() {
                // A snapshot rather than an incremental patch. The SDK has already done
                // the hard part -- resolving the edit chain and reaction aggregation
                // into stable item identities -- so rebuilding the view is cheap
                // relative to reimplementing VectorDiff application, and cannot drift.
                let items = forward_timeline.items().await;
                let entries = convert(items.iter());
                // The only way to tell "nothing arrived" from "something arrived and
                // was dropped in conversion".
                tracing::debug!(
                    view = ?forward_view,
                    items = items.len(),
                    entries = entries.len(),
                    "timeline snapshot"
                );
                emit(entries).await;
            }
        });

        self.views.insert(view, OpenView { timeline, forward });
        Ok(())
    }
}

/// Fetch the room's thread roots for the picker.
///
/// The room timeline hides threaded events, so threads are invisible from it by design.
/// This is the only way to discover them, and for a Hermes room it is the list of agent
/// sessions.
async fn collect_threads(room: &Room) -> anyhow::Result<Vec<ThreadSummary>> {
    use matrix_sdk::room::ListThreadsOptions;
    use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent};

    let roots = room.list_threads(ListThreadsOptions::default()).await?;

    let mut out = Vec::with_capacity(roots.chunk.len());
    for event in roots.chunk {
        let Ok(parsed) = event.raw().deserialize() else {
            continue;
        };
        let AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(message)) =
            parsed
        else {
            continue;
        };
        let Some(original) = message.as_original() else {
            continue;
        };

        let sender = message.sender().to_owned();
        let body = original.content.body();
        out.push(ThreadSummary {
            root_event_id: message.event_id().to_string(),
            sender_display: sender.localpart().to_owned(),
            // One line: the picker is a list, not a transcript.
            preview: body.lines().next().unwrap_or_default().trim().to_owned(),
            timestamp: u64::from(message.origin_server_ts().0),
        });
    }
    Ok(out)
}

/// Build the room list the app renders.
async fn collect_rooms(client: &Client) -> Vec<RoomSummary> {
    let mut out = Vec::new();
    for room in client.rooms() {
        let room_id = room.room_id().to_string();
        let display_name = room
            .cached_display_name()
            .map(|n| n.to_string())
            .unwrap_or_else(|| room_id.clone());

        out.push(RoomSummary {
            room_id,
            display_name,
            is_space: room.is_space(),
            // Populated from m.space.child in M4, when workspaces land.
            parents: Vec::new(),
            is_direct: room.is_direct().await.unwrap_or(false),
            is_encrypted: room
                .latest_encryption_state()
                .await
                .is_ok_and(|s| s.is_encrypted()),
            notification_count: room.num_unread_notifications(),
            highlight_count: room.num_unread_mentions(),
        });
    }
    out.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    out
}

fn convert<'a>(
    items: impl Iterator<Item = &'a Arc<matrix_sdk_ui::timeline::TimelineItem>>,
) -> Vec<Entry> {
    items.map(|item| convert_item(item)).collect()
}

fn convert_item(item: &matrix_sdk_ui::timeline::TimelineItem) -> Entry {
    let id = item.unique_id().0.clone();

    let Some(event) = item.as_event() else {
        let kind = match item.as_virtual() {
            Some(VirtualTimelineItem::DateDivider(ts)) => EntryKind::DateDivider(ts.0.into()),
            Some(VirtualTimelineItem::ReadMarker) => EntryKind::ReadMarker,
            Some(VirtualTimelineItem::TimelineStart) => EntryKind::TimelineStart,
            None => EntryKind::Notice(String::new()),
        };
        return Entry {
            id,
            event_id: None,
            kind,
        };
    };

    let event_id = event.event_id().map(ToString::to_string);

    let kind = match event.content() {
        TimelineItemContent::MsgLike(msg_like) => match msg_like.as_message() {
            Some(message) => {
                let body = message.body().to_owned();
                EntryKind::Message(Message {
                    sender: event.sender().to_string(),
                    sender_display: match event.sender_profile() {
                        TimelineDetails::Ready(profile) => profile
                            .display_name
                            .clone()
                            .unwrap_or_else(|| event.sender().localpart().to_owned()),
                        _ => event.sender().localpart().to_owned(),
                    },
                    agent: decode_agent(event, &body),
                    body,
                    timestamp: event.timestamp().0.into(),
                    is_own: event.is_own(),
                    is_edited: message.is_edited(),
                    thread_root: msg_like.thread_root.as_ref().map(ToString::to_string),
                    // The room timeline hides threaded events, so this summary is the
                    // only sign from inside the room that a thread exists at all.
                    thread_replies: msg_like.thread_summary.as_ref().map(|t| t.num_replies),
                    reactions: msg_like
                        .reactions
                        .iter()
                        .map(|(key, senders)| (key.clone(), senders.len()))
                        .collect(),
                })
            }
            None => match &msg_like.kind {
                MsgLikeKind::UnableToDecrypt(_) => EntryKind::UnableToDecrypt,
                MsgLikeKind::Redacted => EntryKind::Notice("message deleted".into()),
                _ => unrendered(event, "message-like"),
            },
        },
        _ => unrendered(event, "event"),
    };

    Entry { id, event_id, kind }
}

/// Placeholder for a timeline item heddle does not know how to draw.
///
/// Silently rendering nothing is the worst option: the event is on the wire, the user
/// can see the room has moved on, and heddle shows a gap. A dim line naming the type
/// makes an unsupported event diagnosable instead of invisible.
fn unrendered(event: &matrix_sdk_ui::timeline::EventTimelineItem, what: &str) -> EntryKind {
    let kind = event_type(event);
    tracing::debug!(event_type = %kind, sender = %event.sender(), "unrendered {what}");
    EntryKind::Notice(format!("· {kind}"))
}

/// The `type` field of the underlying event.
fn event_type(event: &matrix_sdk_ui::timeline::EventTimelineItem) -> String {
    event
        .latest_json()
        .and_then(|raw| raw.deserialize_as::<serde_json::Value>().ok())
        .and_then(|value| {
            value
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| "unknown event".to_owned())
}

/// Pull agent structure out of an event's raw JSON.
///
/// Uses `latest_json`, which resolves to the newest edit. Hermes streams by
/// progressively editing one event, so the original JSON would only ever show the first
/// frame of a turn.
fn decode_agent(event: &matrix_sdk_ui::timeline::EventTimelineItem, body: &str) -> AgentPayload {
    let Some(raw) = event.latest_json() else {
        return AgentPayload::None;
    };
    let Ok(value) = raw.deserialize_as::<serde_json::Value>() else {
        return AgentPayload::None;
    };
    let Some(content) = value.get("content") else {
        return AgentPayload::None;
    };

    match heddle_agent::ingest(content, body, true) {
        heddle_agent::Ingest::Structured(ev) => AgentPayload::Structured(ev),
        heddle_agent::Ingest::Degraded(p) => AgentPayload::Degraded(p),
        heddle_agent::Ingest::Plain => AgentPayload::None,
    }
}
