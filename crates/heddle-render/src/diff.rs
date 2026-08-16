//! Diff rendering for `tool.result` bodies with `mime: text/x-diff`.
//!
//! This is the payload that makes a Matrix agent session feel like a local one: without
//! it a file edit is just the word "edit".
//!
//! Unified diffs only. There was a `render_pair` that diffed a before/after pair here
//! too, advertised in this paragraph and kept alive by a single test -- but nothing on
//! the wire produces a pair, and `ResultKind` has no variant for one. It went, and
//! `similar` went with it.

use crate::theme::Theme;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// Maximum lines shown before a diff is folded. Large refactors should not push the
/// conversation off the screen.
pub const FOLD_THRESHOLD: usize = 24;

/// Render a unified diff that the agent already produced.
///
/// Returns owned lines: transcripts are rebuilt from a worker-owned snapshot on every
/// timeline update, so a rendered line must not borrow from it.
pub fn render_unified(diff: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut in_hunk = false;
    diff.lines()
        .map(|line| {
            let styled = style_line(line, theme, in_hunk);
            in_hunk |= line.starts_with("@@");
            styled
        })
        .collect()
}

/// Whether a `---`/`+++` line is a file header rather than deleted or added content.
///
/// Position, not spelling. `--- a/x.sql` and `--- an old comment` are identical in
/// shape -- one is a header, the other is a deleted SQL comment -- and no amount of
/// looking at the line itself will tell them apart. What does is that headers appear in
/// the preamble, before the first `@@` hunk, and content only appears after one.
///
/// Matching on the bare prefix meant a diff of a SQL, Lua or Haskell file silently
/// under-reported its own `+12 -3` summary and painted the missing lines as chrome.
fn is_file_header(line: &str, in_hunk: bool) -> bool {
    !in_hunk && (line.starts_with("---") || line.starts_with("+++"))
}

/// Style one line of a unified diff.
fn style_line(line: &str, theme: &Theme, in_hunk: bool) -> Line<'static> {
    // Order matters: a file header also starts with `-` or `+`, so it must be tested
    // before the single-character add/remove prefixes.
    let style = if is_file_header(line, in_hunk) {
        theme.accent_style()
    } else if line.starts_with("@@") {
        theme.dim_style()
    } else if line.starts_with('+') {
        Style::default().fg(theme.added)
    } else if line.starts_with('-') {
        Style::default().fg(theme.removed)
    } else {
        Style::default().fg(theme.text)
    };
    Line::from(Span::styled(line.to_owned(), style))
}

/// Count added and removed lines, for a `+12 -3` summary in a collapsed card header.
pub fn stats(diff: &str) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    let mut in_hunk = false;
    for line in diff.lines() {
        if is_file_header(line, in_hunk) {
            continue;
        }
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

/// Fold a long diff to its first and last few lines with an elision marker.
pub fn fold(lines: Vec<Line<'static>>, theme: &Theme) -> Vec<Line<'static>> {
    if lines.len() <= FOLD_THRESHOLD {
        return lines;
    }
    let head = FOLD_THRESHOLD / 2;
    let tail = FOLD_THRESHOLD - head - 1;
    let hidden = lines.len() - head - tail;

    let mut out: Vec<Line<'static>> = lines.iter().take(head).cloned().collect();
    out.push(Line::from(Span::styled(
        format!("  … {hidden} more lines …"),
        theme.dim_style(),
    )));
    out.extend(lines.iter().skip(lines.len() - tail).cloned());
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    const SAMPLE: &str = "--- a/src/main.rs\n\
                          +++ b/src/main.rs\n\
                          @@ -1,3 +1,3 @@\n\
                          -fn check() {}\n\
                          +fn verify() {}\n\
                           fn main() {}";

    #[test]
    fn file_headers_are_not_counted_as_changes() {
        // `---` and `+++` would otherwise inflate every diff by one each.
        let (added, removed) = stats(SAMPLE);
        assert_eq!((added, removed), (1, 1));
    }

    #[test]
    fn content_that_merely_looks_like_a_header_is_counted() {
        // Deleting a SQL or Lua comment produces `--- comment`; adding one in C++ can
        // produce `+++foo`. Testing the bare prefix classified both as file headers, so
        // they were dropped from the summary and painted as chrome -- a diff of a
        // migration under-reported its own size.
        let diff = "--- a/x.sql\n+++ b/x.sql\n@@ -1 +1 @@\n--- an old comment\n+++new value\n";
        assert_eq!(stats(diff), (1, 1));

        let theme = Theme::default();
        let lines = render_unified(diff, &theme);
        assert_eq!(lines[3].spans[0].style.fg, Some(theme.removed), "content");
        assert_eq!(lines[4].spans[0].style.fg, Some(theme.added), "content");
    }

    #[test]
    fn headers_are_styled_as_headers_not_as_additions() {
        let theme = Theme::default();
        let lines = render_unified(SAMPLE, &theme);
        assert_eq!(lines[0].spans[0].style.fg, Some(theme.accent));
        assert_eq!(lines[1].spans[0].style.fg, Some(theme.accent));
        assert_eq!(lines[2].spans[0].style.fg, Some(theme.dim));
        assert_eq!(lines[3].spans[0].style.fg, Some(theme.removed));
        assert_eq!(lines[4].spans[0].style.fg, Some(theme.added));
    }

    #[test]
    fn short_diffs_are_not_folded() {
        let theme = Theme::default();
        let lines = render_unified(SAMPLE, &theme);
        let n = lines.len();
        assert_eq!(fold(lines, &theme).len(), n);
    }

    #[test]
    fn long_diffs_fold_to_a_bounded_height() {
        let theme = Theme::default();
        let big: String = (0..200).map(|i| format!("+line {i}\n")).collect();
        let folded = fold(render_unified(&big, &theme), &theme);
        assert_eq!(folded.len(), FOLD_THRESHOLD);
        let marker = folded[FOLD_THRESHOLD / 2].spans[0].content.to_string();
        assert!(marker.contains("more lines"), "got {marker:?}");
    }

    #[test]
    fn an_empty_diff_renders_nothing() {
        let theme = Theme::default();
        assert!(render_unified("", &theme).is_empty());
        assert_eq!(stats(""), (0, 0));
    }
}
