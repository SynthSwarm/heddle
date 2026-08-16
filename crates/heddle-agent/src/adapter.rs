//! Pluggable agent integrations.
//!
//! heddle is an agent client, not a Hermes client. An agent speaks to it in one of two
//! registers:
//!
//! * **Structured** — a [`crate::protocol`] envelope under a content key. Lossless, and
//!   requires the agent to have been taught to emit it.
//! * **Textual** — the human-readable tool chrome the agent already prints. Lossy, and
//!   requires nothing of the agent at all.
//!
//! An [`Adapter`] is one agent's answer to those two questions. [`Adapters`] holds an
//! ordered set of them and asks each in turn.
//!
//! Adding an agent is a table and a name, not a parser: see [`crate::fallback::Chrome`].
//! The reason for the indirection is that heddle expects to meet agents it cannot
//! change — an unpatched Hermes today, OpenCode or OpenClaw tomorrow — and the only
//! thing those have in common is that they print *something* a human was meant to read.
//!
//! See `docs/SPEC.md` §3.

use crate::fallback::{self, Chrome, Parsed};
use crate::protocol::{decode_under, AgentEvent, DecodeError, CONTENT_KEY, LEGACY_CONTENT_KEY};
use serde_json::Value;

/// One agent integration.
///
/// Implementors are expected to be cheap and stateless: one is consulted per message.
pub trait Adapter: Send + Sync + std::fmt::Debug {
    /// Short, stable identifier, used in config and in diagnostics.
    fn id(&self) -> &'static str;

    /// Read a structured event from message content.
    ///
    /// `Ok(None)` means "not mine, try someone else". `Err` means "mine, but I cannot
    /// read it", which is worth reporting before falling through to the text path —
    /// the human-readable body is still on the wire either way.
    fn structured(&self, _content: &Value) -> Result<Option<AgentEvent>, DecodeError> {
        Ok(None)
    }

    /// Recover what can be recovered from the human-readable body.
    ///
    /// Return [`Parsed::default`] to decline. Note that declining and recovering
    /// nothing are the same thing, deliberately: an adapter that finds no tool calls
    /// has no opinion worth acting on.
    fn textual(&self, _body: &str) -> Parsed {
        Parsed::default()
    }
}

/// The agent whose schema this is.
///
/// Structured only. Any agent that adopts the published envelope is understood without
/// heddle needing to know anything else about it, which is the point of writing the
/// schema down in `SPEC.md` §3 rather than keeping it between two programs.
#[derive(Debug, Default, Clone, Copy)]
pub struct Heddle;

impl Adapter for Heddle {
    fn id(&self) -> &'static str {
        "heddle"
    }

    fn structured(&self, content: &Value) -> Result<Option<AgentEvent>, DecodeError> {
        absent_is_none(decode_under(content, CONTENT_KEY))
    }
}

/// Hermes, unpatched.
///
/// Reads the legacy content key its patch was specified against, and recovers tool
/// chrome from `format_tool_event` output when the patch is absent — which, since the
/// fork was never merged, is every Hermes in existence.
#[derive(Debug, Default, Clone, Copy)]
pub struct Hermes;

impl Adapter for Hermes {
    fn id(&self) -> &'static str {
        "hermes"
    }

    fn structured(&self, content: &Value) -> Result<Option<AgentEvent>, DecodeError> {
        absent_is_none(decode_under(content, LEGACY_CONTENT_KEY))
    }

    fn textual(&self, body: &str) -> Parsed {
        fallback::parse_with(body, Chrome::HERMES)
    }
}

/// An adapter assembled from a name and a chrome table.
///
/// The escape hatch for an agent whose output fits the shapes heddle already knows but
/// which has no adapter of its own — configured rather than compiled.
#[derive(Debug, Clone)]
pub struct Textual {
    id: &'static str,
    chrome: Chrome,
}

impl Textual {
    pub const fn new(id: &'static str, chrome: Chrome) -> Self {
        Self { id, chrome }
    }
}

impl Adapter for Textual {
    fn id(&self) -> &'static str {
        self.id
    }

    fn textual(&self, body: &str) -> Parsed {
        fallback::parse_with(body, self.chrome)
    }
}

fn absent_is_none(
    result: Result<AgentEvent, DecodeError>,
) -> Result<Option<AgentEvent>, DecodeError> {
    match result {
        Ok(event) => Ok(Some(event)),
        Err(DecodeError::Absent) => Ok(None),
        Err(other) => Err(other),
    }
}

/// Outcome of feeding one message through the registered adapters.
#[derive(Debug, Clone)]
pub enum Ingest {
    /// A structured event was present. Full fidelity.
    Structured {
        /// Which adapter understood it.
        adapter: &'static str,
        event: Box<AgentEvent>,
    },
    /// No extension, but agent-shaped chrome was recovered from the text.
    Degraded {
        adapter: &'static str,
        parsed: Parsed,
    },
    /// An ordinary message. Render as chat.
    Plain,
}

impl Ingest {
    /// The adapter that claimed this message, if any did.
    pub fn adapter(&self) -> Option<&'static str> {
        match self {
            Self::Structured { adapter, .. } | Self::Degraded { adapter, .. } => Some(adapter),
            Self::Plain => None,
        }
    }
}

/// The registered agent integrations, in the order they are consulted.
#[derive(Debug)]
pub struct Adapters {
    adapters: Vec<Box<dyn Adapter>>,
    /// Whether the lossy text path may be used at all.
    textual_enabled: bool,
}

impl Default for Adapters {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapters {
    /// The built-in set: the published schema first, then Hermes.
    ///
    /// Order matters only for structured decoding, and only when two adapters would
    /// both claim the same content — which they cannot here, since each reads its own
    /// key. It is fixed rather than arbitrary so that diagnostics are reproducible.
    pub fn new() -> Self {
        Self {
            adapters: vec![Box::new(Heddle), Box::new(Hermes)],
            textual_enabled: true,
        }
    }

    /// An empty set. Every message is [`Ingest::Plain`].
    pub fn none() -> Self {
        Self {
            adapters: Vec::new(),
            textual_enabled: true,
        }
    }

    /// Select built-in adapters by id, keeping the order given.
    ///
    /// Unknown ids are reported and skipped rather than failing: a typo in a config
    /// file should cost one integration, not the whole client.
    pub fn by_id<'a>(ids: impl IntoIterator<Item = &'a str>) -> Self {
        let mut set = Self::none();
        for id in ids {
            match id {
                "heddle" => set.adapters.push(Box::new(Heddle)),
                "hermes" => set.adapters.push(Box::new(Hermes)),
                other => tracing::warn!(adapter = %other, "unknown agent adapter; ignoring"),
            }
        }
        set
    }

    /// Register another adapter at the end of the order.
    #[must_use]
    pub fn with(mut self, adapter: impl Adapter + 'static) -> Self {
        self.adapters.push(Box::new(adapter));
        self
    }

    /// Turn the lossy text path on or off for every adapter at once.
    #[must_use]
    pub fn textual(mut self, enabled: bool) -> Self {
        self.textual_enabled = enabled;
        self
    }

    /// The ids of the registered adapters, in order.
    pub fn ids(&self) -> Vec<&'static str> {
        self.adapters.iter().map(|a| a.id()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }

    /// Decode one message, structured path first.
    ///
    /// Every adapter gets a chance at the structured path before any of them is asked
    /// about text, because a lossless read by the second adapter beats a lossy one by
    /// the first.
    pub fn ingest(&self, content: &Value, body: &str) -> Ingest {
        for adapter in &self.adapters {
            match adapter.structured(content) {
                Ok(Some(event)) => {
                    return Ingest::Structured {
                        adapter: adapter.id(),
                        event: Box::new(event),
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    // A malformed or future-versioned event is not fatal: the
                    // human-readable body is still on the wire, so fall through and
                    // render what we can.
                    tracing::warn!(
                        adapter = adapter.id(),
                        %error,
                        "agent event present but unusable; falling back to the text"
                    );
                }
            }
        }

        if !self.textual_enabled {
            return Ingest::Plain;
        }

        for adapter in &self.adapters {
            let parsed = adapter.textual(body);
            if !parsed.is_empty() {
                return Ingest::Degraded {
                    adapter: adapter.id(),
                    parsed,
                };
            }
        }

        Ingest::Plain
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use crate::protocol::SCHEMA_VERSION;
    use serde_json::json;

    fn envelope(key: &str) -> Value {
        json!({
            "body": "🔧 edit: \"src/main.rs\"",
            key: {
                "v": SCHEMA_VERSION, "session_id": "s", "turn_id": "t", "seq": 1,
                "kind": "tool.call",
                "tool": { "name": "edit", "index": 0, "status": "running" }
            }
        })
    }

    #[test]
    fn the_published_key_is_understood_without_naming_an_agent() {
        let set = Adapters::new();
        match set.ingest(&envelope(CONTENT_KEY), "chrome") {
            Ingest::Structured { adapter, .. } => assert_eq!(adapter, "heddle"),
            other => panic!("expected structured, got {other:?}"),
        }
    }

    #[test]
    fn the_legacy_hermes_key_still_works() {
        // The patch was specified against it, and anything already emitting it should
        // not stop being understood because the schema was given a neutral name.
        let set = Adapters::new();
        match set.ingest(&envelope(LEGACY_CONTENT_KEY), "chrome") {
            Ingest::Structured { adapter, .. } => assert_eq!(adapter, "hermes"),
            other => panic!("expected structured, got {other:?}"),
        }
    }

    #[test]
    fn chrome_is_recovered_when_no_extension_is_present() {
        let set = Adapters::new();
        let content = json!({ "body": "🔧 edit: \"src/main.rs\"" });
        match set.ingest(&content, "🔧 edit: \"src/main.rs\"") {
            Ingest::Degraded { adapter, parsed } => {
                assert_eq!(adapter, "hermes");
                assert_eq!(parsed.tools.len(), 1);
            }
            other => panic!("expected degraded, got {other:?}"),
        }
    }

    #[test]
    fn a_lossless_read_beats_a_lossy_one_whoever_offers_it() {
        // Hermes is asked about text before heddle is, but heddle's structured read
        // must still win: fidelity outranks registration order.
        let set = Adapters::none()
            .with(Textual::new("eager", Chrome::HERMES))
            .with(Heddle);
        match set.ingest(&envelope(CONTENT_KEY), "🔧 edit: \"x\"") {
            Ingest::Structured { adapter, .. } => assert_eq!(adapter, "heddle"),
            other => panic!("expected structured, got {other:?}"),
        }
    }

    #[test]
    fn ordinary_chat_belongs_to_nobody() {
        let set = Adapters::new();
        let content = json!({ "body": "morning" });
        assert!(matches!(set.ingest(&content, "morning"), Ingest::Plain));
        assert_eq!(set.ingest(&content, "morning").adapter(), None);
    }

    #[test]
    fn the_lossy_path_can_be_switched_off_without_losing_the_lossless_one() {
        let set = Adapters::new().textual(false);
        let chrome = json!({ "body": "🔧 edit: \"x\"" });
        assert!(matches!(
            set.ingest(&chrome, "🔧 edit: \"x\""),
            Ingest::Plain
        ));
        assert!(matches!(
            set.ingest(&envelope(CONTENT_KEY), "🔧 edit: \"x\""),
            Ingest::Structured { .. }
        ));
    }

    #[test]
    fn a_future_schema_degrades_rather_than_dropping_the_message() {
        let content = json!({
            "body": "🔧 edit: \"src/main.rs\"",
            CONTENT_KEY: { "v": 99, "session_id": "s", "turn_id": "t", "seq": 1, "kind": "tool.call" }
        });
        match Adapters::new().ingest(&content, "🔧 edit: \"src/main.rs\"") {
            Ingest::Degraded { parsed, .. } => assert_eq!(parsed.tools.len(), 1),
            other => panic!("expected degraded, got {other:?}"),
        }
    }

    #[test]
    fn an_agent_can_be_added_without_touching_this_crate() {
        // The whole point of the indirection: a new integration is a table and a name.
        #[derive(Debug)]
        struct Bracketed;
        impl Adapter for Bracketed {
            fn id(&self) -> &'static str {
                "bracketed"
            }
            fn textual(&self, body: &str) -> Parsed {
                fallback::parse_with(
                    body,
                    Chrome {
                        leading_emoji: false,
                        preview: true,
                        ellipsis: false,
                        call: false,
                    },
                )
            }
        }

        let set = Adapters::none().with(Bracketed);
        match set.ingest(&json!({}), "edit: \"src/main.rs\"") {
            Ingest::Degraded { adapter, parsed } => {
                assert_eq!(adapter, "bracketed");
                assert_eq!(parsed.tools[0].name, "edit");
            }
            other => panic!("expected degraded, got {other:?}"),
        }

        // And the built-in set, which requires the emoji, declines the same line.
        assert!(matches!(
            Adapters::new().ingest(&json!({}), "edit: \"src/main.rs\""),
            Ingest::Plain
        ));
    }

    #[test]
    fn adapters_are_selected_by_name_and_a_typo_costs_only_itself() {
        let set = Adapters::by_id(["hermes", "nonsense"]);
        assert_eq!(set.ids(), vec!["hermes"]);

        let empty = Adapters::by_id(std::iter::empty());
        assert!(empty.is_empty());
        assert!(matches!(
            empty.ingest(&envelope(CONTENT_KEY), "🔧 edit: \"x\""),
            Ingest::Plain
        ));
    }
}
