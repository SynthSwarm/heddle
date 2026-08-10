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
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Environment variable naming the capture file.
pub const CAPTURE_ENV: &str = "HEDDLE_CAPTURE";

/// One recorded message.
#[derive(Debug, Serialize)]
pub struct Record<'a> {
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

/// Append one record, if capture is switched on.
///
/// Failures are logged and dropped. A diagnostic that could interrupt a conversation
/// would be worse than the missing diagnostic.
pub fn record(record: &Record<'_>) {
    let Some(path) = path() else { return };

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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn a_record_serialises_to_something_a_fixture_can_be_built_from() {
        let content = json!({ "msgtype": "m.notice", "body": "🔧 edit: \"src/main.rs\"" });
        let record = Record {
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
    }

    #[test]
    fn capture_is_off_unless_asked_for() {
        // The default has to be off: this writes decrypted traffic to disk.
        if std::env::var(CAPTURE_ENV).is_err() {
            assert!(!enabled());
        }
    }
}
