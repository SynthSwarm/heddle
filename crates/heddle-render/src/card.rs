//! Collapsible tool cards.
//!
//! The single largest difference between reading an agent in Element and reading it in
//! heddle. Element shows `🔧 edit: "src/main.rs..."`; a card shows the invocation, the
//! result, the diff and the duration, and folds itself away once it is history.
//!
//! See `docs/SPEC.md` §5.1.

use crate::diff;
use crate::theme::{disclosure, duration, tool_glyph, Theme};
use heddle_agent::{ResultKind, Tool, ToolStatus};
use ratatui::text::{Line, Span};

/// Maximum plain-text result lines before folding.
const PLAIN_FOLD: usize = 20;

/// When a card's body is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoExpand {
    /// Never expand automatically; the user drives it.
    Never,
    /// Expand while running, collapse when finished. The default: you watch work
    /// happen, then it gets out of the way.
    #[default]
    Running,
    /// Always expand.
    Always,
}

/// Decide whether a card should be open.
///
/// A user override always wins. Failures always expand regardless of policy, because a
/// silently collapsed error is the worst possible outcome.
pub fn is_expanded(tool: &Tool, policy: AutoExpand, user_override: Option<bool>) -> bool {
    if let Some(explicit) = user_override {
        return explicit;
    }
    if tool.status == ToolStatus::Error {
        return true;
    }
    match policy {
        AutoExpand::Never => false,
        AutoExpand::Always => true,
        AutoExpand::Running => tool.status == ToolStatus::Running,
    }
}

/// Render the one-line header of a card.
pub fn header(tool: &Tool, expanded: bool, theme: &Theme) -> Line<'static> {
    let style = theme.tool(tool.status);
    let mut spans = vec![
        Span::styled(disclosure(expanded).to_owned(), theme.dim_style()),
        Span::raw(" "),
        Span::styled(tool_glyph(tool.status).to_owned(), style),
        Span::raw(" "),
        Span::styled(tool.name.clone(), style),
    ];

    let summary = tool.summary();
    if !summary.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(summary, theme.dim_style()));
    }

    // A diff's shape is the most useful thing to know without opening the card.
    if tool.result_kind() == ResultKind::Diff {
        if let Some(body) = &tool.body {
            let (added, removed) = diff::stats(body);
            if added > 0 || removed > 0 {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!("+{added}"),
                    ratatui::style::Style::default().fg(theme.added),
                ));
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    format!("-{removed}"),
                    ratatui::style::Style::default().fg(theme.removed),
                ));
            }
        }
    }

    if let Some(ms) = tool.duration_ms {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(duration(ms), theme.dim_style()));
    }

    if tool.truncated {
        spans.push(Span::raw(" "));
        spans.push(Span::styled("(truncated)".to_owned(), theme.dim_style()));
    }

    Line::from(spans)
}

/// Render the body of an expanded card, indented under its header.
pub fn body(tool: &Tool, theme: &Theme) -> Vec<Line<'static>> {
    let Some(text) = tool.body.as_deref().filter(|b| !b.is_empty()) else {
        return Vec::new();
    };

    let lines = match tool.result_kind() {
        ResultKind::Diff => diff::fold(diff::render_unified(text, theme), theme),
        ResultKind::Json => render_json(text, theme),
        ResultKind::Markdown | ResultKind::Plain => render_plain(text, theme),
    };

    lines.into_iter().map(indent).collect()
}

/// Render a whole card.
pub fn render(tool: &Tool, expanded: bool, theme: &Theme) -> Vec<Line<'static>> {
    let mut out = vec![header(tool, expanded, theme)];
    if expanded {
        out.extend(body(tool, theme));
    }
    out
}

fn render_plain(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    let lines: Vec<Line<'static>> = text
        .lines()
        .map(|l| Line::from(Span::styled(l.to_owned(), theme.dim_style())))
        .collect();

    if lines.len() <= PLAIN_FOLD {
        return lines;
    }
    let hidden = lines.len() - PLAIN_FOLD;
    let mut out: Vec<Line<'static>> = lines.into_iter().take(PLAIN_FOLD).collect();
    out.push(Line::from(Span::styled(
        format!("… {hidden} more lines …"),
        theme.dim_style(),
    )));
    out
}

/// Pretty-print JSON, falling back to plain text when it does not parse.
///
/// A tool result arrives as whatever the agent serialised, which for a `Json` body is
/// usually one long line. Re-indenting it is the difference between a card and a wall.
/// Still folded by `render_plain`, so a large document does not take the pane.
fn render_json(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) => match serde_json::to_string_pretty(&value) {
            Ok(pretty) => render_plain(&pretty, theme),
            Err(_) => render_plain(text, theme),
        },
        // Not JSON after all. The mime is a claim by the agent, not a guarantee, and a
        // card that renders nothing because the claim was wrong is worse than one that
        // renders the bytes.
        Err(_) => render_plain(text, theme),
    }
}

/// Indent a line so card bodies sit visually under their header.
fn indent(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw("  │ ")];
    spans.extend(line.spans);
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    fn tool(status: ToolStatus) -> Tool {
        Tool {
            name: "edit".into(),
            index: 0,
            args: None,
            preview: Some("src/main.rs".into()),
            status,
            duration_ms: Some(1_400),
            mime: Some("text/x-diff".into()),
            body: Some("--- a/x\n+++ b/x\n+new\n-old".into()),
            truncated: false,
        }
    }

    #[test]
    fn a_json_body_is_reindented() {
        // `render_json` used to be a one-line forwarder to `render_plain` under a doc
        // comment describing a function nobody had written. A tool result arrives as
        // one long line, and the card is where it becomes readable.
        let theme = Theme::default();
        let tool = Tool {
            name: "read".into(),
            index: 0,
            args: None,
            preview: None,
            status: ToolStatus::Ok,
            duration_ms: None,
            mime: Some("application/json".into()),
            body: Some(r#"{"a":1,"b":[2,3]}"#.into()),
            truncated: false,
        };

        let lines = body(&tool, &theme);
        assert!(lines.len() > 1, "one line is not pretty-printed");
    }

    #[test]
    fn a_json_body_that_is_not_json_is_still_shown() {
        // The mime is a claim by the agent, not a guarantee.
        let theme = Theme::default();
        let tool = Tool {
            name: "read".into(),
            index: 0,
            args: None,
            preview: None,
            status: ToolStatus::Ok,
            duration_ms: None,
            mime: Some("application/json".into()),
            body: Some("not json at all".into()),
            truncated: false,
        };

        let text: String = body(&tool, &theme)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("not json at all"), "{text}");
    }

    #[test]
    fn running_tools_expand_by_default() {
        assert!(is_expanded(
            &tool(ToolStatus::Running),
            AutoExpand::Running,
            None
        ));
    }

    #[test]
    fn finished_tools_collapse_by_default() {
        assert!(!is_expanded(
            &tool(ToolStatus::Ok),
            AutoExpand::Running,
            None
        ));
    }

    #[test]
    fn failures_expand_regardless_of_policy() {
        // A silently collapsed error is the worst possible outcome.
        for policy in [AutoExpand::Never, AutoExpand::Running, AutoExpand::Always] {
            assert!(
                is_expanded(&tool(ToolStatus::Error), policy, None),
                "policy {policy:?} hid a failure"
            );
        }
    }

    #[test]
    fn a_user_override_beats_everything() {
        assert!(!is_expanded(
            &tool(ToolStatus::Error),
            AutoExpand::Always,
            Some(false)
        ));
        assert!(is_expanded(
            &tool(ToolStatus::Ok),
            AutoExpand::Never,
            Some(true)
        ));
    }

    #[test]
    fn header_summarises_a_diff_without_opening_it() {
        let theme = Theme::default();
        let line = header(&tool(ToolStatus::Ok), false, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("edit"), "{text}");
        assert!(text.contains("src/main.rs"), "{text}");
        assert!(text.contains("+1"), "{text}");
        assert!(text.contains("-1"), "{text}");
        assert!(text.contains("1.4s"), "{text}");
        assert!(text.starts_with('▸'), "collapsed marker missing: {text}");
    }

    #[test]
    fn collapsed_cards_are_exactly_one_line() {
        let theme = Theme::default();
        assert_eq!(render(&tool(ToolStatus::Ok), false, &theme).len(), 1);
    }

    #[test]
    fn expanded_cards_include_the_diff() {
        let theme = Theme::default();
        let lines = render(&tool(ToolStatus::Ok), true, &theme);
        assert!(lines.len() > 1);
        let body: String = lines[1..]
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(body.contains("new"), "{body}");
    }

    #[test]
    fn a_card_with_no_body_stays_one_line_even_when_expanded() {
        let theme = Theme::default();
        let mut t = tool(ToolStatus::Ok);
        t.body = None;
        assert_eq!(render(&t, true, &theme).len(), 1);
    }

    #[test]
    fn long_plain_output_is_folded() {
        let theme = Theme::default();
        let mut t = tool(ToolStatus::Ok);
        t.mime = Some("text/plain".into());
        t.body = Some((0..100).map(|i| format!("line {i}\n")).collect());
        let lines = body(&t, &theme);
        // The literal, not `PLAIN_FOLD + 1`. Written against the constant, this test
        // still passed with `PLAIN_FOLD = 0` -- it restated the implementation instead
        // of pinning the behaviour SPEC §5.2 documents.
        assert_eq!(lines.len(), 21, "20 lines and an elision marker");
    }

    #[test]
    fn truncation_is_advertised() {
        let theme = Theme::default();
        let mut t = tool(ToolStatus::Ok);
        t.truncated = true;
        let text: String = header(&t, false, &theme)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("truncated"), "{text}");
    }
}
