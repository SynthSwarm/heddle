//! Capturing real agent output, so fixtures are recordings rather than guesses.
//!
//! Every test of the chrome parser is currently a string somebody imagined. That is a
//! poor way to verify a parser whose entire job is to match output it does not control,
//! and it is the largest untested surface in the client.
//!
//! Setting `HEDDLE_CAPTURE` to a path makes heddle append one JSON object per message
//! it considers for agent structure, recording what arrived and what heddle made of it.
//! The result can be replayed as a test fixture.
//!
//! **This writes decrypted message content to a plain file.** It is off unless the
//! variable is set, the file is created `0600`, and it should be deleted when the
//! recording is done. Nothing redacts it, because a capture that quietly dropped the
//! part the parser mishandled would defeat the purpose.

use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Environment variable naming the capture file.
pub const CAPTURE_ENV: &str = "HEDDLE_CAPTURE";

/// One recorded message.
#[derive(Debug, Serialize)]
pub struct Record<'a> {
    /// The event this came from, or `None` while it is still a local echo.
    ///
    /// Recorded for two reasons. It is the deduplication key -- see [`record`] -- and it
    /// is the only way to tie a line in the capture back to a line in the log. Without
    /// it a capture answers "what did the parser see" but never "what was that event",
    /// which is the question a diagnosis usually turns on.
    pub event_id: Option<&'a str>,
    pub sender: &'a str,
    /// The human-readable body, in full and unmodified.
    pub body: &'a str,
    /// What heddle made of it: `structured`, `degraded` or `plain`.
    pub verdict: &'a str,
    /// Which adapter claimed it, if any.
    pub adapter: Option<&'a str>,
    /// Tool names recovered, so a bad match is obvious without rerunning the parser.
    pub tools: Vec<String>,
    /// The whole `content` object, which is what a fixture needs.
    pub content: &'a Value,
}

fn path() -> Option<&'static PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let raw = std::env::var(CAPTURE_ENV).ok().filter(|p| !p.is_empty())?;
        let path = PathBuf::from(raw);
        // Announced loudly and once. Recording decrypted traffic to disk is not
        // something to discover afterwards from a stray file.
        tracing::warn!(
            path = %path.display(),
            "HEDDLE_CAPTURE is set: decrypted message bodies are being written to disk"
        );
        Some(path)
    })
    .as_ref()
}

/// Whether capture is switched on.
pub fn enabled() -> bool {
    path().is_some()
}

/// Append one record, if capture is switched on and this event is new.
///
/// Deduplicated by event id, because the worker converts a whole timeline on every
/// snapshot and an agent streams by editing one event repeatedly: a run that captured
/// every conversion recorded 10,180 lines for a few hundred messages, most of them the
/// same message at different lengths. Fixtures built from that are mostly duplicates of
/// each other, and the file is large enough to discourage reading.
///
/// Failures are logged and dropped. A diagnostic that could interrupt a conversation
/// would be worse than the missing diagnostic.
pub fn record(record: &Record<'_>) {
    let Some(path) = path() else { return };

    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    if !is_new(SEEN.get_or_init(Default::default), record.event_id) {
        return;
    }

    let Ok(mut line) = serde_json::to_string(record) else {
        return;
    };
    line.push('\n');

    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    match options.open(path) {
        Ok(mut file) => {
            if let Err(error) = file.write_all(line.as_bytes()) {
                tracing::warn!(?error, "could not write to the capture file");
            }
        }
        Err(error) => {
            tracing::warn!(?error, path = %path.display(), "could not open the capture file")
        }
    }
}

/// Whether this event has not been recorded before.
///
/// A local echo has no id yet and is always new; it acquires one a moment later and the
/// remote echo is what gets deduplicated. A poisoned lock means another thread panicked
/// mid-insert, and the answer is yes: capture is a diagnostic, and a duplicate line is a
/// far better outcome than a panic propagating out of one.
fn is_new(seen: &Mutex<HashSet<String>>, event_id: Option<&str>) -> bool {
    let Some(event_id) = event_id else {
        return true;
    };
    match seen.lock() {
        Ok(mut seen) => seen.insert(event_id.to_owned()),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn a_record_serialises_to_something_a_fixture_can_be_built_from() {
        let content = json!({ "msgtype": "m.notice", "body": "🔧 edit: \"src/main.rs\"" });
        let record = Record {
            event_id: Some("$abc"),
            sender: "@hermes:example.org",
            body: "🔧 edit: \"src/main.rs\"",
            verdict: "degraded",
            adapter: Some("hermes"),
            tools: vec!["edit".into()],
            content: &content,
        };

        let line = serde_json::to_string(&record).expect("serialises");
        let back: Value = serde_json::from_str(&line).expect("round trips");
        assert_eq!(back["verdict"], "degraded");
        assert_eq!(back["adapter"], "hermes");
        assert_eq!(back["tools"][0], "edit");
        // The whole content object has to survive, or the recording cannot be replayed.
        assert_eq!(back["content"]["msgtype"], "m.notice");
        assert_eq!(back["body"], "🔧 edit: \"src/main.rs\"");
        // The key that ties a captured line back to a line in the log.
        assert_eq!(back["event_id"], "$abc");
    }

    #[test]
    fn an_event_is_recorded_once_however_often_it_is_converted() {
        // The worker converts the whole timeline on every snapshot, and an agent streams
        // by editing one event over and over. Without this, a few hundred messages
        // recorded ten thousand lines, nearly all of them the same message part-written.
        let seen = Mutex::new(HashSet::new());
        assert!(is_new(&seen, Some("$a")));
        assert!(!is_new(&seen, Some("$a")));
        assert!(is_new(&seen, Some("$b")));
    }

    #[test]
    fn a_local_echo_is_always_recorded() {
        // It has no id to deduplicate on yet. Dropping it would lose the one form of a
        // message that only exists before the server has seen it.
        let seen = Mutex::new(HashSet::new());
        assert!(is_new(&seen, None));
        assert!(is_new(&seen, None));
    }

    #[test]
    fn capture_is_off_unless_asked_for() {
        // The default has to be off: this writes decrypted traffic to disk.
        if std::env::var(CAPTURE_ENV).is_err() {
            assert!(!enabled());
        }
    }
}
