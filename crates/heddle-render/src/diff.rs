//! Diff rendering for `tool.result` bodies with `mime: text/x-diff`.
//!
//! This is the payload that makes a Matrix agent session feel like a local one: without
//! it a file edit is just the word "edit". Handles both a unified diff supplied by the
//! agent and a before/after pair that heddle diffs itself.

use crate::theme::Theme;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use similar::{ChangeTag, TextDiff};

/// Maximum lines shown before a diff is folded. Large refactors should not push the
/// conversation off the screen.
pub const FOLD_THRESHOLD: usize = 24;

/// Render a unified diff that the agent already produced.
///
/// Returns owned lines: transcripts are rebuilt from a worker-owned snapshot on every
/// timeline update, so a rendered line must not borrow from it.
pub fn render_unified(diff: &str, theme: &Theme) -> Vec<Line<'static>> {
    diff.lines().map(|line| style_line(line, theme)).collect()
}

/// Diff two texts and render the result.
pub fn render_pair(before: &str, after: &str, theme: &Theme) -> Vec<Line<'static>> {
    let diff = TextDiff::from_lines(before, after);
    diff.iter_all_changes()
        .map(|change| {
            let (sign, style) = match change.tag() {
                ChangeTag::Delete => ("-", Style::default().fg(theme.removed)),
                ChangeTag::Insert => ("+", Style::default().fg(theme.added)),
                ChangeTag::Equal => (" ", theme.dim_style()),
            };
            Line::from(vec![
                Span::styled(sign, style),
                Span::styled(change.value().trim_end_matches('\n').to_owned(), style),
            ])
        })
        .collect()
}

/// Style one line of a unified diff.
fn style_line(line: &str, theme: &Theme) -> Line<'static> {
    // Order matters: `+++` and `---` are file headers, not content, and must be tested
    // before the single-character add/remove prefixes.
    let style = if line.starts_with("+++") || line.starts_with("---") {
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
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
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
    fn diffing_a_pair_marks_both_sides() {
        let theme = Theme::default();
        let lines = render_pair("a\nb\n", "a\nc\n", &theme);
        let colours: Vec<_> = lines.iter().map(|l| l.spans[0].style.fg).collect();
        assert!(colours.contains(&Some(theme.removed)));
        assert!(colours.contains(&Some(theme.added)));
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
