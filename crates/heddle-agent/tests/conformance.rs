//! Conformance: the opencode plugin's output against this crate's reader.
//!
//! The fixtures under `plugins/opencode/test/fixtures` are recordings of the emitter, not
//! hand-written JSON — a fixture somebody typed proves only that they read the schema the
//! same way twice. Regenerate them with `npm run fixtures` in `plugins/opencode`.
//!
//! Two failures are worth telling apart:
//!
//!   * a fixture that no longer decodes means the emitter and this crate have diverged;
//!   * a fixture that decodes but leaves the store unhappy means they agree about the
//!     bytes and disagree about the meaning, which is the more expensive kind and the
//!     reason this test folds them through [`AgentStore`] rather than stopping at serde.

// Same allowance the in-crate test modules take: in a test a panic is the report, and
// threading Results through assertions would obscure what is being asserted.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use heddle_agent::protocol::{AgentEvent, Kind, ToolStatus};
use heddle_agent::store::{AgentState, AgentStore};
use std::collections::BTreeSet;
use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/opencode/test/fixtures")
        .canonicalize()
        .expect("fixtures directory; run `npm run fixtures` in plugins/opencode")
}

fn read(path: &PathBuf) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn manifest() -> serde_json::Value {
    serde_json::from_str(&read(&fixture_dir().join("manifest.json"))).expect("manifest parses")
}

/// Every recorded frame decodes, including the ones only a live reader sees.
#[test]
fn every_frame_decodes() {
    let dir = fixture_dir();
    let m = manifest();
    let frames = m["frames"].as_array().expect("frames");
    assert!(!frames.is_empty(), "no frames recorded");

    for frame in frames {
        let file = frame["file"].as_str().expect("file");
        let raw = read(&dir.join(file));

        let ev: AgentEvent = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("{file} does not decode: {e}\nthe emitter has diverged"));

        assert_eq!(ev.v, 1, "{file}: schema version");
        assert!(
            !matches!(ev.kind, Kind::Unknown),
            "{file}: decoded as Kind::Unknown — the emitter writes a kind this crate has \
             no variant for"
        );
        assert_eq!(
            format!("{:?}", ev.kind).to_lowercase().replace('_', ""),
            frame["kind"]
                .as_str()
                .expect("kind")
                .replace(['.', '_'], "")
                .to_lowercase(),
            "{file}: manifest kind disagrees with the payload"
        );
        assert_eq!(ev.seq, frame["seq"].as_u64().expect("seq"), "{file}: seq");
        assert!(!ev.session_id.is_empty(), "{file}: session_id");
        assert!(!ev.turn_id.is_empty(), "{file}: turn_id");

        // Round trip asserted semantically. Byte equality is the wrong property: several
        // fields are `#[serde(default)]` without `skip_serializing_if`, so this crate
        // writes them even when a correct emitter omits them.
        let back = serde_json::to_value(&ev).expect("re-serialise");
        let again: AgentEvent = serde_json::from_value(back).expect("re-read");
        assert_eq!(again, ev, "{file}: round trip is not stable");
    }
}

/// The fixture set must not quietly stop covering a kind.
#[test]
fn the_fixtures_cover_what_the_emitter_can_send() {
    let m = manifest();
    let kinds: BTreeSet<String> = m["frames"]
        .as_array()
        .expect("frames")
        .iter()
        .map(|f| f["kind"].as_str().expect("kind").to_owned())
        .collect();

    for required in [
        "message.delta",
        "commentary",
        "tool.call",
        "tool.result",
        "approval.request",
        "approval.resolved",
        "usage",
        "message.stop",
    ] {
        assert!(
            kinds.contains(required),
            "no fixture covers `{required}`; regenerate with `npm run fixtures`"
        );
    }
}

/// Fold the resolved view through the real store and insist the pane is clean.
///
/// This is the assertion that matters. Decoding proves the bytes are understood; this
/// proves the emitter's sequencing produces a session that reports itself finished, with
/// no gaps and one card per tool — the three things that were each got wrong at least
/// once while the emitter was being written.
#[test]
fn the_resolved_view_produces_a_clean_session() {
    let dir = fixture_dir();
    let m = manifest();

    // Rebuild the resolved timeline: the last frame written for each event wins, which is
    // what a client reading the room fresh is handed.
    let frames = m["frames"].as_array().expect("frames");
    let resolved_meta = m["resolved"].as_array().expect("resolved");

    let mut store = AgentStore::new();
    let mut applied = Vec::new();
    for (i, want) in resolved_meta.iter().enumerate() {
        // Find the last frame with this seq and kind, which is the resolved payload.
        let seq = want["seq"].as_u64().expect("seq");
        let kind = want["kind"].as_str().expect("kind");
        let file = frames
            .iter()
            .rfind(|f| f["seq"].as_u64() == Some(seq) && f["kind"].as_str() == Some(kind))
            .unwrap_or_else(|| panic!("resolved entry {i} ({kind} seq {seq}) has no frame"))
            ["file"]
            .as_str()
            .expect("file");

        let ev: AgentEvent = serde_json::from_str(&read(&dir.join(file))).expect("decode");
        store.apply(&ev);
        applied.push(ev);
    }

    let session_id = applied
        .first()
        .expect("at least one event")
        .session_id
        .clone();
    let session = store.get(&session_id).expect("session exists");

    assert!(
        !session.has_gaps(),
        "the resolved view has sequence gaps, so every pane fed by this emitter would be \
         marked as missing events"
    );

    let turn = session.latest_turn().expect("a turn");
    assert!(turn.complete, "message.stop never completed the turn");
    assert!(
        !turn.has_running_tool(),
        "a tool is still running after the turn finished; the session would never come \
         to rest"
    );
    assert_eq!(
        session.state(),
        AgentState::Done,
        "a finished turn must leave the session Done"
    );

    // One card per tool, each carrying what the call knew and what the result added.
    let tools: Vec<_> = turn.tools().collect();
    assert_eq!(tools.len(), 3, "expected one card per tool call");

    let edit = tools.iter().find(|t| t.name == "edit").expect("edit card");
    assert_eq!(edit.status, ToolStatus::Ok);
    assert_eq!(edit.duration_ms, Some(1240), "duration survived the edit");
    assert_eq!(
        edit.mime.as_deref(),
        Some("text/x-diff"),
        "a diff must be announced as one or heddle renders it as flat text"
    );
    assert!(
        edit.args.is_some(),
        "the arguments the call carried were lost when the result replaced it"
    );

    let bash = tools.iter().find(|t| t.name == "bash").expect("bash card");
    assert_eq!(bash.status, ToolStatus::Error);
    assert_eq!(bash.mime.as_deref(), Some("text/plain"));

    let fetch = tools
        .iter()
        .find(|t| t.name == "webfetch")
        .expect("webfetch card");
    assert_eq!(fetch.mime.as_deref(), Some("application/json"));

    // Streaming text resolved to the whole message, not a fragment and not a duplicate.
    assert_eq!(turn.text, "The emitter is live and lossless.");
    assert!(turn.commentary.contains("contiguous"));

    let usage = turn.usage.as_ref().expect("usage");
    assert_eq!(usage.input_tokens, 18422);
    assert_eq!(usage.output_tokens, 970);
    // Cost rides as an integer number of millionths: Matrix canonical JSON has no
    // floats, and Synapse rejects an event carrying one.
    assert_eq!(usage.cost_micro_usd, Some(41_200));
    assert_eq!(usage.cost(), Some(0.0412));
}

/// An unanswered approval must block the session.
///
/// This is the behaviour heddle's approval UI is built on and the one that had never run
/// against a real event: `state()` returns `Blocked` while anything is pending, which is
/// what raises the badge and what tells the user they are the bottleneck. Applying the
/// request without its resolution is the state a user actually sits in.
#[test]
fn an_unanswered_approval_blocks_the_session() {
    let dir = fixture_dir();
    let m = manifest();
    let frames = m["frames"].as_array().expect("frames");

    let mut store = AgentStore::new();
    let mut session_id = String::new();
    let mut asked = false;

    for frame in frames {
        let kind = frame["kind"].as_str().expect("kind");
        // Stop at the request: everything up to it is the turn as the user sees it when
        // the agent stops and waits.
        let file = frame["file"].as_str().expect("file");
        let ev: AgentEvent = serde_json::from_str(&read(&dir.join(file))).expect("decode");
        session_id.clone_from(&ev.session_id);
        store.apply(&ev);
        if kind == "approval.request" {
            asked = true;
            break;
        }
    }
    assert!(asked, "no approval.request among the fixtures");

    let session = store.get(&session_id).expect("session");
    assert!(
        !session.pending.is_empty(),
        "the request did not land as a pending prompt, so nothing would ask the user"
    );
    assert_eq!(
        session.state(),
        AgentState::Blocked,
        "an unanswered approval must outrank a running tool: the human is the bottleneck"
    );
}

/// Answering it must unblock the session again.
#[test]
fn answering_the_approval_releases_the_session() {
    let dir = fixture_dir();
    let m = manifest();

    let mut store = AgentStore::new();
    let mut session_id = String::new();
    for frame in m["frames"].as_array().expect("frames") {
        let file = frame["file"].as_str().expect("file");
        let ev: AgentEvent = serde_json::from_str(&read(&dir.join(file))).expect("decode");
        session_id.clone_from(&ev.session_id);
        store.apply(&ev);
    }

    let session = store.get(&session_id).expect("session");
    assert!(
        session.pending.is_empty(),
        "the resolution did not clear the prompt, so the pane would ask for ever"
    );
    assert_ne!(
        session.state(),
        AgentState::Blocked,
        "an answered approval must stop blocking"
    );
}
