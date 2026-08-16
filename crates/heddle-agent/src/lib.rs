//! Agent event protocol for heddle.
//!
//! heddle is an agent client rather than a client for any one agent. What it needs from
//! an agent is structure — which tool ran, what it returned, whether a human is being
//! waited on — and agents supply that at two very different fidelities:
//!
//! 1. [`protocol`] — the `dev.heddle.agent.v1` wire format. Structured tool calls,
//!    results, diffs, approvals and usage, carried beside the human-readable body so
//!    that other Matrix clients still show something sensible. Lossless, and requires
//!    the agent to emit it.
//! 2. [`fallback`] — best-effort recovery from the tool chrome an agent already prints
//!    for humans. Structurally lossy, and requires nothing of the agent at all. This is
//!    the only path in use today, since no agent yet emits the extension.
//!
//! [`adapter`] binds the two together: an [`adapter::Adapter`] is one agent's answer to
//! both questions, and [`adapter::Adapters`] is the ordered set heddle consults. Hermes
//! is the first integration; adding a second is a chrome table and a name rather than
//! another parser.
//!
//! [`store`] then folds whatever came out into turns and the derived
//! [`store::AgentState`] that drives pane, tab and workspace badges — and it neither
//! knows nor cares which adapter produced the input.
//!
//! This crate is deliberately free of any Matrix dependency: it operates on
//! `serde_json::Value` content objects so it can be tested without a homeserver.

pub mod adapter;
pub mod fallback;
pub mod protocol;
pub mod store;

pub use adapter::{Adapter, Adapters, Ingest};
pub use fallback::Chrome;
pub use protocol::{
    decode, decode_under, is_agent_event, AgentEvent, AgentInfo, Approval, ApprovalChoice,
    DecodeError, Kind, Notice, Picker, PickerOption, ResultKind, Tool, ToolStatus, Usage,
    CONTENT_KEY, LEGACY_CONTENT_KEY, SCHEMA_VERSION,
};
pub use store::{AgentState, AgentStore, Pending, Session, Turn};
