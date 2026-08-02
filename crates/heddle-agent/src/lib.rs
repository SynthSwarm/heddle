//! Agent event protocol for heddle.
//!
//! Three layers, in decreasing order of fidelity:
//!
//! 1. [`protocol`] — the `dev.hermes.agent.v1` wire format. Structured tool calls,
//!    results, diffs, approvals and usage. Requires the Hermes patch described in
//!    `docs/SPEC.md` §3.3.
//! 2. [`fallback`] — best-effort recovery from human-readable tool chrome, for agents
//!    that do not emit the extension. Structurally lossy.
//! 3. [`store`] — turn assembly and the derived [`store::AgentState`] machine that
//!    drives pane, tab and workspace badges.
//!
//! This crate is deliberately free of any Matrix dependency: it operates on
//! `serde_json::Value` content objects so it can be tested without a homeserver.

pub mod fallback;
pub mod protocol;
pub mod store;

pub use protocol::{
    decode, is_agent_event, AgentEvent, AgentInfo, Approval, ApprovalChoice, DecodeError, Kind,
    Notice, Picker, PickerOption, ResultKind, Tool, ToolStatus, Usage, CONTENT_KEY, SCHEMA_VERSION,
};
pub use store::{AgentState, AgentStore, Pending, Session, Turn};

/// Outcome of feeding one message body through both layers.
#[derive(Debug, Clone)]
pub enum Ingest {
    /// A structured event was present. Full fidelity.
    Structured(Box<AgentEvent>),
    /// No extension, but agent-shaped chrome was recovered from the text.
    Degraded(fallback::Parsed),
    /// An ordinary message. Render as chat.
    Plain,
}

/// Decode a message content object, falling back to text parsing.
///
/// `body` is the human-readable `m.room.message` body, used only when the extension is
/// absent. Pass `fallback_enabled = false` to disable the lossy path entirely.
pub fn ingest(content: &serde_json::Value, body: &str, fallback_enabled: bool) -> Ingest {
    match decode(content) {
        Ok(ev) => return Ingest::Structured(Box::new(ev)),
        Err(DecodeError::Absent) => {}
        Err(e) => {
            // A malformed or future-versioned event is not fatal: the human-readable
            // body is still on the wire, so fall through and render what we can.
            tracing::warn!(error = %e, "agent event present but unusable; falling back");
        }
    }

    if !fallback_enabled {
        return Ingest::Plain;
    }

    let parsed = fallback::parse(body);
    if parsed.is_empty() {
        Ingest::Plain
    } else {
        Ingest::Degraded(parsed)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn prefers_the_structured_path() {
        let content = json!({
            "body": "🔧 edit: \"src/main.rs\"",
            CONTENT_KEY: {
                "v": 1, "session_id": "s", "turn_id": "t", "seq": 1,
                "kind": "tool.call",
                "tool": { "name": "edit", "index": 0, "status": "running" }
            }
        });
        assert!(matches!(
            ingest(&content, "🔧 edit: \"src/main.rs\"", true),
            Ingest::Structured(_)
        ));
    }

    #[test]
    fn falls_back_when_the_extension_is_absent() {
        let content = json!({ "body": "🔧 edit: \"src/main.rs\"" });
        match ingest(&content, "🔧 edit: \"src/main.rs\"", true) {
            Ingest::Degraded(p) => assert_eq!(p.tools.len(), 1),
            other => panic!("expected degraded, got {other:?}"),
        }
    }

    #[test]
    fn ordinary_chat_stays_plain() {
        let content = json!({ "body": "morning" });
        assert!(matches!(ingest(&content, "morning", true), Ingest::Plain));
    }

    #[test]
    fn fallback_can_be_switched_off() {
        let content = json!({ "body": "🔧 edit: \"x\"" });
        assert!(matches!(
            ingest(&content, "🔧 edit: \"x\"", false),
            Ingest::Plain
        ));
    }

    #[test]
    fn a_future_schema_degrades_rather_than_dropping_the_message() {
        let content = json!({
            "body": "🔧 edit: \"src/main.rs\"",
            CONTENT_KEY: { "v": 99, "session_id": "s", "turn_id": "t", "seq": 1, "kind": "tool.call" }
        });
        match ingest(&content, "🔧 edit: \"src/main.rs\"", true) {
            Ingest::Degraded(p) => assert_eq!(p.tools.len(), 1),
            other => panic!("expected degraded, got {other:?}"),
        }
    }
}
