//! The `--check` doctor.
//!
//! Three things can be wrong before heddle has drawn a single frame: the homeserver
//! cannot do what the architecture requires, the terminal cannot do what the renderer
//! assumes, or the local store is unreadable. Each has a different fix and none of them
//! is diagnosable from the symptom, which is usually "it starts and then behaves oddly".
//!
//! Checks report rather than decide. Only [`Status::Fail`] means heddle will not work;
//! a warning is something to know about when a glyph lands in the wrong column.

use std::fmt;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Not `yes`/`no`: most findings report a fact rather than answer a question,
        // and "TERM: yes" reads as nonsense.
        f.write_str(match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "FAIL",
        })
    }
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub name: String,
    pub status: Status,
    /// Shown beneath the line when there is something to explain.
    pub detail: Option<String>,
}

impl Finding {
    pub fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Ok,
            detail: Some(detail.into()),
        }
    }

    pub fn warn(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Warn,
            detail: Some(detail.into()),
        }
    }

    pub fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Fail,
            detail: Some(detail.into()),
        }
    }
}

/// What heddle assumes about the terminal.
///
/// The cursor-position round trip is safe here and nowhere else: during the TUI,
/// crossterm's `EventStream` owns stdin and swallows the reply, which is what made
/// `Terminal::clear` hang for two seconds and then kill the app. `--check` has no event
/// stream, so it can ask.
pub fn terminal() -> Vec<Finding> {
    use crossterm::tty::IsTty;

    let mut findings = Vec::new();

    let is_tty = std::io::stdout().is_tty();
    findings.push(if is_tty {
        Finding::ok("terminal", "stdout is a terminal")
    } else {
        Finding::warn(
            "terminal",
            "stdout is not a terminal; only the homeserver checks are meaningful",
        )
    });

    let term = std::env::var("TERM").unwrap_or_else(|_| "unset".into());
    let colour = std::env::var("COLORTERM").unwrap_or_default();
    findings.push(Finding::ok("TERM", term));
    findings.push(
        if colour.contains("truecolor") || colour.contains("24bit") {
            Finding::ok("colour", "24-bit")
        } else {
            Finding::warn(
                "colour",
                "COLORTERM does not advertise truecolor; themes fall back to the 256-colour cube",
            )
        },
    );

    match crossterm::terminal::size() {
        Ok((w, h)) if w >= 60 && h >= 16 => findings.push(Finding::ok("size", format!("{w}x{h}"))),
        Ok((w, h)) => findings.push(Finding::warn(
            "size",
            format!("{w}x{h}; panes need roughly 60x16 before splitting is useful"),
        )),
        Err(e) => findings.push(Finding::warn("size", format!("cannot be read: {e}"))),
    }

    if is_tty {
        findings.push(glyph_widths());
    }

    findings
}

/// Ask the terminal how wide it actually paints the glyphs heddle prints.
///
/// This is the one measurement worth making by experiment rather than by table.
/// `unicode-width` reports an intention; the terminal reports a fact, and where they
/// disagree every column to the right of the glyph is wrong. Emoji with East Asian
/// Width `Neutral` are the usual culprits and are banned from the transcript for that
/// reason, but a terminal is free to surprise us about the rest.
fn glyph_widths() -> Finding {
    let mut disagreements = Vec::new();

    for heddle_render::Glyph { glyph, what, cells } in heddle_render::PRINTED {
        let expected = *cells;
        match measure(glyph) {
            Ok(actual) if actual == expected => {}
            Ok(actual) => disagreements.push(format!(
                "{glyph} ({what}): heddle lays out {expected}, this terminal paints {actual}"
            )),
            Err(e) => {
                return Finding::warn("glyph widths", format!("could not be measured: {e}"));
            }
        }
    }

    if disagreements.is_empty() {
        Finding::ok(
            "glyph widths",
            "the terminal paints every glyph heddle prints at the assumed width",
        )
    } else {
        Finding::warn("glyph widths", disagreements.join("; "))
    }
}

/// Print `glyph` and report how many columns the cursor moved.
fn measure(glyph: &str) -> std::io::Result<usize> {
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    use std::io::Write;

    enable_raw_mode()?;
    // Restore raw mode however this returns: leaving a terminal raw would force the
    // user to blind-type `reset`, which is a poor reward for running a diagnostic.
    let result = (|| {
        let mut out = std::io::stdout();
        let (before, _) = crossterm::cursor::position()?;
        write!(out, "{glyph}")?;
        out.flush()?;
        let (after, _) = crossterm::cursor::position()?;
        // Erase the probe so the report is not littered with stray glyphs.
        write!(
            out,
            "\r{}\r",
            " ".repeat((after.saturating_sub(before)) as usize)
        )?;
        out.flush()?;
        Ok(usize::from(after.saturating_sub(before)))
    })();
    disable_raw_mode()?;
    result
}

/// What heddle assumes about the local store.
pub fn store(profile: &str, store_dir: &Path, session_file: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();

    if !session_file.exists() {
        findings.push(Finding::fail(
            "session",
            format!(
                "no session for profile `{profile}` at {}; run `heddle --profile {profile} login`",
                session_file.display()
            ),
        ));
        return findings;
    }

    match std::fs::read_to_string(session_file) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value) => {
                let device = value
                    .pointer("/device_id")
                    .or_else(|| value.pointer("/tokens/device_id"))
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned);
                findings.push(match device {
                    // The device ID is what every other device's verification of this
                    // one is pinned to; losing it silently means re-verifying.
                    Some(id) => Finding::ok("session", format!("device {id}")),
                    None => Finding::warn("session", "parses, but carries no device ID"),
                });
            }
            Err(e) => findings.push(Finding::fail("session", format!("will not parse: {e}"))),
        },
        Err(e) => findings.push(Finding::fail("session", format!("cannot be read: {e}"))),
    }

    findings.push(permissions("session file", session_file, 0o600));
    findings.push(permissions("store directory", store_dir, 0o700));

    // The SDK keeps state and crypto in SQLite. An empty directory beside a valid
    // session means the store was deleted underneath a live login, which presents as
    // every room being undecryptable rather than as anything obviously wrong.
    match std::fs::read_dir(store_dir) {
        Ok(entries) => {
            let count = entries.filter_map(Result::ok).count();
            findings.push(if count > 0 {
                Finding::ok("store", format!("{count} files in {}", store_dir.display()))
            } else {
                Finding::warn(
                    "store",
                    "the store directory is empty; encrypted history will not open until \
                     the keys are recovered",
                )
            });
        }
        Err(e) => findings.push(Finding::fail("store", format!("cannot be read: {e}"))),
    }

    findings
}

/// Check that a path is no more permissive than `most`.
#[cfg(unix)]
fn permissions(what: &str, path: &Path, most: u32) -> Finding {
    use std::os::unix::fs::PermissionsExt;

    match std::fs::metadata(path) {
        Ok(meta) => {
            let mode = meta.permissions().mode() & 0o777;
            if mode & !most == 0 {
                Finding::ok(what, format!("{mode:04o}"))
            } else {
                // The store holds the access token and every Megolm key this device has
                // ever seen. Group- or world-readable is a real leak, not a nit.
                Finding::warn(
                    what,
                    format!(
                        "{mode:04o} is more permissive than {most:04o}: chmod {most:o} {}",
                        path.display()
                    ),
                )
            }
        }
        Err(e) => Finding::warn(what, format!("cannot be read: {e}")),
    }
}

#[cfg(not(unix))]
fn permissions(what: &str, _path: &Path, _most: u32) -> Finding {
    Finding::ok(what, "not checked on this platform")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "heddle-doctor-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    fn worst(findings: &[Finding]) -> Status {
        findings
            .iter()
            .map(|f| f.status)
            .max_by_key(|s| match s {
                Status::Ok => 0,
                Status::Warn => 1,
                Status::Fail => 2,
            })
            .unwrap_or(Status::Ok)
    }

    #[test]
    fn a_missing_session_is_a_failure_that_says_what_to_run() {
        let dir = scratch("missing");
        let findings = store("work", &dir.join("store"), &dir.join("nope.json"));
        assert_eq!(worst(&findings), Status::Fail);
        let detail = findings[0].detail.clone().expect("detail");
        assert!(detail.contains("heddle --profile work login"), "{detail}");
    }

    #[test]
    fn a_corrupt_session_is_reported_as_such() {
        let dir = scratch("corrupt");
        let session = dir.join("session.json");
        std::fs::write(&session, "{ not json").expect("seed");
        std::fs::create_dir_all(dir.join("store")).expect("store");

        let findings = store("work", &dir.join("store"), &session);
        assert_eq!(findings[0].status, Status::Fail);
        assert!(findings[0]
            .detail
            .as_ref()
            .expect("detail")
            .contains("will not parse"));
    }

    #[test]
    fn a_healthy_store_reports_the_device_it_is_pinned_to() {
        let dir = scratch("healthy");
        let session = dir.join("session.json");
        std::fs::write(&session, r#"{"device_id":"WYQEISWIKB"}"#).expect("seed");
        let store_dir = dir.join("store");
        std::fs::create_dir_all(&store_dir).expect("store");
        std::fs::write(store_dir.join("matrix-sdk-state.sqlite3"), "x").expect("db");

        let findings = store("work", &store_dir, &session);
        assert!(findings[0]
            .detail
            .as_ref()
            .expect("detail")
            .contains("WYQEISWIKB"));
        assert!(
            findings
                .iter()
                .any(|f| f.name == "store"
                    && f.detail.as_ref().is_some_and(|d| d.contains("1 files")))
        );
    }

    #[test]
    fn an_empty_store_beside_a_valid_session_is_warned_about() {
        // Deleting the store under a live login presents as every room being
        // undecryptable, which looks like a server problem and is not one.
        let dir = scratch("empty");
        let session = dir.join("session.json");
        std::fs::write(&session, r#"{"device_id":"AAA"}"#).expect("seed");
        let store_dir = dir.join("store");
        std::fs::create_dir_all(&store_dir).expect("store");

        let findings = store("work", &store_dir, &session);
        let store_finding = findings.iter().find(|f| f.name == "store").expect("store");
        assert_eq!(store_finding.status, Status::Warn);
    }

    #[cfg(unix)]
    #[test]
    fn a_world_readable_session_is_warned_about_with_the_fix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("perms");
        let session = dir.join("session.json");
        std::fs::write(&session, r#"{"device_id":"AAA"}"#).expect("seed");
        std::fs::set_permissions(&session, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        std::fs::create_dir_all(dir.join("store")).expect("store");

        let findings = store("work", &dir.join("store"), &session);
        let perms = findings
            .iter()
            .find(|f| f.name == "session file")
            .expect("permissions finding");
        assert_eq!(perms.status, Status::Warn);
        assert!(perms.detail.as_ref().expect("detail").contains("chmod 600"));
    }

    #[test]
    fn the_doctor_probes_every_glyph_heddle_prints() {
        // The point of the probe is to catch the terminal that disagrees with
        // `unicode-width`. It can only do that for glyphs it is given, so it is given
        // the table rather than a copy of part of it.
        //
        // The copy it used to hold had five entries. One of them was `❓`, which the
        // transcript explicitly refuses to draw, and it omitted every one-cell glyph --
        // so the probe reported an all-clear on a set that was mostly not the set.
        assert!(heddle_render::PRINTED.len() > 5);
        for heddle_render::Glyph { glyph, what, cells } in heddle_render::PRINTED {
            assert_eq!(
                UnicodeWidthStr::width(*glyph),
                *cells,
                "{glyph} ({what}) is probed at a width it is not laid out at"
            );
        }
    }
}
