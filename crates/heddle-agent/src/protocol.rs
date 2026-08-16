//! The `dev.heddle.agent.v1` wire format.
//!
//! Hermes carries a rich internal stream ([`MessageChunk`], [`ToolCallChunk`],
//! [`ToolCallFinished`] and friends in `gateway/stream_events.py`) but flattens it to a
//! human string at the Matrix boundary:
//!
//! ```text
//! f"{emoji} {event.tool_name}: \"{preview}\""
//! ```
//!
//! That discards tool results, exit codes, durations and token usage. This module
//! defines the namespaced content key that carries the structure alongside the
//! human-readable `body`, so other Matrix clients are unaffected.
//!
//! See `docs/SPEC.md` §3.
//!
//! [`MessageChunk`]: Kind::MessageDelta
//! [`ToolCallChunk`]: Kind::ToolCall
//! [`ToolCallFinished`]: Kind::ToolResult

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The reverse-DNS content key an agent event is attached under.
pub const CONTENT_KEY: &str = "dev.heddle.agent.v1";

/// The key Hermes' own patch was specified against, accepted for compatibility.
///
/// The schema is heddle's, not any one agent's, and a key named after the first agent
/// to carry it discourages the second from adopting it. Both keys are read;
/// [`CONTENT_KEY`] is what anything heddle documents should write.
pub const LEGACY_CONTENT_KEY: &str = "dev.hermes.agent.v1";

/// The schema major this build understands. Envelopes with a higher `v` are rejected
/// rather than misinterpreted.
pub const SCHEMA_VERSION: u32 = 1;

/// A single structured agent event, read from the [`CONTENT_KEY`] of an
/// `m.room.message` content (or of its `m.new_content` when the event is an edit).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentEvent {
    /// Schema version. See [`SCHEMA_VERSION`].
    pub v: u32,
    /// Hermes session key. Stable for the lifetime of a pane.
    pub session_id: String,
    /// ULID identifying one user-turn. Groups every event of a single response.
    pub turn_id: String,
    /// Monotonic within a turn. Used for ordering and gap detection.
    pub seq: u64,
    pub kind: Kind,

    /// Sent on the first event of a turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentInfo>,

    /// Incremental assistant text. Present for [`Kind::MessageDelta`] and
    /// [`Kind::Commentary`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,

    /// Whether a [`Kind::MessageStop`] terminates the turn or is only a segment break
    /// (text -> tool -> text).
    #[serde(rename = "final", default, skip_serializing_if = "Option::is_none")]
    pub final_: Option<bool>,

    /// Present for [`Kind::ToolCall`] and [`Kind::ToolResult`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<Tool>,

    /// Present for [`Kind::Notice`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<Notice>,

    /// Present for [`Kind::ApprovalRequest`] and [`Kind::ApprovalResolved`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<Approval>,

    /// Present for [`Kind::ModelPicker`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picker: Option<Picker>,

    /// Present for [`Kind::Usage`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// Discriminant for [`AgentEvent`].
///
/// Unknown values deserialise to [`Kind::Unknown`] rather than failing, so that a newer
/// Hermes emitting an additional kind degrades to "ignored" instead of "breaks the
/// timeline".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[serde(rename = "message.delta")]
    MessageDelta,
    #[serde(rename = "message.stop")]
    MessageStop,
    Commentary,
    #[serde(rename = "tool.call")]
    ToolCall,
    #[serde(rename = "tool.result")]
    ToolResult,
    Notice,
    #[serde(rename = "approval.request")]
    ApprovalRequest,
    #[serde(rename = "approval.resolved")]
    ApprovalResolved,
    #[serde(rename = "model.picker")]
    ModelPicker,
    Usage,
    #[serde(other)]
    Unknown,
}

/// Identity of the responding agent. Sent once per turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// A tool invocation or its result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    /// Index of this call within the turn. Pairs a `tool.result` to its `tool.call`.
    #[serde(default)]
    pub index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(default)]
    pub status: ToolStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// MIME type of `body`. Drives which renderer is used; see [`ResultKind`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    #[default]
    Running,
    Ok,
    Error,
}

impl ToolStatus {
    /// Whether the call has finished, successfully or otherwise.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Ok | Self::Error)
    }
}

/// How a [`Tool`] result body should be rendered, resolved from its MIME type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultKind {
    /// Unified diff. Rendered with an add/delete gutter and syntax highlighting.
    Diff,
    /// Pretty-printed and folded past a threshold.
    Json,
    /// Rendered as a monospace block; the markdown renderer is not wired to tool
    /// bodies yet.
    Markdown,
    /// Monospace block, folded past a threshold.
    Plain,
}

impl Tool {
    pub fn result_kind(&self) -> ResultKind {
        match self.mime.as_deref() {
            Some("text/x-diff" | "text/x-patch" | "application/x-patch") => ResultKind::Diff,
            Some("application/json" | "text/json") => ResultKind::Json,
            Some("text/markdown" | "text/x-markdown") => ResultKind::Markdown,
            _ => ResultKind::Plain,
        }
    }

    /// A one-line summary for the collapsed card header.
    ///
    /// Prefers the explicit `preview`, falling back to the first scalar argument so a
    /// card is never headerless.
    pub fn summary(&self) -> String {
        if let Some(p) = self.preview.as_deref().filter(|p| !p.is_empty()) {
            return p.to_owned();
        }
        let Some(serde_json::Value::Object(map)) = &self.args else {
            return String::new();
        };
        map.values()
            .find_map(|v| match v {
                serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .unwrap_or_default()
    }
}

/// A `GatewayNotice` passed through from Hermes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notice {
    pub kind: String,
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// A human-in-the-loop approval prompt.
///
/// Hermes drives these over `m.reaction` (see `send_exec_approval` in
/// `gateway/platforms/matrix.py`); heddle renders them as a keypress with a countdown
/// and sends the corresponding reaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Approval {
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Unix seconds. Absent means no timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// Emoji Hermes accepts, mapped to the choice they represent.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reactions: BTreeMap<String, String>,
    /// Set on `approval.resolved`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<ApprovalChoice>,
    /// Matrix user who resolved it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalChoice {
    Approve,
    Deny,
    Timeout,
}

/// A `/model` selection prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Picker {
    pub id: String,
    #[serde(default)]
    pub options: Vec<PickerOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickerOption {
    /// The reaction emoji that selects this option.
    pub key: String,
    pub label: String,
}

/// Token accounting for a turn.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// Why an event could not be decoded.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    /// The content had no `dev.hermes.agent.v1` key. Expected for ordinary messages.
    #[error("no agent event present")]
    Absent,
    /// The envelope declares a schema major this build does not understand. The event
    /// is skipped; the human-readable `body` still renders.
    #[error("unsupported schema version {found} (this build understands {SCHEMA_VERSION})")]
    UnsupportedVersion { found: u32 },
    /// The key was present but malformed.
    #[error("malformed agent event: {0}")]
    Malformed(String),
}

/// Extract an [`AgentEvent`] from an `m.room.message` content object.
///
/// Handles the edit case transparently: when the content is an `m.replace` the
/// authoritative payload lives in `m.new_content`, so that is preferred when present.
///
/// Reads [`CONTENT_KEY`] and then [`LEGACY_CONTENT_KEY`]. An adapter written for one
/// specific agent should call [`decode_under`] with that agent's key instead, so that
/// two agents sharing a room are never confused for one another.
pub fn decode(content: &serde_json::Value) -> Result<AgentEvent, DecodeError> {
    match decode_under(content, CONTENT_KEY) {
        Err(DecodeError::Absent) => decode_under(content, LEGACY_CONTENT_KEY),
        other => other,
    }
}

/// Extract an [`AgentEvent`] from one named key of a content object.
pub fn decode_under(content: &serde_json::Value, key: &str) -> Result<AgentEvent, DecodeError> {
    let raw = content
        .get("m.new_content")
        .and_then(|c| c.get(key))
        .or_else(|| content.get(key))
        .ok_or(DecodeError::Absent)?;

    // Check the version before full deserialisation so a future schema produces a
    // precise error rather than a confusing field-level one.
    match raw.get("v").and_then(serde_json::Value::as_u64) {
        Some(v) if v as u32 > SCHEMA_VERSION => {
            return Err(DecodeError::UnsupportedVersion { found: v as u32 })
        }
        Some(_) => {}
        None => return Err(DecodeError::Malformed("missing `v`".into())),
    }

    serde_json::from_value(raw.clone()).map_err(|e| DecodeError::Malformed(e.to_string()))
}

/// Whether a content object carries an agent event at all.
///
/// Cheaper than [`decode`] when all that is needed is a routing decision.
pub fn is_agent_event(content: &serde_json::Value) -> bool {
    [CONTENT_KEY, LEGACY_CONTENT_KEY].iter().any(|key| {
        content.get(key).is_some()
            || content
                .get("m.new_content")
                .is_some_and(|c| c.get(key).is_some())
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    fn tool_call() -> serde_json::Value {
        json!({
            "msgtype": "m.notice",
            "body": "🔧 edit: \"src/main.rs\"",
            CONTENT_KEY: {
                "v": 1,
                "session_id": "proj-b/thread-$root",
                "turn_id": "01JQ8XKQ2W9YHVB0ZC7T5N3E4M",
                "seq": 12,
                "kind": "tool.call",
                "tool": {
                    "name": "edit",
                    "index": 0,
                    "args": { "path": "src/main.rs" },
                    "preview": "src/main.rs",
                    "status": "running"
                }
            }
        })
    }

    #[test]
    fn decodes_a_tool_call() {
        let ev = decode(&tool_call()).expect("decodes");
        assert_eq!(ev.kind, Kind::ToolCall);
        assert_eq!(ev.seq, 12);
        let tool = ev.tool.expect("tool present");
        assert_eq!(tool.name, "edit");
        assert_eq!(tool.status, ToolStatus::Running);
        assert_eq!(tool.summary(), "src/main.rs");
    }

    #[test]
    fn absent_key_is_not_an_error_condition() {
        let content = json!({ "msgtype": "m.text", "body": "hello" });
        assert_eq!(decode(&content), Err(DecodeError::Absent));
        assert!(!is_agent_event(&content));
    }

    #[test]
    fn prefers_new_content_on_edits() {
        // Hermes streams by progressively editing one event. The authoritative payload
        // is in m.new_content; the outer key may be a stale first frame.
        let content = json!({
            "msgtype": "m.text",
            "body": "* updated",
            CONTENT_KEY: { "v": 1, "session_id": "s", "turn_id": "t", "seq": 1, "kind": "message.delta", "text": "stale" },
            "m.new_content": {
                "msgtype": "m.text",
                "body": "updated",
                CONTENT_KEY: { "v": 1, "session_id": "s", "turn_id": "t", "seq": 9, "kind": "message.delta", "text": "fresh" }
            },
            "m.relates_to": { "rel_type": "m.replace", "event_id": "$abc" }
        });
        let ev = decode(&content).expect("decodes");
        assert_eq!(ev.seq, 9);
        assert_eq!(ev.text.as_deref(), Some("fresh"));
    }

    #[test]
    fn rejects_a_future_schema_rather_than_guessing() {
        let content = json!({
            CONTENT_KEY: { "v": 2, "session_id": "s", "turn_id": "t", "seq": 1, "kind": "message.delta" }
        });
        assert_eq!(
            decode(&content),
            Err(DecodeError::UnsupportedVersion { found: 2 })
        );
    }

    #[test]
    fn unknown_kinds_degrade_instead_of_failing() {
        // A newer Hermes emitting an extra kind must not break the timeline.
        let content = json!({
            CONTENT_KEY: { "v": 1, "session_id": "s", "turn_id": "t", "seq": 1, "kind": "some.future.kind" }
        });
        assert_eq!(decode(&content).expect("decodes").kind, Kind::Unknown);
    }

    #[test]
    fn resolves_result_renderers_from_mime() {
        let cases = [
            (Some("text/x-diff"), ResultKind::Diff),
            (Some("application/json"), ResultKind::Json),
            (Some("text/markdown"), ResultKind::Markdown),
            (Some("text/plain"), ResultKind::Plain),
            (None, ResultKind::Plain),
        ];
        for (mime, want) in cases {
            let tool = Tool {
                name: "t".into(),
                index: 0,
                args: None,
                preview: None,
                status: ToolStatus::Ok,
                duration_ms: None,
                mime: mime.map(str::to_owned),
                body: None,
                truncated: false,
            };
            assert_eq!(tool.result_kind(), want, "mime {mime:?}");
        }
    }

    #[test]
    fn summary_falls_back_to_first_scalar_arg() {
        let tool = Tool {
            name: "bash".into(),
            index: 0,
            args: Some(json!({ "command": "cargo test" })),
            preview: None,
            status: ToolStatus::Running,
            duration_ms: None,
            mime: None,
            body: None,
            truncated: false,
        };
        assert_eq!(tool.summary(), "cargo test");
    }
}
