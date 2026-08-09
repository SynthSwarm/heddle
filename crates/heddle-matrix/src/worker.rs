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
    encryption::verification::VerificationRequest,
    ruma::{
        events::{
            key::verification::request::ToDeviceKeyVerificationRequestEvent,
            room::message::RoomMessageEventContent,
        },
        EventId, RoomId,
    },
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
            verification: None,
            verification_driver: None,
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
    /// The verification in progress, if any. Held so the user's answers have something
    /// to act on; the flow itself is followed on `verification_driver`.
    verification: Option<VerificationRequest>,
    verification_driver: Option<DriverTask>,
}

/// A spawned verification driver, aborted when replaced or dropped.
struct DriverTask(JoinHandle<()>);

impl Drop for DriverTask {
    fn drop(&mut self) {
        self.0.abort();
    }
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

        // Verification requests arrive as to-device events, which the timeline never
        // sees, so they need their own listener. The handler cannot touch the worker --
        // it runs on the sync task -- so it hands the request over a channel instead.
        let (verify_tx, mut verify_rx) = mpsc::channel::<VerificationRequest>(4);
        let handler = self.client.add_event_handler({
            let tx = verify_tx.clone();
            move |ev: ToDeviceKeyVerificationRequestEvent, client: Client| {
                let tx = tx.clone();
                async move {
                    if let Some(request) = client
                        .encryption()
                        .get_verification_request(&ev.sender, &ev.content.transaction_id)
                        .await
                    {
                        let _ = tx.send(request).await;
                    }
                }
            }
        });

        self.report_device_verified().await;

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

                Some(request) = verify_rx.recv() => {
                    tracing::info!(from = %request.other_user_id(), "verification requested");
                    self.drive_verification(request);
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
        self.client.remove_event_handler(handler);
        self.sync_service.stop().await;
        Ok(())
    }

    /// Tell the app whether this device is verified.
    ///
    /// Best effort: before the first sync the store may not know its own device yet, and
    /// an unanswered question is better left unanswered than reported as "unverified",
    /// which would flash a warning shield at a user who has done nothing wrong.
    async fn report_device_verified(&self) {
        if let Ok(Some(device)) = self.client.encryption().get_own_device().await {
            let _ = self
                .events
                .send(WorkerEvent::DeviceVerified(device.is_verified()))
                .await;
        }
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
                let result = timeline.paginate_backwards(count).await;
                // A pagination that adds nothing produces no diff, so the subscriber
                // stays silent -- and the app clears its in-flight flag on snapshots.
                // Left to that alone the flag leaks, and because the flag is what
                // suppresses duplicate requests, every later pagination for the view is
                // silently dropped: scrolling up stops loading history until some
                // unrelated event happens to produce a snapshot. Emit one ourselves,
                // including on failure, so the flag always clears.
                let entries = convert(timeline.items().await.iter());
                let _ = self
                    .events
                    .send(WorkerEvent::Timeline { view, entries })
                    .await;
                result?;
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

            Command::StartVerification => self.start_verification().await?,

            Command::AcceptVerification => {
                self.verification
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("no verification to accept"))?
                    .accept()
                    .await?;
            }

            Command::ConfirmVerification => {
                self.sas().await?.confirm().await?;
            }

            Command::MismatchVerification => {
                // Reported as a mismatch rather than a cancel. The distinction is the
                // whole point of the flow: a cancel means someone changed their mind, a
                // mismatch means the keys did not agree and the other side should be
                // told loudly.
                self.sas().await?.mismatch().await?;
            }

            Command::CancelVerification => {
                if let Some(request) = self.verification.take() {
                    let _ = request.cancel().await;
                }
                self.verification_driver.take();
                let _ = self
                    .events
                    .send(WorkerEvent::Verification(Verification::Cancelled {
                        reason: "cancelled here".into(),
                    }))
                    .await;
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

    /// Ask this account's other devices to verify this one.
    ///
    /// The request goes to the user identity rather than to one named device, so every
    /// other device the user owns can answer it. Sending to a single device would mean
    /// picking one on the user's behalf, and the phone in their pocket is a better
    /// choice than any heuristic we could write.
    async fn start_verification(&mut self) -> anyhow::Result<()> {
        let user_id = self
            .client
            .user_id()
            .ok_or_else(|| anyhow::anyhow!("not logged in"))?
            .to_owned();

        let identity = self
            .client
            .encryption()
            .get_user_identity(&user_id)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("this account has no cross-signing identity to verify against")
            })?;

        let request = identity.request_verification().await?;
        self.drive_verification(request);
        Ok(())
    }

    /// Watch a verification request through to its end, reporting each step.
    ///
    /// Driven on its own task because the flow is a conversation with a human at the far
    /// end: it can sit at any step for minutes, and the worker loop must stay responsive
    /// to everything else while it does.
    fn drive_verification(&mut self, request: VerificationRequest) {
        // Only one at a time. A second flow would put two sets of emoji on screen, and a
        // user who confirms the wrong one has verified an attacker.
        self.verification_driver.take();

        self.verification = Some(request.clone());
        let events = self.events.clone();
        self.verification_driver = Some(DriverTask(tokio::spawn(async move {
            drive(request, events).await;
        })));
    }

    /// The SAS flow currently in progress, if the user has answered far enough for one
    /// to exist.
    async fn sas(&self) -> anyhow::Result<matrix_sdk::encryption::verification::SasVerification> {
        let request = self
            .verification
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no verification in progress"))?;

        // Looked up rather than held: the SAS is created inside the flow, on the driver
        // task, and the store is the one place both sides can agree on what it is.
        match self
            .client
            .encryption()
            .get_verification(request.own_user_id(), request.flow_id())
            .await
        {
            Some(matrix_sdk::encryption::verification::Verification::SasV1(sas)) => Ok(sas),
            _ => anyhow::bail!("the emoji are not ready yet"),
        }
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
                // At trace level, say what is actually in the snapshot. Counts alone
                // cannot distinguish "the event never arrived" from "it arrived as
                // something unexpected".
                if tracing::enabled!(tracing::Level::TRACE) {
                    for (i, entry) in entries.iter().enumerate() {
                        tracing::trace!(index = i, entry = %describe(entry), "entry");
                    }
                }
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

/// Map each room to the Spaces that list it as a child.
///
/// Read from the Space's own `m.space.child` state rather than the room's
/// `m.space.parent`: any room may assert a parent it was never added to, while only the
/// Space decides what it contains. A child is removed by emptying `via` rather than by
/// deleting the event, so an empty `via` means "no longer in this Space".
async fn space_parents(client: &Client) -> HashMap<String, Vec<String>> {
    use matrix_sdk::ruma::events::space::child::SpaceChildEventContent;

    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for space in client.rooms().into_iter().filter(|room| room.is_space()) {
        let children = match space
            .get_state_events_static::<SpaceChildEventContent>()
            .await
        {
            Ok(children) => children,
            Err(error) => {
                tracing::warn!(space = %space.room_id(), %error, "cannot read Space children");
                continue;
            }
        };

        let space_id = space.room_id().to_string();
        for raw in children {
            let Ok(child) = raw.deserialize() else {
                continue;
            };
            // Only joined and left Spaces carry full state; an invite carries stripped
            // state, and a Space we have not accepted should not be claiming rooms in
            // the room list anyway. A redacted child event has no content and is gone.
            let Some(event) = child.as_sync().and_then(|e| e.as_original()) else {
                continue;
            };
            if event.content.via.is_empty() {
                continue;
            }
            out.entry(event.state_key.to_string())
                .or_default()
                .push(space_id.clone());
        }
    }
    out
}

/// Build the room list the app renders.
async fn collect_rooms(client: &Client) -> Vec<RoomSummary> {
    // Gathered once for the whole list: every room needs to know its Spaces, and asking
    // per room would re-read the same Space state once per member.
    let parents = space_parents(client).await;

    let mut out = Vec::new();
    for room in client.rooms() {
        let room_id = room.room_id().to_string();
        let display_name = room
            .cached_display_name()
            .map(|n| n.to_string())
            .unwrap_or_else(|| room_id.clone());

        out.push(RoomSummary {
            parents: parents.get(&room_id).cloned().unwrap_or_default(),
            room_id,
            display_name,
            is_space: room.is_space(),
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

/// One-line description of an entry, for trace logging.
/// Follow one verification request from start to finish, reporting each step.
///
/// Two things happen here that the user never sees and must not have to think about.
///
/// When the request becomes ready, someone has to actually start the SAS flow. Both
/// sides are allowed to, and the spec resolves the tie, so heddle starts it rather than
/// waiting: a client that waits politely for the other side to move is a client that
/// hangs when the other side is doing the same.
///
/// When the flow arrives having been started by the other device, it needs accepting
/// before any keys are exchanged. That accept is a protocol step, not a decision the
/// user is making -- the decision comes later, when they compare the emoji -- so asking
/// them here would be asking them to approve something they have not been shown yet.
async fn drive(request: VerificationRequest, events: mpsc::Sender<WorkerEvent>) {
    use matrix_sdk::encryption::verification::VerificationRequestState;

    let other = device_label(&request);
    let emit = |state: Verification| {
        let events = events.clone();
        async move {
            let _ = events.send(WorkerEvent::Verification(state)).await;
        }
    };

    let changes = request.changes();
    pin_mut!(changes);

    // The current state is reported before waiting, because a request that arrived while
    // we were busy is already past `Created` and would otherwise sit invisible.
    let mut state = Some(request.state());

    loop {
        let Some(current) = state.take().or(None) else {
            break;
        };

        match current {
            VerificationRequestState::Created { .. } => {
                emit(Verification::Negotiating {
                    other_device: other.clone(),
                })
                .await;
            }
            VerificationRequestState::Requested { .. } => {
                emit(Verification::Requested {
                    other_device: other.clone(),
                })
                .await;
            }
            VerificationRequestState::Ready { .. } => {
                emit(Verification::Negotiating {
                    other_device: other.clone(),
                })
                .await;
                if let Err(e) = request.start_sas().await {
                    tracing::warn!(error = %e, "could not start SAS");
                }
            }
            VerificationRequestState::Transitioned { verification } => {
                let matrix_sdk::encryption::verification::Verification::SasV1(sas) = verification
                else {
                    tracing::warn!("verification transitioned to an unsupported method");
                    let _ = request.cancel().await;
                    emit(Verification::Cancelled {
                        reason: "unsupported verification method".into(),
                    })
                    .await;
                    return;
                };
                drive_sas(sas, other.clone(), events.clone()).await;
                return;
            }
            VerificationRequestState::Done => {
                emit(Verification::Done).await;
                return;
            }
            VerificationRequestState::Cancelled(info) => {
                emit(Verification::Cancelled {
                    reason: info.reason().to_owned(),
                })
                .await;
                return;
            }
        }

        state = changes.next().await;
        if state.is_none() {
            return;
        }
    }
}

/// Follow the SAS half: accept if needed, show the emoji, wait for the outcome.
async fn drive_sas(
    sas: matrix_sdk::encryption::verification::SasVerification,
    other: String,
    events: mpsc::Sender<WorkerEvent>,
) {
    use matrix_sdk::encryption::verification::SasState;

    let emit = |state: Verification| {
        let events = events.clone();
        async move {
            let _ = events.send(WorkerEvent::Verification(state)).await;
        }
    };

    if !sas.we_started() {
        if let Err(e) = sas.accept().await {
            tracing::warn!(error = %e, "could not accept SAS");
            emit(Verification::Cancelled {
                reason: e.to_string(),
            })
            .await;
            return;
        }
    }

    let changes = sas.changes();
    pin_mut!(changes);

    let mut state = Some(sas.state());
    loop {
        let Some(current) = state.take() else { return };

        match current {
            SasState::Created { .. } | SasState::Started { .. } | SasState::Accepted { .. } => {
                emit(Verification::Negotiating {
                    other_device: other.clone(),
                })
                .await;
            }
            SasState::KeysExchanged { emojis, .. } => {
                // Emoji, not decimals. Both are offered by the protocol, but comparing
                // seven pictures across a room is something people do reliably and
                // comparing three five-digit numbers is not.
                let Some(short) = emojis else {
                    let _ = sas.cancel().await;
                    emit(Verification::Cancelled {
                        reason: "the other device refused emoji comparison".into(),
                    })
                    .await;
                    return;
                };
                let emoji = short
                    .emojis
                    .iter()
                    .map(|e| (e.symbol.to_owned(), e.description.to_owned()))
                    .collect();
                emit(Verification::Compare {
                    other_device: other.clone(),
                    emoji,
                })
                .await;
            }
            SasState::Confirmed => {
                emit(Verification::WaitingForOther {
                    other_device: other.clone(),
                })
                .await;
            }
            SasState::Done { .. } => {
                emit(Verification::Done).await;
                let _ = events.send(WorkerEvent::DeviceVerified(true)).await;
                return;
            }
            SasState::Cancelled(info) => {
                emit(Verification::Cancelled {
                    reason: info.reason().to_owned(),
                })
                .await;
                return;
            }
        }

        state = changes.next().await;
    }
}

/// How to name the device at the other end.
///
/// Its display name if it has one, since that is what the user set and will recognise;
/// the device ID otherwise, which is at least checkable against the other screen.
fn device_label(request: &VerificationRequest) -> String {
    match request.state() {
        matrix_sdk::encryption::verification::VerificationRequestState::Requested {
            other_device_data,
            ..
        }
        | matrix_sdk::encryption::verification::VerificationRequestState::Ready {
            other_device_data,
            ..
        } => other_device_data
            .display_name()
            .map_or_else(|| other_device_data.device_id().to_string(), str::to_owned),
        _ => request.other_user_id().to_string(),
    }
}

/// A one-line summary of an entry, for the trace log.
fn describe(entry: &Entry) -> String {
    match &entry.kind {
        EntryKind::Message(m) => {
            let body: String = m.body.chars().take(48).collect();
            format!(
                "message from {} thread_root={:?} replies={:?} agent={} {body:?}",
                m.sender,
                m.thread_root,
                m.thread_replies,
                match &m.agent {
                    AgentPayload::Structured(_) => "structured",
                    AgentPayload::Degraded(_) => "degraded",
                    AgentPayload::None => "plain",
                },
            )
        }
        EntryKind::Notice(text) => format!("notice {text:?}"),
        EntryKind::UnableToDecrypt => "unable to decrypt".to_owned(),
        EntryKind::DateDivider(ts) => format!("date divider {ts}"),
        EntryKind::ReadMarker => "read marker".to_owned(),
        EntryKind::TimelineStart => "timeline start".to_owned(),
    }
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
