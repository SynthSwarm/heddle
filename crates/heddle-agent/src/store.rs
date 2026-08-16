//! Turn assembly and derived agent state.
//!
//! Matrix delivers agent events out of order, duplicated (edits replay) and interleaved
//! across sessions. This module folds that stream into per-session [`Turn`]s and derives
//! the [`AgentState`] that drives pane, tab and workspace badges.
//!
//! State is always *derived*, never read off the wire, so a client that joins mid-turn
//! or reconnects converges on the same answer as one that saw every event.
//!
//! See `docs/SPEC.md` §2.1.

use crate::protocol::{
    AgentEvent, AgentInfo, Approval, ApprovalChoice, Kind, Picker, Tool, ToolStatus, Usage,
};
use std::collections::HashMap;

/// Lifecycle of an agent session, in badge priority order.
///
/// `Ord` is the priority used when rolling child state up to a tab or workspace badge:
/// `Blocked` outranks `Working` outranks `Done` outranks `Idle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum AgentState {
    /// Nothing in flight, or the user has seen the result.
    #[default]
    Idle,
    /// Finished, and the pane has not been focused since.
    Done,
    /// Actively producing output or running a tool.
    Working,
    /// Waiting on a human: an approval or a model choice.
    Blocked,
}

impl AgentState {
    /// The glyph shown in badges.
    ///
    /// All four are text-presentation geometric shapes laid out at one cell. `⚠` is the
    /// obvious choice for `Blocked` and is unusable: terminals promote it to a two-cell
    /// emoji. See `heddle_render::glyphs`.
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Idle => "·",
            Self::Done => "✓",
            Self::Working => "●",
            Self::Blocked => "▲",
        }
    }

    pub fn is_notable(self) -> bool {
        matches!(self, Self::Blocked | Self::Done)
    }
}

/// One user-turn: everything the agent emitted in response to a single prompt.
#[derive(Debug, Clone, Default)]
pub struct Turn {
    pub turn_id: String,
    pub agent: Option<AgentInfo>,
    /// Assistant prose, accumulated from `message.delta`.
    pub text: String,
    /// Reasoning blocks, accumulated from `commentary`.
    pub commentary: String,
    /// Tool calls keyed by their wire index, so a `tool.result` updates its `tool.call`
    /// in place rather than appending a duplicate card.
    tools: HashMap<u32, Tool>,
    /// Insertion order of `tools`, so cards render in the order they were invoked.
    tool_order: Vec<u32>,
    pub usage: Option<Usage>,
    /// Set once `message.stop { final: true }` arrives.
    pub complete: bool,
    /// Highest `seq` folded in. Used for gap detection and to reject replays.
    pub high_seq: u64,
    /// `seq` values that never arrived. Non-empty means the transcript may be partial.
    pub gaps: Vec<u64>,
}

impl Turn {
    /// Tool cards in invocation order.
    pub fn tools(&self) -> impl Iterator<Item = &Tool> {
        self.tool_order
            .iter()
            .filter_map(move |i| self.tools.get(i))
    }

    /// Whether any tool is still running.
    pub fn has_running_tool(&self) -> bool {
        self.tools.values().any(|t| !t.status.is_terminal())
    }

    /// Whether any tool failed. Failed cards auto-expand in the UI.
    pub fn has_failed_tool(&self) -> bool {
        self.tools.values().any(|t| t.status == ToolStatus::Error)
    }
}

/// A pending human-in-the-loop prompt.
#[derive(Debug, Clone)]
pub enum Pending {
    Approval(Approval),
    Picker(Picker),
}

impl Pending {
    pub fn id(&self) -> &str {
        match self {
            Self::Approval(a) => &a.id,
            Self::Picker(p) => &p.id,
        }
    }

    /// Unix seconds at which this prompt expires, if it does.
    pub fn expires_at(&self) -> Option<u64> {
        match self {
            Self::Approval(a) => a.expires_at,
            Self::Picker(p) => p.expires_at,
        }
    }
}

/// Everything known about one agent session — one Matrix thread, one heddle pane.
#[derive(Debug, Clone, Default)]
pub struct Session {
    pub session_id: String,
    /// Turns in arrival order, oldest first.
    pub turns: Vec<Turn>,
    /// Unresolved approvals and pickers. Any entry here forces [`AgentState::Blocked`].
    pub pending: Vec<Pending>,
    /// Set when the session is fed by the fallback parser rather than the extension.
    /// Surfaced as a `~` marker so the degradation is visible.
    pub degraded: bool,
    /// Whether the user has looked at this session since it last finished. Drives the
    /// `Done` -> `Idle` transition.
    seen: bool,
    /// Set by the transport when the agent is typing. Combined with tool state to
    /// produce `Working`, so a session looks busy during model latency, before the
    /// first token lands.
    typing: bool,
}

impl Session {
    fn turn_mut(&mut self, turn_id: &str) -> &mut Turn {
        if let Some(i) = self.turns.iter().position(|t| t.turn_id == turn_id) {
            return &mut self.turns[i];
        }
        self.turns.push(Turn {
            turn_id: turn_id.to_owned(),
            ..Default::default()
        });
        let last = self.turns.len() - 1;
        &mut self.turns[last]
    }

    /// The turn currently in flight, if any.
    pub fn active_turn(&self) -> Option<&Turn> {
        self.turns.last().filter(|t| !t.complete)
    }

    /// The most recent turn, complete or not.
    pub fn latest_turn(&self) -> Option<&Turn> {
        self.turns.last()
    }

    /// Derive the session's state.
    ///
    /// A blocking prompt outranks a running tool: the human is the bottleneck.
    pub fn state(&self) -> AgentState {
        if !self.pending.is_empty() {
            return AgentState::Blocked;
        }
        if self.typing {
            return AgentState::Working;
        }
        match self.turns.last() {
            Some(t) if !t.complete || t.has_running_tool() => AgentState::Working,
            Some(_) if !self.seen => AgentState::Done,
            _ => AgentState::Idle,
        }
    }

    /// Mark the session as looked at, collapsing `Done` to `Idle`.
    pub fn mark_seen(&mut self) {
        self.seen = true;
    }

    pub fn set_typing(&mut self, typing: bool) {
        self.typing = typing;
    }

    /// Whether the transcript is known to be missing events.
    pub fn has_gaps(&self) -> bool {
        self.turns.iter().any(|t| !t.gaps.is_empty())
    }
}

/// Sessions keyed by `session_id`.
#[derive(Debug, Default)]
pub struct AgentStore {
    sessions: HashMap<String, Session>,
}

impl AgentStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, session_id: &str) -> Option<&Session> {
        self.sessions.get(session_id)
    }

    pub fn get_mut(&mut self, session_id: &str) -> Option<&mut Session> {
        self.sessions.get_mut(session_id)
    }

    pub fn sessions(&self) -> impl Iterator<Item = &Session> {
        self.sessions.values()
    }

    /// Roll every session's state up to a single badge value, as used for a tab or
    /// workspace indicator.
    pub fn rollup(&self) -> AgentState {
        self.sessions
            .values()
            .map(Session::state)
            .max()
            .unwrap_or_default()
    }

    /// Number of sessions currently in `state`.
    pub fn count_in(&self, state: AgentState) -> usize {
        self.sessions
            .values()
            .filter(|s| s.state() == state)
            .count()
    }

    /// Fold one decoded event into the store.
    ///
    /// Idempotent with respect to replays: an event whose `seq` has already been folded
    /// into its turn is ignored, which matters because Matrix edits re-deliver the whole
    /// content on every progressive update.
    pub fn apply(&mut self, ev: &AgentEvent) {
        let session = self
            .sessions
            .entry(ev.session_id.clone())
            .or_insert_with(|| Session {
                session_id: ev.session_id.clone(),
                ..Default::default()
            });

        // Any new activity means the user has not seen the outcome yet.
        session.seen = false;

        // Resolve blocking prompts before the seq guard: a resolution may legitimately
        // arrive with a seq we have already seen if the request was re-delivered.
        match ev.kind {
            Kind::ApprovalResolved => {
                if let Some(a) = &ev.approval {
                    session.pending.retain(|p| p.id() != a.id);
                }
            }
            Kind::ApprovalRequest => {
                if let Some(a) = &ev.approval {
                    if !session.pending.iter().any(|p| p.id() == a.id) {
                        session.pending.push(Pending::Approval(a.clone()));
                    }
                }
            }
            Kind::ModelPicker => {
                if let Some(p) = &ev.picker {
                    if !session.pending.iter().any(|x| x.id() == p.id) {
                        session.pending.push(Pending::Picker(p.clone()));
                    }
                }
            }
            _ => {}
        }

        let turn = session.turn_mut(&ev.turn_id);

        // Replay guard. Exceptions: text deltas (Hermes sends cumulative text on the
        // edit chain, so the newest frame replaces), events filling a known gap (late,
        // not duplicate), and the first event of a turn.
        let fills_gap = turn.gaps.contains(&ev.seq);
        if ev.seq <= turn.high_seq && ev.kind != Kind::MessageDelta && !fills_gap {
            return;
        }
        if ev.seq > turn.high_seq + 1 && turn.high_seq > 0 {
            turn.gaps.extend((turn.high_seq + 1)..ev.seq);
        }
        turn.high_seq = turn.high_seq.max(ev.seq);
        turn.gaps.retain(|g| *g != ev.seq);

        if turn.agent.is_none() {
            turn.agent.clone_from(&ev.agent);
        }

        match ev.kind {
            Kind::MessageDelta => {
                if let Some(text) = &ev.text {
                    // Hermes streams cumulative text via m.replace: each frame is the
                    // whole message so far. Appending would duplicate it.
                    if text.starts_with(turn.text.as_str()) {
                        turn.text.clone_from(text);
                    } else {
                        turn.text.push_str(text);
                    }
                }
            }
            Kind::Commentary => {
                if let Some(text) = &ev.text {
                    if text.starts_with(turn.commentary.as_str()) {
                        turn.commentary.clone_from(text);
                    } else {
                        turn.commentary.push_str(text);
                    }
                }
            }
            Kind::MessageStop => {
                // A non-final stop is only a segment break (text -> tool -> text).
                if ev.final_.unwrap_or(false) {
                    turn.complete = true;
                }
            }
            Kind::ToolCall | Kind::ToolResult => {
                if let Some(tool) = &ev.tool {
                    match turn.tools.get_mut(&tool.index) {
                        // A result updates the existing card in place, preserving the
                        // args the call carried and the result the call did not.
                        Some(existing) => {
                            existing.status = tool.status;
                            if tool.duration_ms.is_some() {
                                existing.duration_ms = tool.duration_ms;
                            }
                            if tool.mime.is_some() {
                                existing.mime.clone_from(&tool.mime);
                            }
                            if tool.body.is_some() {
                                existing.body.clone_from(&tool.body);
                            }
                            if tool.args.is_some() {
                                existing.args.clone_from(&tool.args);
                            }
                            if tool.preview.is_some() {
                                existing.preview.clone_from(&tool.preview);
                            }
                            existing.truncated |= tool.truncated;
                        }
                        None => {
                            turn.tools.insert(tool.index, tool.clone());
                            turn.tool_order.push(tool.index);
                        }
                    }
                }
            }
            Kind::Usage => turn.usage = ev.usage,
            Kind::Notice | Kind::ApprovalRequest | Kind::ApprovalResolved | Kind::ModelPicker => {}
            Kind::Unknown => {
                tracing::debug!(
                    session = %ev.session_id,
                    seq = ev.seq,
                    "ignoring unknown agent event kind"
                );
            }
        }
    }

    pub fn mark_degraded(&mut self, session_id: &str) {
        self.sessions
            .entry(session_id.to_owned())
            .or_insert_with(|| Session {
                session_id: session_id.to_owned(),
                ..Default::default()
            })
            .degraded = true;
    }

    /// Record whether the agent for a session is typing.
    ///
    /// Creates the session when setting the flag, because the whole point is the window
    /// before the first event arrives: on a fresh thread there is nothing to look up
    /// yet, and a lookup that returns `None` would silence exactly the case this
    /// exists for. Clearing an unknown session is a no-op -- there is nothing to clear.
    pub fn set_typing(&mut self, session_id: &str, typing: bool) {
        if typing {
            self.sessions
                .entry(session_id.to_owned())
                .or_insert_with(|| Session {
                    session_id: session_id.to_owned(),
                    ..Default::default()
                })
                .set_typing(true);
        } else if let Some(session) = self.sessions.get_mut(session_id) {
            session.set_typing(false);
        }
    }

    /// Expire prompts whose deadline has passed.
    ///
    /// Hermes times approvals out server-side (`MATRIX_APPROVAL_TIMEOUT_SECONDS`,
    /// default 300) but the resolution event can be lost. Without this a pane would
    /// stay `Blocked` for ever.
    pub fn expire_pending(&mut self, now_unix: u64) -> usize {
        let mut expired = 0;
        for session in self.sessions.values_mut() {
            let before = session.pending.len();
            session
                .pending
                .retain(|p| p.expires_at().is_none_or(|e| e > now_unix));
            expired += before - session.pending.len();
        }
        expired
    }

    /// Resolve a prompt locally, having sent the corresponding reaction.
    pub fn resolve_pending(&mut self, session_id: &str, id: &str, choice: ApprovalChoice) {
        let Some(session) = self.sessions.get_mut(session_id) else {
            return;
        };
        session.pending.retain(|p| p.id() != id);
        tracing::debug!(%session_id, %id, ?choice, "resolved prompt locally");
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use crate::protocol::SCHEMA_VERSION;

    fn ev(seq: u64, kind: Kind) -> AgentEvent {
        AgentEvent {
            v: SCHEMA_VERSION,
            session_id: "s1".into(),
            turn_id: "t1".into(),
            seq,
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

    fn tool(index: u32, status: ToolStatus) -> Tool {
        Tool {
            name: "bash".into(),
            index,
            args: None,
            preview: Some("cargo test".into()),
            status,
            duration_ms: None,
            mime: None,
            body: None,
            truncated: false,
        }
    }

    #[test]
    fn accumulates_cumulative_text_without_duplicating() {
        // Hermes re-sends the whole message on every progressive edit.
        let mut store = AgentStore::new();
        for (seq, text) in [(1, "Hello"), (2, "Hello wor"), (3, "Hello world")] {
            let mut e = ev(seq, Kind::MessageDelta);
            e.text = Some(text.into());
            store.apply(&e);
        }
        let s = store.get("s1").expect("session");
        assert_eq!(s.latest_turn().expect("turn").text, "Hello world");
    }

    #[test]
    fn appends_when_deltas_are_genuinely_incremental() {
        let mut store = AgentStore::new();
        for (seq, text) in [(1, "abc"), (2, "def")] {
            let mut e = ev(seq, Kind::MessageDelta);
            e.text = Some(text.into());
            store.apply(&e);
        }
        assert_eq!(
            store.get("s1").expect("s").latest_turn().expect("t").text,
            "abcdef"
        );
    }

    #[test]
    fn a_tool_result_updates_its_call_in_place() {
        let mut store = AgentStore::new();

        let mut call = ev(1, Kind::ToolCall);
        call.tool = Some(tool(0, ToolStatus::Running));
        store.apply(&call);
        assert_eq!(store.get("s1").expect("s").state(), AgentState::Working);

        let mut result = ev(2, Kind::ToolResult);
        result.tool = Some(Tool {
            status: ToolStatus::Ok,
            duration_ms: Some(1400),
            mime: Some("text/x-diff".into()),
            body: Some("--- a\n+++ b".into()),
            ..tool(0, ToolStatus::Ok)
        });
        store.apply(&result);

        let s = store.get("s1").expect("s");
        let t = s.latest_turn().expect("t");
        assert_eq!(t.tools().count(), 1, "result must not append a second card");
        let card = t.tools().next().expect("card");
        assert_eq!(card.status, ToolStatus::Ok);
        assert_eq!(card.duration_ms, Some(1400));
        // The args/preview from the call survive the result.
        assert_eq!(card.preview.as_deref(), Some("cargo test"));
        assert!(!t.has_running_tool());
    }

    #[test]
    fn blocked_outranks_working() {
        let mut store = AgentStore::new();
        let mut call = ev(1, Kind::ToolCall);
        call.tool = Some(tool(0, ToolStatus::Running));
        store.apply(&call);

        let mut req = ev(2, Kind::ApprovalRequest);
        req.approval = Some(Approval {
            id: "a1".into(),
            kind: "exec".into(),
            command: Some("rm -rf ./build".into()),
            cwd: None,
            expires_at: Some(9999),
            reactions: Default::default(),
            choice: None,
            by: None,
        });
        store.apply(&req);

        assert_eq!(store.get("s1").expect("s").state(), AgentState::Blocked);
    }

    #[test]
    fn resolution_unblocks() {
        let mut store = AgentStore::new();
        let mut req = ev(1, Kind::ApprovalRequest);
        req.approval = Some(Approval {
            id: "a1".into(),
            kind: "exec".into(),
            command: None,
            cwd: None,
            expires_at: None,
            reactions: Default::default(),
            choice: None,
            by: None,
        });
        store.apply(&req);
        assert_eq!(store.get("s1").expect("s").state(), AgentState::Blocked);

        let mut res = ev(2, Kind::ApprovalResolved);
        res.approval = Some(Approval {
            id: "a1".into(),
            kind: "exec".into(),
            command: None,
            cwd: None,
            expires_at: None,
            reactions: Default::default(),
            choice: Some(ApprovalChoice::Approve),
            by: Some("@quintin:example.org".into()),
        });
        store.apply(&res);
        assert!(store.get("s1").expect("s").pending.is_empty());
    }

    #[test]
    fn expired_prompts_do_not_block_for_ever() {
        let mut store = AgentStore::new();
        let mut req = ev(1, Kind::ApprovalRequest);
        req.approval = Some(Approval {
            id: "a1".into(),
            kind: "exec".into(),
            command: None,
            cwd: None,
            expires_at: Some(1_000),
            reactions: Default::default(),
            choice: None,
            by: None,
        });
        store.apply(&req);

        assert_eq!(store.expire_pending(999), 0, "not yet due");
        assert_eq!(store.expire_pending(1_001), 1);
        assert_ne!(store.get("s1").expect("s").state(), AgentState::Blocked);
    }

    #[test]
    fn done_collapses_to_idle_once_seen() {
        let mut store = AgentStore::new();
        let mut stop = ev(1, Kind::MessageStop);
        stop.final_ = Some(true);
        store.apply(&stop);
        assert_eq!(store.get("s1").expect("s").state(), AgentState::Done);

        store.get_mut("s1").expect("s").mark_seen();
        assert_eq!(store.get("s1").expect("s").state(), AgentState::Idle);
    }

    #[test]
    fn non_final_stop_is_only_a_segment_break() {
        let mut store = AgentStore::new();
        let mut stop = ev(1, Kind::MessageStop);
        stop.final_ = Some(false);
        store.apply(&stop);
        assert_eq!(
            store.get("s1").expect("s").state(),
            AgentState::Working,
            "text -> tool -> text must not look finished"
        );
    }

    #[test]
    fn detects_and_heals_sequence_gaps() {
        let mut store = AgentStore::new();
        store.apply(&ev(1, Kind::Commentary));
        store.apply(&ev(4, Kind::Commentary));

        let s = store.get("s1").expect("s");
        assert!(s.has_gaps());
        assert_eq!(s.latest_turn().expect("t").gaps, vec![2, 3]);

        store.apply(&ev(2, Kind::Commentary));
        store.apply(&ev(3, Kind::Commentary));
        assert!(!store.get("s1").expect("s").has_gaps());
    }

    #[test]
    fn rollup_takes_the_most_urgent_session() {
        let mut store = AgentStore::new();

        let mut a = ev(1, Kind::MessageStop);
        a.final_ = Some(true);
        store.apply(&a);

        let mut b = ev(1, Kind::ApprovalRequest);
        b.session_id = "s2".into();
        b.approval = Some(Approval {
            id: "a1".into(),
            kind: "exec".into(),
            command: None,
            cwd: None,
            expires_at: None,
            reactions: Default::default(),
            choice: None,
            by: None,
        });
        store.apply(&b);

        assert_eq!(store.rollup(), AgentState::Blocked);
        assert_eq!(store.count_in(AgentState::Done), 1);
        assert_eq!(store.count_in(AgentState::Blocked), 1);
    }

    #[test]
    fn typing_reads_as_working_before_the_first_token() {
        let mut store = AgentStore::new();
        store.apply(&ev(1, Kind::Commentary));
        store.set_typing("s1", true);
        assert_eq!(store.get("s1").expect("s").state(), AgentState::Working);
    }

    #[test]
    fn typing_creates_the_session_it_is_about() {
        // The window this exists for is the one before any event has arrived, so a
        // lookup that requires an existing session would miss every first turn.
        let mut store = AgentStore::new();
        store.set_typing("s1", true);
        assert_eq!(store.get("s1").expect("s").state(), AgentState::Working);

        store.set_typing("s1", false);
        assert_eq!(store.get("s1").expect("s").state(), AgentState::Idle);
    }

    #[test]
    fn clearing_typing_does_not_conjure_a_session() {
        let mut store = AgentStore::new();
        store.set_typing("s1", false);
        assert!(store.get("s1").is_none());
    }
}
