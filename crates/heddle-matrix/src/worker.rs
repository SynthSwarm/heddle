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
use heddle_agent::{Adapters, Ingest};
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

const PAGINATE_BATCH: u16 = 40;

/// How many times to ask for a page before giving a pane up as empty.
///
/// Five pages of [`PAGINATE_BATCH`] is enough to get past a long run of threaded events
/// without walking a busy room back to its creation on every startup.
const PAGINATE_ATTEMPTS: u8 = 5;

/// Bound on the app -> worker channel. Commands are user-initiated, so this only needs
/// to absorb a burst of keypresses.
const COMMAND_BUFFER: usize = 64;

/// Bound on the worker -> app channel. Sized to absorb a sync burst without dropping
/// updates; the app drains it with a per-frame budget.
const EVENT_BUFFER: usize = 512;

/// How many rooms the sliding sync list holds.
const ROOM_LIST_PAGE: usize = 500;

/// How long [`Handle::shutdown`] waits for the worker before abandoning it.
///
/// Long enough for an in-flight request to finish, short enough that a wedged worker
/// does not hold the terminal after the user has asked to quit.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// What became of a command handed to [`Handle::send`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Dispatch {
    Queued,
    /// The worker is alive but its queue was full, so the command was discarded.
    ///
    /// Distinct from [`Dispatch::Stopped`], which is fatal: this one is worth a status
    /// line, not a shutdown. Reported rather than swallowed because a discarded
    /// `SendMessage` is a message the user typed and watched disappear.
    Dropped,
    /// The worker has stopped. Nothing further will be delivered.
    Stopped,
}

/// The app-side handle to the worker.
pub struct Handle {
    commands: mpsc::Sender<Command>,
    events: mpsc::Receiver<WorkerEvent>,
    task: JoinHandle<()>,
}

impl Handle {
    /// Queue a command.
    ///
    /// Never blocks: this is called from the render loop, where awaiting the worker
    /// would be a frame spent not drawing.
    pub fn send(&self, command: Command) -> Dispatch {
        // Named, never logged whole: `Command`'s `Debug` carries message bodies, and
        // writing decrypted content to a log is what `capture` is gated to avoid.
        let kind = command.kind();
        match self.commands.try_send(command) {
            Ok(()) => Dispatch::Queued,
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(kind, "worker command buffer full; dropping");
                Dispatch::Dropped
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Dispatch::Stopped,
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

    pub async fn shutdown(mut self) {
        let _ = self.commands.send(Command::Shutdown).await;
        drop(self.commands);

        // Close the event channel before joining. Nothing drains it past this point --
        // the event loop has returned -- and the worker sends into it with `.await` in
        // about twenty-five places, so a full buffer would park the worker in `send`
        // and the join below would wait for a task waiting on us. `close` wakes those
        // senders with an error, so the worker unwinds through its normal paths rather
        // than being cut off mid-command.
        self.events.close();

        // Bounded even so: `Shutdown` involves the network, and a homeserver that has
        // stopped answering must not become a client that will not exit.
        if tokio::time::timeout(SHUTDOWN_GRACE, &mut self.task)
            .await
            .is_err()
        {
            tracing::warn!("matrix worker did not stop within the grace period; abandoning it");
            self.task.abort();
        }
    }
}

/// Spawn the worker for an authenticated client.
pub async fn spawn(
    client: Client,
    agents: Adapters,
) -> Result<Handle, matrix_sdk_ui::sync_service::Error> {
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
            agents: Arc::new(agents),
            verification: None,
            verification_driver: None,
            verified_watch: None,
            recovery_watch: None,
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
    /// The agent integrations consulted for every message. Shared with the per-view
    /// forwarding tasks, which decode on their own.
    agents: Arc<Adapters>,
    /// The verification in progress, if any. Held so the user's answers have something
    /// to act on; the flow itself is followed on `verification_driver`.
    verification: Option<VerificationRequest>,
    verification_driver: Option<DriverTask>,
    /// Watches this device's cross-signing verification state.
    verified_watch: Option<DriverTask>,
    /// Watches whether this account's secrets are recoverable.
    recovery_watch: Option<DriverTask>,
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
        // Subscribe *before* starting: `SyncService::state()` yields only transitions
        // after the subscription is created, so starting first races the
        // Idle -> Running edge and leaves the status line stuck on "syncing…".
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

        // Verification requests are to-device events, which no timeline sees. The
        // handler runs on the sync task and cannot touch the worker, so it hands the
        // request over a channel.
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

        self.watch_verification_state();
        self.watch_recovery_state();

        // Late-arriving room keys. Without this the "unable to decrypt" placeholder is
        // terminal: the key arrives, the store accepts it, and the row goes on saying
        // it cannot be read until heddle restarts.
        let mut room_keys = match self.client.encryption().room_keys_received_stream().await {
            Some(stream) => Box::pin(stream),
            None => {
                tracing::warn!("no olm machine; late room keys will not be retried");
                Box::pin(futures_util::stream::empty())
                    as std::pin::Pin<Box<dyn futures_util::Stream<Item = _> + Send>>
            }
        };

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

                Some(update) = room_keys.next() => {
                    match update {
                        Ok(keys) => {
                            let pairs = keys
                                .into_iter()
                                .map(|k| (k.room_id.to_string(), k.session_id))
                                .collect();
                            self.retry_decryption(pairs).await;
                        }
                        // Lagging means we missed *which* keys arrived. There is no
                        // public blanket retry, so report the gap.
                        Err(skipped) => {
                            tracing::warn!(?skipped, "lagged behind room key updates");
                            self.report_lost_key_updates().await;
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
        self.client.remove_event_handler(handler);
        self.sync_service.stop().await;
        Ok(())
    }

    /// Tell the app whether this device is verified, and keep telling it.
    ///
    /// `Device::is_verified` is the wrong question for our own device: it is
    /// `is_locally_trusted() || is_cross_signing_trusted()`, and a device always trusts
    /// itself locally, so it answers `true` regardless of what the server holds.
    /// `verification_state()` asks whether our own user identity has signed this device,
    /// and is a stream, so the answer stays current when a verification completes
    /// elsewhere.
    fn watch_verification_state(&mut self) {
        use matrix_sdk::encryption::VerificationState;

        let client = self.client.clone();
        let events = self.events.clone();
        self.verified_watch = Some(DriverTask(tokio::spawn(async move {
            let mut states = client.encryption().verification_state();
            while let Some(state) = states.next().await {
                let verified = match state {
                    VerificationState::Verified => Some(true),
                    VerificationState::Unverified => Some(false),
                    VerificationState::Unknown => None,
                };
                let _ = events.send(WorkerEvent::DeviceVerified(verified)).await;
            }
        })));
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
                // One call is not reliably one page; see `keep_paginating`.
                let mut result = Ok(false);
                for attempt in 1..=PAGINATE_ATTEMPTS {
                    let started = std::time::Instant::now();
                    result = timeline.paginate_backwards(count).await;
                    let items = timeline.items().await;
                    let shown = items.iter().filter(|i| i.as_event().is_some()).count();
                    // Pagination is the only way a pane gets history that sync did not
                    // bring, so when one comes up empty this says whether it was asked
                    // for, how long it took and whether it brought anything back. An
                    // empty pane and a pane nobody filled look identical without it.
                    tracing::debug!(
                        view = ?view,
                        attempt,
                        requested = count,
                        items = items.len(),
                        shown,
                        reached_start = ?result.as_ref().ok(),
                        elapsed_ms = started.elapsed().as_millis(),
                        "paginated backwards"
                    );
                    let Ok(reached_start) = result else { break };
                    if !keep_paginating(attempt, shown, reached_start) {
                        break;
                    }
                }
                // A pagination that adds nothing produces no diff, so the subscriber
                // stays silent -- and the app clears its in-flight flag on snapshots.
                // Without one the flag leaks, and it is what suppresses duplicate
                // requests, so every later pagination for the view is dropped. Emitted
                // here on failure too.
                let entries = convert(timeline.items().await.iter(), &self.agents);
                let _ = self
                    .events
                    .send(WorkerEvent::Timeline { view, entries })
                    .await;
                result?;
            }

            Command::SendMessage {
                view,
                body,
                mentions,
            } => {
                // For a thread view the timeline is thread-focused, so `send` adds the
                // m.thread relation itself. That is what keeps Hermes' session routing
                // intact.
                let timeline = self.timeline(&view)?;
                timeline.send(mention(&body, &mentions)?.into()).await?;
            }

            Command::SendReply {
                view,
                in_reply_to,
                body,
                mentions,
            } => {
                let timeline = self.timeline(&view)?;
                let event_id = EventId::parse(in_reply_to.as_str())?;
                timeline
                    .send_reply(mention(&body, &mentions)?.into(), event_id.to_owned())
                    .await?;
            }

            Command::Edit {
                view,
                event_id,
                body,
                mentions,
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
                        EditedContent::RoomMessage(mention(&body, &mentions)?.into()),
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

            Command::ListMembers { room_id } => {
                let members = collect_members(&self.room(&room_id)?).await?;
                let _ = self
                    .events
                    .send(WorkerEvent::Members { room_id, members })
                    .await;
            }

            Command::ToggleReaction {
                view,
                event_id,
                key,
            } => {
                // Approvals and the model picker both ride on reactions.
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
                let timeline = self.timeline(&view)?;
                // The event is chosen here rather than left to `Timeline::mark_as_read`;
                // see `receipt_target` for what that got wrong.
                let items = timeline.items().await;
                if let Some(event_id) = receipt_target(items.iter(), &view) {
                    // `send_single_receipt` infers the receipt's thread from the
                    // timeline's focus and drops the request when an existing receipt
                    // already covers the event. A refusal is not reported: the user can
                    // do nothing about it, and a raw Synapse 400 in the status line is
                    // worse than silence.
                    if let Err(e) = timeline
                        .send_single_receipt(ReceiptType::Read, event_id)
                        .await
                    {
                        tracing::debug!(view = ?view, error = %e, "read receipt refused");
                    }
                }
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
                // A mismatch, not a cancel: the other side needs to know the keys
                // disagreed rather than that someone changed their mind.
                self.sas().await?.mismatch().await?;
            }

            Command::RecoverWithKey(key) => {
                // Recovery keys are pasted from password managers or typed from
                // paper; trailing whitespace is not a wrong key.
                let key = key.trim();
                if key.is_empty() {
                    anyhow::bail!("no recovery key given");
                }
                // Not `?`: the prompt needs this specifically so it can ask again.
                let recovery = self.client.encryption().recovery();
                match recovery.recover(key).await {
                    Ok(()) => {
                        // The state stream publishes changes only, so a state that was
                        // already correct would leave the prompt waiting for ever.
                        let _ = self
                            .events
                            .send(WorkerEvent::Recovery(map_recovery(recovery.state())))
                            .await;
                    }
                    Err(e) => {
                        let _ = self
                            .events
                            .send(WorkerEvent::RecoveryFailed(e.to_string()))
                            .await;
                    }
                }
            }

            Command::EnableRecovery => {
                let recovery = self.client.encryption().recovery();
                match recovery.enable().await {
                    Ok(key) => {
                        let _ = self.events.send(WorkerEvent::RecoveryKeyCreated(key)).await;
                    }
                    Err(e) => {
                        let _ = self
                            .events
                            .send(WorkerEvent::RecoveryFailed(e.to_string()))
                            .await;
                    }
                }
            }

            Command::ResetRecoveryKey => {
                let recovery = self.client.encryption().recovery();
                match recovery.reset_key().await {
                    Ok(key) => {
                        let _ = self.events.send(WorkerEvent::RecoveryKeyCreated(key)).await;
                    }
                    Err(e) => {
                        let _ = self
                            .events
                            .send(WorkerEvent::RecoveryFailed(e.to_string()))
                            .await;
                    }
                }
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
    /// Sent to the user identity rather than one named device, so any device the user
    /// owns can answer.
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
    /// Its own task: there is a human at the far end, so any step can take minutes.
    fn drive_verification(&mut self, request: VerificationRequest) {
        // One at a time: two sets of emoji on screen means a user can confirm the
        // wrong one and verify an attacker.
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

        // The SAS is created on the driver task, so the store is the only place both
        // sides can agree on what it is.
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

    /// Re-decrypt what a batch of newly arrived room keys unlocks.
    ///
    /// Only the timelines for the rooms those keys belong to are touched, and only with
    /// the session ids that actually arrived. Retrying every open timeline on every key
    /// would mean re-running decryption across the whole app each time a single message
    /// is sent to us in any room.
    /// `keys` is (room id, session id) pairs; the SDK's own `RoomKeyInfo` is not
    /// re-exported at a public path, and naming it is not worth reaching through
    /// `matrix_sdk_base` for.
    async fn retry_decryption(&self, keys: Vec<(String, String)>) {
        let mut by_room: HashMap<String, Vec<String>> = HashMap::new();
        for (room_id, session_id) in keys {
            by_room.entry(room_id).or_default().push(session_id);
        }

        for (room_id, sessions) in by_room {
            for (view, open) in &self.views {
                if view.room_id == room_id {
                    tracing::debug!(%room_id, count = sessions.len(), "retrying decryption");
                    open.timeline.retry_decryption(sessions.clone()).await;
                }
            }
        }
    }

    /// Report that keys arrived which we can no longer identify.
    ///
    /// There is no public "retry everything": `retry_decryption` takes session ids and
    /// an empty list retries nothing. Lagging means the broadcast buffer overflowed,
    /// which takes a flood of keys.
    async fn report_lost_key_updates(&self) {
        let _ = self
            .events
            .send(WorkerEvent::Warning(
                "missed some key updates; reopen the room if a message still cannot be read".into(),
            ))
            .await;
    }

    /// Watch whether this account's secrets are recoverable.
    fn watch_recovery_state(&mut self) {
        let client = self.client.clone();
        let events = self.events.clone();
        self.recovery_watch = Some(DriverTask(tokio::spawn(async move {
            let recovery = client.encryption().recovery();

            let _ = events
                .send(WorkerEvent::Recovery(map_recovery(recovery.state())))
                .await;

            let states = recovery.state_stream();
            pin_mut!(states);
            while let Some(state) = states.next().await {
                let _ = events
                    .send(WorkerEvent::Recovery(map_recovery(state)))
                    .await;
            }
        })));
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
        let agents = Arc::clone(&self.agents);

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

            let initial = convert(initial.iter(), &agents);
            // What the app is showing for this view, which is not always what the last
            // snapshot said -- see `classify`.
            let mut showing = initial.len();
            let mut refilling = false;
            emit(initial).await;

            while stream.next().await.is_some() {
                // A whole snapshot: the SDK has already resolved edit chains and
                // reaction aggregation into stable item identities, so rebuilding is
                // cheap next to applying VectorDiffs, and cannot drift.
                let items = forward_timeline.items().await;
                let entries = convert(items.iter(), &agents);
                // `messages`, not a second count of the items: they are equal by
                // construction. A pane of thirty items and no messages is the shape of a
                // room whose recent history is all threaded, and that took a second bug
                // to notice for want of this number.
                tracing::debug!(
                    view = ?forward_view,
                    items = items.len(),
                    messages = messages(&entries),
                    "timeline snapshot"
                );
                // Counts alone cannot distinguish "never arrived" from "arrived as
                // something unexpected".
                if tracing::enabled!(tracing::Level::TRACE) {
                    for (i, entry) in entries.iter().enumerate() {
                        tracing::trace!(index = i, entry = %describe(entry), "entry");
                    }
                }

                match classify(entries.len(), showing, refilling) {
                    Snapshot::Show => {
                        showing = entries.len();
                        refilling = false;
                        emit(entries).await;
                    }
                    Snapshot::Wait => {}
                    Snapshot::Refill => {
                        tracing::debug!(
                            view = ?forward_view,
                            showing,
                            "timeline cache invalidated; refilling instead of blanking the pane"
                        );
                        refilling = true;
                        // Back-pagination is what reloads the unloaded chunk, for a
                        // thread as much as for a room. Its diffs wake this same loop,
                        // and the snapshot that follows is the one the app gets.
                        if let Err(e) = forward_timeline.paginate_backwards(PAGINATE_BATCH).await {
                            // The pane keeps what it had, and the next live event
                            // refills it.
                            tracing::warn!(
                                view = ?forward_view,
                                error = %e,
                                "could not refill an invalidated timeline"
                            );
                        }
                    }
                }
            }
        });

        self.views.insert(view, OpenView { timeline, forward });
        Ok(())
    }
}

/// How many entries a reader would call messages.
///
/// Dividers, read markers and notices fill a pane without giving it anything to say, so
/// they are exactly what a count meant to answer "is this pane empty?" must leave out.
fn messages(entries: &[Entry]) -> usize {
    entries
        .iter()
        .filter(|e| matches!(e.kind, EntryKind::Message(_)))
        .count()
}

/// Whether a pane that still has nothing to show is worth another page.
///
/// One call to `paginate_backwards` is not reliably one page of history, and a client
/// that assumes it is leaves panes empty. Two SDK behaviours do it:
///
/// - A live timeline shows only the last `MAXIMUM_NUMBER_OF_INITIAL_ITEMS` (20) of what
///   it holds, and hides the rest behind a skip count. `paginate_backwards` first tries
///   to satisfy the request by lowering that count, and returns without touching the
///   event cache when it can. The SDK says as much where it does it: "A subsequent call
///   will go to the `Some()` arm of this match, and cause a call to the event cache's
///   pagination."
/// - A room pane hides threaded events, so a page that is entirely thread replies adds
///   nothing it can draw. In an agent room that is the normal shape of recent history.
///
/// Thread panes go straight to `/relations` and have neither behaviour. A pane that
/// already has content asks once.
fn keep_paginating(attempt: u8, shown: usize, reached_start: bool) -> bool {
    shown == 0 && !reached_start && attempt < PAGINATE_ATTEMPTS
}

/// What to do with a timeline snapshot, given what the pane is already showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Snapshot {
    /// Send it: it is the truth about the view.
    Show,
    /// Drop it and back-paginate; the cache was unloaded, not the history deleted.
    Refill,
    /// Drop it; a refill is already in flight.
    Wait,
}

/// Decide whether an empty snapshot is news or an artefact.
///
/// A *gappy* sync -- one the server marks `limited`, carrying a fresh prev-batch token
/// -- makes the SDK unload the room's linked chunk down to its last chunk, and invalidate
/// every thread in the room along with it, because it cannot know which ones the gap
/// touched. Both are deliberate: `RoomEventCacheState::handle_sync` says so, and the
/// SDK's own tests assert the events disappear. Every timeline for that room then
/// publishes a snapshot of nothing, within milliseconds of each other, and nothing
/// refills them until something asks for a page.
///
/// Forwarded verbatim, that empties every pane at once. So an empty snapshot for a view
/// that was showing something is treated as the invalidation it is and answered with a
/// back-pagination. The cost of being wrong is a pane that keeps its transcript after a
/// room genuinely emptied, which needs the room to be left -- redaction leaves entries
/// behind.
fn classify(snapshot: usize, showing: usize, refilling: bool) -> Snapshot {
    if snapshot > 0 || showing == 0 {
        Snapshot::Show
    } else if refilling {
        Snapshot::Wait
    } else {
        Snapshot::Refill
    }
}

/// The newest event in `items` that a receipt for `view` is allowed to name.
///
/// `Timeline::mark_as_read` picks this itself, and picked wrong: the server answered
/// `[400 / M_INVALID_PARAM] event_id $… is not related to thread main`, and heddle
/// reported it as a command failure, so a raw Synapse error landed in the status line.
///
/// For a live timeline with `hide_threaded_events` the SDK sends the receipt against
/// `main` and skips events it knows are threaded -- but it must also skip *aggregations
/// of* threaded events, and a reaction carries no thread relation of its own, so it
/// looks unthreaded until its target is resolved. That resolution needs the target still
/// in the timeline's remote events, and after a gappy sync unloads the room's chunk
/// (§2.1) it is not.
///
/// A view already answers the question: its entries are the events it displays, the room
/// pane hides threaded events, and a thread pane shows one thread. Picking from what was
/// drawn makes the receipt consistent with the pane by construction, and reactions never
/// appear here -- they are folded into the message they annotate.
fn receipt_target<'a>(
    items: impl DoubleEndedIterator<Item = &'a Arc<matrix_sdk_ui::timeline::TimelineItem>>,
    view: &View,
) -> Option<matrix_sdk::ruma::OwnedEventId> {
    items.rev().find_map(|item| {
        let event = item.as_event()?;
        let event_id = event.event_id()?;
        let root = match event.content() {
            TimelineItemContent::MsgLike(msg_like) => {
                msg_like.thread_root.as_ref().map(ToString::to_string)
            }
            _ => None,
        };
        belongs_to(
            view.thread_root.as_deref(),
            event_id.as_str(),
            root.as_deref(),
        )
        .then(|| event_id.to_owned())
    })
}

/// Whether an event may carry the read receipt for a view.
///
/// This is the same question the homeserver asks when it validates the receipt's
/// `thread_id`, which is why it is worth asking in the same terms.
fn belongs_to(view_root: Option<&str>, event_id: &str, event_root: Option<&str>) -> bool {
    match view_root {
        // The room pane hides threaded events, so its receipt goes against `main`, and
        // `main` means an event that is in no thread.
        None => event_root.is_none(),
        // A thread pane's receipt names that thread. The root qualifies: it is the
        // thread's first event, not an event outside it.
        Some(root) => event_root == Some(root) || event_id == root,
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

/// Fetch the room's joined members for the mention picker.
///
/// Joined only: mentioning someone who has left notifies nobody. `members` rather than
/// `members_no_sync`, because a lazily loaded member list has nothing in the store yet.
async fn collect_members(room: &Room) -> anyhow::Result<Vec<MemberSummary>> {
    use matrix_sdk::RoomMemberships;

    let mut out: Vec<MemberSummary> = room
        .members(RoomMemberships::JOIN)
        .await?
        .into_iter()
        .map(|member| MemberSummary {
            user_id: member.user_id().to_string(),
            display_name: member.name().to_owned(),
            // The SDK computes ambiguity across the whole room; a partial list cannot.
            ambiguous: member.name_ambiguous(),
        })
        .collect();

    // Sorted by name so the picker's unfiltered order is predictable; by user ID after
    // that so two members sharing a name do not swap places between openings.
    out.sort_by(|a, b| {
        a.display_name
            .to_lowercase()
            .cmp(&b.display_name.to_lowercase())
            .then_with(|| a.user_id.cmp(&b.user_id))
    });
    Ok(out)
}

/// Build message content that mentions `mentions`.
///
/// Since spec v1.7 the push rules read `m.mentions`, so a body containing `@someone` and
/// nothing else notifies nobody and an agent waiting to be called never hears. Both are
/// written: the name in the body for readability, the ID in `m.mentions` for delivery.
///
/// An unparseable user ID fails the send rather than being dropped.
fn mention(body: &str, mentions: &[String]) -> anyhow::Result<RoomMessageEventContent> {
    use matrix_sdk::ruma::{events::Mentions, UserId};

    let content = RoomMessageEventContent::text_markdown(body);
    if mentions.is_empty() {
        return Ok(content);
    }

    let ids = mentions
        .iter()
        .map(|id| UserId::parse(id.as_str()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(content.add_mentions(Mentions::with_user_ids(ids)))
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
    // Once for the whole list: per room would re-read the same Space state per member.
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
    agents: &Adapters,
) -> Vec<Entry> {
    items.map(|item| convert_item(item, agents)).collect()
}

fn convert_item(item: &matrix_sdk_ui::timeline::TimelineItem, agents: &Adapters) -> Entry {
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
                    agent: decode_agent(event, &body, agents),
                    shield: shield_of(event),
                    body,
                    timestamp: event.timestamp().0.into(),
                    is_own: event.is_own(),
                    is_edited: message.is_edited(),
                    thread_root: msg_like.thread_root.as_ref().map(ToString::to_string),
                    // The room timeline hides threaded events, so this is the only
                    // sign from inside the room that a thread exists.
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

/// Follow one verification request from start to finish, reporting each step.
///
/// Two protocol steps happen here without asking the user.
///
/// Once the request is ready someone must start the SAS flow. Both sides may, and the
/// spec resolves the tie, so heddle starts rather than waits -- two waiting clients
/// hang.
///
/// A flow started by the other device needs accepting before keys are exchanged. That
/// accept is a protocol step; the user's decision comes later, when they compare the
/// emoji.
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
        let Some(current) = state.take() else {
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
                // The protocol offers decimals too; seven pictures are compared across
                // a room more reliably than three five-digit numbers.
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

/// What the SDK makes of this event's authenticity.
///
/// `strict` is false, as in Element and the SDK's own callers. The strict variant also
/// shields messages from devices merely unsigned by a sender whose identity we have
/// never verified, which is most senders in most rooms -- a shield on every message is
/// read as a shield on none.
fn shield_of(event: &matrix_sdk_ui::timeline::EventTimelineItem) -> Shield {
    use matrix_sdk_ui::timeline::{
        TimelineEventShieldState as State, TimelineEventShieldStateCode as Code,
    };

    let reason = |code| match code {
        Code::AuthenticityNotGuaranteed => ShieldReason::Unknown,
        Code::UnknownDevice => ShieldReason::UnknownDevice,
        Code::UnsignedDevice => ShieldReason::UnsignedDevice,
        Code::UnverifiedIdentity => ShieldReason::UnverifiedIdentity,
        Code::VerificationViolation => ShieldReason::IdentityChanged,
        Code::MismatchedSender => ShieldReason::MismatchedSender,
        Code::SentInClear => ShieldReason::SentInClear,
    };

    match event.get_shield(false) {
        State::None => Shield::None,
        State::Grey { code } => Shield::Caution(reason(code)),
        State::Red { code } => Shield::Warning(reason(code)),
    }
}

/// Translate the SDK's recovery state into ours.
fn map_recovery(state: matrix_sdk::encryption::recovery::RecoveryState) -> RecoveryState {
    use matrix_sdk::encryption::recovery::RecoveryState as Sdk;
    match state {
        Sdk::Enabled => RecoveryState::Enabled,
        Sdk::Disabled => RecoveryState::Disabled,
        Sdk::Incomplete => RecoveryState::Incomplete,
        Sdk::Unknown => RecoveryState::Unknown,
    }
}

/// How to name the device at the other end.
///
/// Display name if it has one, device ID otherwise -- both are checkable against the
/// other screen.
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
                    AgentPayload::Structured { .. } => "structured",
                    AgentPayload::Degraded { .. } => "degraded",
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
/// The event is on the wire and the user can see the room has moved on, so a gap would
/// read as a bug. A dim line naming the type is diagnosable.
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
fn decode_agent(
    event: &matrix_sdk_ui::timeline::EventTimelineItem,
    body: &str,
    agents: &Adapters,
) -> AgentPayload {
    let Some(raw) = event.latest_json() else {
        return AgentPayload::None;
    };
    let Ok(value) = raw.deserialize_as::<serde_json::Value>() else {
        return AgentPayload::None;
    };
    let Some(content) = value.get("content") else {
        return AgentPayload::None;
    };

    let payload = match agents.ingest(content, body) {
        Ingest::Structured { adapter, event } => AgentPayload::Structured { adapter, event },
        Ingest::Degraded { adapter, parsed } => AgentPayload::Degraded {
            adapter,
            parsed: Box::new(parsed),
        },
        Ingest::Plain => AgentPayload::None,
    };

    // Recording is opt-in and off by default; see `capture`. Every message is offered,
    // including the plain ones, because "heddle showed nothing for this" is exactly the
    // case a fixture is wanted for.
    if crate::capture::enabled() {
        crate::capture::record(&crate::capture::Record {
            event_id: event.event_id().map(|id| id.as_str()),
            sender: event.sender().as_str(),
            body,
            verdict: match &payload {
                AgentPayload::Structured { .. } => "structured",
                AgentPayload::Degraded { .. } => "degraded",
                AgentPayload::None => "plain",
            },
            adapter: payload.adapter(),
            tools: match &payload {
                AgentPayload::Structured { event, .. } => {
                    event.tool.iter().map(|t| t.name.clone()).collect()
                }
                AgentPayload::Degraded { parsed, .. } => {
                    parsed.tools.iter().map(|t| t.name.clone()).collect()
                }
                AgentPayload::None => Vec::new(),
            },
            content,
        });
    }

    payload
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    /// The channel halves a `Handle` is built from, without a homeserver behind them.
    ///
    /// The worker needs an authenticated `Client`; the parts of `Handle` worth testing
    /// -- a full queue, and shutdown -- need only the channels.
    struct Rig {
        commands: mpsc::Receiver<Command>,
        events: mpsc::Sender<WorkerEvent>,
        command_tx: mpsc::Sender<Command>,
        event_rx: mpsc::Receiver<WorkerEvent>,
    }

    fn rig(command_buffer: usize, event_buffer: usize) -> Rig {
        let (command_tx, commands) = mpsc::channel(command_buffer);
        let (events, event_rx) = mpsc::channel(event_buffer);
        Rig {
            commands,
            events,
            command_tx,
            event_rx,
        }
    }

    impl Rig {
        fn handle(self, task: JoinHandle<()>) -> (Handle, mpsc::Receiver<Command>) {
            (
                Handle {
                    commands: self.command_tx,
                    events: self.event_rx,
                    task,
                },
                self.commands,
            )
        }
    }

    fn idle() -> JoinHandle<()> {
        tokio::spawn(async {})
    }

    #[tokio::test]
    async fn a_dropped_command_is_not_reported_as_sent() {
        // A discarded `SendMessage` is a message the user typed; the caller has to be
        // able to tell it from a delivered one.
        let (handle, _commands) = rig(1, 8).handle(idle());

        assert_eq!(handle.send(Command::Shutdown), Dispatch::Queued);
        assert_eq!(handle.send(Command::Shutdown), Dispatch::Dropped);
    }

    #[tokio::test]
    async fn a_stopped_worker_is_distinguished_from_a_busy_one() {
        // The caller quits on one and carries on with the other.
        let (handle, commands) = rig(1, 8).handle(idle());
        drop(commands);

        assert_eq!(handle.send(Command::Shutdown), Dispatch::Stopped);
    }

    #[tokio::test]
    async fn shutdown_returns_even_though_nothing_is_draining_events() {
        // One slot, and nothing drains it: the state the real channel reaches during
        // a sync burst, which is when people quit.
        let rig = rig(4, 1);
        let events = rig.events.clone();

        let worker = tokio::spawn(async move {
            // What the real worker does in about twenty-five places: push events and
            // ignore the result, noticing shutdown only when the send fails.
            while events
                .send(WorkerEvent::SyncState(SyncState::Running))
                .await
                .is_ok()
            {}
        });

        let (handle, _commands) = rig.handle(worker);

        tokio::time::timeout(std::time::Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown must not wait on a worker that is waiting on it");
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_gives_up_on_a_worker_that_will_not_stop() {
        // A homeserver that has stopped answering must not become a client that will
        // not exit. The clock is paused, so the bound costs no real time.
        let wedged = tokio::spawn(std::future::pending::<()>());
        let (handle, _commands) = rig(4, 8).handle(wedged);

        tokio::time::timeout(SHUTDOWN_GRACE * 4, handle.shutdown())
            .await
            .expect("shutdown must abandon a worker that will not stop");
    }

    #[test]
    fn every_command_can_name_itself_without_quoting_its_payload() {
        // The log says what was dropped without writing the body to disk.
        let body = "the plaintext nobody should find in a log file";
        let command = Command::SendMessage {
            view: View::room("!r:x"),
            body: body.into(),
            mentions: vec![],
        };

        assert_eq!(command.kind(), "SendMessage");
        assert!(!command.kind().contains(body));
    }

    #[test]
    fn a_pane_with_something_to_show_asks_for_one_page() {
        // Scrolling must not turn into five requests.
        assert!(!keep_paginating(1, 20, false));
    }

    #[test]
    fn a_pane_with_nothing_to_show_asks_again() {
        // The startup case: the first call lowered a skip count, or returned a page of
        // threaded events the room pane hides. Either way the pane is still blank.
        assert!(keep_paginating(1, 0, false));
    }

    #[test]
    fn a_pane_stops_asking_at_the_start_of_the_room() {
        // A room really can have no history this pane can draw; asking again would be
        // asking for events that do not exist.
        assert!(!keep_paginating(1, 0, true));
    }

    #[test]
    fn a_pane_gives_up_rather_than_walking_back_forever() {
        // A room whose entire history is threaded would otherwise paginate to its
        // creation event every time it is opened.
        assert!(!keep_paginating(PAGINATE_ATTEMPTS, 0, false));
    }

    #[test]
    fn an_ordinary_snapshot_is_shown() {
        assert_eq!(classify(12, 7, false), Snapshot::Show);
    }

    #[test]
    fn a_view_that_has_nothing_yet_may_be_told_it_has_nothing() {
        // The first snapshot of a room the client has not synced is legitimately empty,
        // and suppressing it would leave the pane waiting for a page that never comes.
        assert_eq!(classify(0, 0, false), Snapshot::Show);
    }

    #[test]
    fn a_gappy_sync_does_not_blank_a_pane_that_had_a_transcript() {
        // Bug 2.1: three views of one room emptied within 5ms of each other because the
        // SDK unloaded the room's chunk and invalidated its threads after a limited
        // sync. None of them was actually empty.
        assert_eq!(classify(0, 71, false), Snapshot::Refill);
    }

    #[test]
    fn a_refill_is_asked_for_once_and_not_on_every_snapshot() {
        // Pagination that returns nothing must not turn into a request per wake-up: the
        // pane keeps what it has and waits instead.
        assert_eq!(classify(0, 71, true), Snapshot::Wait);
    }

    #[test]
    fn a_room_receipt_may_only_name_an_event_outside_every_thread() {
        // Bug 2.3: the room pane's receipt goes against `main`, and the server checks it.
        assert!(belongs_to(None, "$msg", None));
        assert!(!belongs_to(None, "$reply", Some("$root")));
    }

    #[test]
    fn a_thread_receipt_may_name_a_reply_in_that_thread() {
        assert!(belongs_to(Some("$root"), "$reply", Some("$root")));
    }

    #[test]
    fn a_thread_receipt_may_name_the_root_itself() {
        // The root carries no thread relation -- it starts the thread rather than
        // sitting in it -- but it is the one event a thread pane always shows.
        assert!(belongs_to(Some("$root"), "$root", None));
    }

    #[test]
    fn a_thread_receipt_may_not_name_another_thread_or_the_main_timeline() {
        assert!(!belongs_to(Some("$root"), "$reply", Some("$other")));
        assert!(!belongs_to(Some("$root"), "$msg", None));
    }

    #[test]
    fn a_refill_that_worked_ends_the_wait() {
        // The snapshot after a successful back-pagination is shown, and `refilling` is
        // cleared by the caller so a later invalidation is answered afresh.
        assert_eq!(classify(40, 71, true), Snapshot::Show);
    }
}
