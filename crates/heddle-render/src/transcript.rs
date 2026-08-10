//! Turning a timeline into lines.
//!
//! Converts [`heddle_matrix::Entry`] values into styled [`Line`]s, routing agent
//! messages through [`crate::card`] and ordinary messages through markdown.

use crate::card::{self, AutoExpand};
use crate::theme::Theme;
use heddle_agent::{AgentEvent, Kind};
use heddle_matrix::{AgentPayload, Entry, EntryKind, Message, Shield};
use ratatui::text::{Line, Span};
use std::collections::HashMap;

/// Per-card expansion overrides, keyed by `event_id`, then tool index.
pub type Overrides = HashMap<(String, u32), bool>;

/// Options affecting how a transcript is drawn.
#[derive(Debug, Clone)]
pub struct Options {
    pub auto_expand: AutoExpand,
    /// Show reasoning/commentary blocks.
    pub show_commentary: bool,
    /// Show a `~` marker on entries recovered by the fallback parser.
    pub mark_degraded: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            auto_expand: AutoExpand::default(),
            show_commentary: true,
            mark_degraded: true,
        }
    }
}

/// Where an event begins in the rendered output.
///
/// Selection and scroll-to-selection need to map an event id to a row, and only the
/// renderer knows how many rows anything occupied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub event_id: String,
    /// Row of the event's first line, before wrapping.
    pub row: u16,
}

/// A rendered transcript, plus the index needed to navigate it.
#[derive(Debug, Clone, Default)]
pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    pub anchors: Vec<Anchor>,
}

/// Marker drawn in the left gutter of the selected event.
const SELECTION_MARK: &str = "\u{258e}";

/// Render a whole transcript.
///
/// Every line carries a one-column gutter, marked on the selected event. A gutter on
/// every line rather than an indent on one keeps the text aligned regardless of what is
/// selected, so selecting does not reflow the transcript.
pub fn render(
    entries: &[Entry],
    theme: &Theme,
    options: &Options,
    overrides: &Overrides,
    selected: Option<&str>,
) -> Rendered {
    let mut out = Rendered::default();
    for entry in entries {
        let is_selected = entry
            .event_id
            .as_deref()
            .is_some_and(|id| Some(id) == selected);
        let body = render_entry(entry, theme, options, overrides);
        if body.is_empty() {
            continue;
        }

        if let Some(event_id) = &entry.event_id {
            out.anchors.push(Anchor {
                event_id: event_id.clone(),
                row: out.lines.len() as u16,
            });
        }

        let gutter = if is_selected {
            Span::styled(SELECTION_MARK.to_owned(), theme.accent_style())
        } else {
            Span::raw(" ")
        };
        for line in body {
            let mut spans = Vec::with_capacity(line.spans.len() + 1);
            spans.push(gutter.clone());
            spans.extend(line.spans);
            out.lines.push(Line::from(spans));
        }
    }
    out
}

fn render_entry(
    entry: &Entry,
    theme: &Theme,
    options: &Options,
    overrides: &Overrides,
) -> Vec<Line<'static>> {
    match &entry.kind {
        EntryKind::DateDivider(ts) => vec![Line::from(Span::styled(
            format!("──── {} ────", format_date(*ts)),
            theme.dim_style(),
        ))],
        EntryKind::ReadMarker => vec![Line::from(Span::styled(
            "─── new ───".to_owned(),
            theme.accent_style(),
        ))],
        EntryKind::TimelineStart => vec![Line::from(Span::styled(
            "─── beginning of history ───".to_owned(),
            theme.dim_style(),
        ))],
        EntryKind::UnableToDecrypt => vec![Line::from(Span::styled(
            "🔒 unable to decrypt — waiting for keys".to_owned(),
            theme.dim_style(),
        ))],
        EntryKind::Notice(text) if text.is_empty() => Vec::new(),
        EntryKind::Notice(text) => vec![Line::from(Span::styled(text.clone(), theme.dim_style()))],
        EntryKind::Message(message) => render_message(
            entry.event_id.as_deref(),
            message,
            theme,
            options,
            overrides,
        ),
    }
}

fn render_message(
    event_id: Option<&str>,
    message: &Message,
    theme: &Theme,
    options: &Options,
    overrides: &Overrides,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();

    match &message.agent {
        AgentPayload::Structured { event, .. } => {
            out.extend(render_agent_event(
                event, event_id, theme, options, overrides,
            ));
        }
        AgentPayload::Degraded { parsed, .. } => {
            for (i, tool) in parsed.tools.iter().enumerate() {
                let expanded = card::is_expanded(
                    tool,
                    options.auto_expand,
                    override_for(overrides, event_id, i as u32),
                );
                out.extend(card::render(tool, expanded, theme));
            }
            if !parsed.prose.is_empty() {
                out.push(sender_line(message, theme, options.mark_degraded));
                out.extend(markdown(&parsed.prose));
            }
            // The parser lifts fenced blocks out of the prose. Putting them back is not
            // optional: an agent reply is mostly code, so dropping the blocks dropped
            // most of the message.
            for block in &parsed.blocks {
                out.extend(code_block(block));
            }
        }
        AgentPayload::None => {
            out.push(sender_line(message, theme, false));
            out.extend(markdown(&message.body));
        }
    }

    // Never render a message as nothing. Any path that yields no lines — an event kind
    // we do not draw, a parse that swallowed the text — would drop the message from the
    // transcript silently, which reads as the agent having said nothing at all.
    if out.is_empty() && !message.body.trim().is_empty() {
        out.push(sender_line(message, theme, false));
        out.extend(markdown(&message.body));
    }

    if !message.reactions.is_empty() {
        out.push(reaction_line(message, theme));
    }

    // A thread root is the entry point to an agent session, and the room timeline hides
    // the replies, so without this the thread is invisible from here.
    if let Some(replies) = message.thread_replies {
        out.push(thread_line(replies, theme));
    }

    out
}

/// The "this message has a thread" affordance.
fn thread_line(replies: u32, theme: &Theme) -> Line<'static> {
    let label = match replies {
        0 => "thread".to_owned(),
        1 => "1 reply".to_owned(),
        n => format!("{n} replies"),
    };
    Line::from(vec![
        Span::styled("  ⤷ ".to_owned(), theme.accent_style()),
        Span::styled(label, theme.accent_style()),
        Span::styled("  enter to open".to_owned(), theme.dim_style()),
    ])
}

fn render_agent_event(
    event: &AgentEvent,
    event_id: Option<&str>,
    theme: &Theme,
    options: &Options,
    overrides: &Overrides,
) -> Vec<Line<'static>> {
    match event.kind {
        Kind::ToolCall | Kind::ToolResult => {
            let Some(tool) = &event.tool else {
                return Vec::new();
            };
            let expanded = card::is_expanded(
                tool,
                options.auto_expand,
                override_for(overrides, event_id, tool.index),
            );
            card::render(tool, expanded, theme)
        }
        Kind::MessageDelta => event.text.as_deref().map(markdown).unwrap_or_default(),
        Kind::Commentary if options.show_commentary => event
            .text
            .as_deref()
            .map(|t| {
                t.lines()
                    .map(|l| {
                        Line::from(vec![
                            Span::styled("┊ ".to_owned(), theme.dim_style()),
                            Span::styled(l.to_owned(), theme.dim_style()),
                        ])
                    })
                    .collect()
            })
            .unwrap_or_default(),
        Kind::Notice => event
            .notice
            .as_ref()
            .filter(|n| !n.text.is_empty())
            .map(|n| vec![Line::from(Span::styled(n.text.clone(), theme.dim_style()))])
            .unwrap_or_default(),
        Kind::Usage => event
            .usage
            .map(|u| {
                vec![Line::from(Span::styled(
                    format!("  {} in / {} out", u.input_tokens, u.output_tokens),
                    theme.dim_style(),
                ))]
            })
            .unwrap_or_default(),
        // Approvals and pickers are drawn as interactive prompts by the app, not as
        // transcript rows, so that they can carry a live countdown and keybindings.
        Kind::ApprovalRequest
        | Kind::ApprovalResolved
        | Kind::ModelPicker
        | Kind::MessageStop
        | Kind::Commentary
        | Kind::Unknown => Vec::new(),
    }
}

fn override_for(overrides: &Overrides, event_id: Option<&str>, index: u32) -> Option<bool> {
    let id = event_id?;
    overrides.get(&(id.to_owned(), index)).copied()
}

fn sender_line(message: &Message, theme: &Theme, mark_degraded: bool) -> Line<'static> {
    let style = if message.is_own {
        ratatui::style::Style::default().fg(theme.own)
    } else {
        theme.accent_style()
    };

    // Who is speaking, at a glance. In a client where half the participants are agents,
    // the distinction is worth more than a name alone: it is the difference between a
    // colleague and a process, and the eye finds a glyph before it reads a word.
    //
    // Every emoji here is East_Asian_Width=Wide. That is not decoration policy, it is a
    // correctness requirement: `unicode-width` reports Neutral emoji as one cell while
    // terminals paint two, and a sender line that measures short leaves stale cells the
    // renderer believes are already correct. The obvious glyphs for this job -- ⚠ and
    // 🛡 -- are both Neutral, and both would rot the transcript.
    let who = if message.agent.is_agent() {
        "🤖"
    } else {
        "👤"
    };

    let mut spans = vec![
        Span::styled(format!("{who} "), theme.dim_style()),
        Span::styled(message.sender_display.clone(), style),
    ];
    if mark_degraded {
        spans.push(Span::styled(" ~".to_owned(), theme.dim_style()));
    }
    if message.is_edited {
        spans.push(Span::styled(" (edited)".to_owned(), theme.dim_style()));
    }
    // Beside the name, because a shield is a statement about who sent this, and the name
    // is the claim it qualifies. The reason stays spelled out: a glyph alone gets read as
    // decoration, and the user is owed the specific meaning of "unverified".
    //
    // Cautions are deliberately not drawn. In non-strict mode the only one the SDK ever
    // produces is `AuthenticityNotGuaranteed`, which means the Megolm key came from an
    // "insecure source" -- and a key backup is one. Every message a recovered device can
    // read therefore carries it, for ever, which made the transcript a wall of identical
    // warnings saying nothing about any particular message. The SDK's own comment calls
    // this case "quite common and mostly noise". A warning on every line is a warning on
    // none, and the red one has to survive being noticed.
    match message.shield {
        Shield::None | Shield::Caution(_) => {}
        Shield::Warning(reason) => spans.push(Span::styled(
            format!("  ⛔ {}", reason.describe()),
            theme.error_style(),
        )),
    }
    Line::from(spans)
}

fn reaction_line(message: &Message, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    for (key, count) in &message.reactions {
        spans.push(Span::styled(format!("{key} {count}  "), theme.dim_style()));
    }
    Line::from(spans)
}

/// Render a recovered code block, re-fenced so it is highlighted like any other.
fn code_block(block: &heddle_agent::fallback::CodeBlock) -> Vec<Line<'static>> {
    let lang = block.lang.clone().unwrap_or_default();
    markdown(&format!("```{lang}\n{}\n```", block.body))
}

/// Render markdown to styled lines.
fn markdown(source: &str) -> Vec<Line<'static>> {
    // `tui_markdown` borrows from its input, so the result is deep-copied into owned
    // lines. Transcripts are re-rendered on every timeline snapshot and must not hold a
    // borrow on worker-owned data.
    let text = tui_markdown::from_str(source);
    text.lines
        .into_iter()
        .map(|line| {
            Line::from(
                line.spans
                    .into_iter()
                    .map(|s| Span::styled(s.content.into_owned(), s.style))
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

/// Format a millisecond timestamp as a date divider label.
fn format_date(ms: u64) -> String {
    // Deliberately dependency-free: a civil-date conversion from the Unix epoch. Adding
    // a full date-time crate for one label is not worth the compile time.
    let days = (ms / 86_400_000) as i64;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use heddle_agent::{Tool, ToolStatus};

    fn text_of(rendered: &Rendered) -> String {
        rendered
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn message(agent: AgentPayload) -> Entry {
        Entry {
            id: "i1".into(),
            event_id: Some("$e1".into()),
            kind: EntryKind::Message(Message {
                shield: Shield::None,
                sender: "@hermes:x".into(),
                sender_display: "hermes".into(),
                body: "hello".into(),
                timestamp: 0,
                is_own: false,
                is_edited: false,
                thread_root: None,
                thread_replies: None,
                reactions: Vec::new(),
                agent,
            }),
        }
    }

    #[test]
    fn plain_messages_show_a_sender_and_body() {
        let out = render(
            &[message(AgentPayload::None)],
            &Theme::default(),
            &Options::default(),
            &Overrides::new(),
            None,
        );
        let text = text_of(&out);
        assert!(text.contains("hermes"), "{text}");
        assert!(text.contains("hello"), "{text}");
    }

    #[test]
    fn tool_calls_render_as_cards_not_as_chat() {
        let event = AgentEvent {
            v: 1,
            session_id: "s".into(),
            turn_id: "t".into(),
            seq: 1,
            kind: Kind::ToolCall,
            agent: None,
            text: None,
            final_: None,
            tool: Some(Tool {
                name: "bash".into(),
                index: 0,
                args: None,
                preview: Some("cargo test".into()),
                status: ToolStatus::Ok,
                duration_ms: Some(1_400),
                mime: None,
                body: None,
                truncated: false,
            }),
            notice: None,
            approval: None,
            picker: None,
            usage: None,
        };
        let out = render(
            &[message(AgentPayload::Structured {
                adapter: "test",
                event: Box::new(event),
            })],
            &Theme::default(),
            &Options::default(),
            &Overrides::new(),
            None,
        );
        let text = text_of(&out);
        assert!(text.contains("bash"), "{text}");
        assert!(text.contains("cargo test"), "{text}");
        // The raw body must not leak through alongside the card.
        assert!(!text.contains("hello"), "{text}");
    }

    #[test]
    fn a_user_override_reopens_a_finished_card() {
        let theme = Theme::default();
        let tool = Tool {
            name: "edit".into(),
            index: 0,
            args: None,
            preview: None,
            status: ToolStatus::Ok,
            duration_ms: None,
            mime: Some("text/plain".into()),
            body: Some("the body".into()),
            truncated: false,
        };
        let event = AgentEvent {
            v: 1,
            session_id: "s".into(),
            turn_id: "t".into(),
            seq: 1,
            kind: Kind::ToolResult,
            agent: None,
            text: None,
            final_: None,
            tool: Some(tool),
            notice: None,
            approval: None,
            picker: None,
            usage: None,
        };
        let entry = message(AgentPayload::Structured {
            adapter: "test",
            event: Box::new(event),
        });

        let collapsed = render(
            std::slice::from_ref(&entry),
            &theme,
            &Options::default(),
            &Overrides::new(),
            None,
        );
        assert!(!text_of(&collapsed).contains("the body"));

        let mut overrides = Overrides::new();
        overrides.insert(("$e1".into(), 0), true);
        let expanded = render(&[entry], &theme, &Options::default(), &overrides, None);
        assert!(text_of(&expanded).contains("the body"));
    }

    #[test]
    fn degraded_entries_are_marked() {
        let mut parsed = heddle_agent::fallback::Parsed {
            prose: "done".into(),
            ..Default::default()
        };
        parsed.tools.push(Tool {
            name: "edit".into(),
            index: 0,
            args: None,
            preview: Some("x.rs".into()),
            status: ToolStatus::Ok,
            duration_ms: None,
            mime: None,
            body: None,
            truncated: false,
        });
        let out = render(
            &[message(AgentPayload::Degraded {
                adapter: "test",
                parsed: Box::new(parsed),
            })],
            &Theme::default(),
            &Options::default(),
            &Overrides::new(),
            None,
        );
        let text = text_of(&out);
        assert!(text.contains('~'), "degradation must be visible: {text}");
        assert!(text.contains("edit"), "{text}");
    }

    #[test]
    fn commentary_can_be_hidden() {
        let event = AgentEvent {
            v: 1,
            session_id: "s".into(),
            turn_id: "t".into(),
            seq: 1,
            kind: Kind::Commentary,
            agent: None,
            text: Some("thinking about it".into()),
            final_: None,
            tool: None,
            notice: None,
            approval: None,
            picker: None,
            usage: None,
        };
        let entry = message(AgentPayload::Structured {
            adapter: "test",
            event: Box::new(event),
        });
        let theme = Theme::default();

        let shown = render(
            std::slice::from_ref(&entry),
            &theme,
            &Options::default(),
            &Overrides::new(),
            None,
        );
        assert!(text_of(&shown).contains("thinking about it"));

        let hidden = render(
            &[entry],
            &theme,
            &Options {
                show_commentary: false,
                ..Options::default()
            },
            &Overrides::new(),
            None,
        );
        assert!(!text_of(&hidden).contains("thinking about it"));
    }

    #[test]
    fn a_key_from_backup_does_not_shield_every_message_it_unlocks() {
        // Recovering from backup marks every restored key as an insecure source, so this
        // caution lands on all of history at once. Drawn, it buries the warning that
        // means something.
        let mut entry = message(AgentPayload::None);
        if let EntryKind::Message(m) = &mut entry.kind {
            m.shield = Shield::Caution(heddle_matrix::ShieldReason::Unknown);
        }
        let out = render(
            std::slice::from_ref(&entry),
            &Theme::default(),
            &Options::default(),
            &Overrides::new(),
            None,
        );

        let text = text_of(&out);
        assert!(!text.contains("❓"));
        assert!(!text.contains("authenticity unknown"));
    }

    #[test]
    fn every_glyph_the_transcript_prints_is_measured_as_it_is_painted() {
        use unicode_width::UnicodeWidthStr;

        // `unicode-width` reports East_Asian_Width=Neutral emoji as one cell while every
        // terminal heddle targets paints them as two. A glyph that measures short leaves
        // cells the renderer believes are already correct, so they are never repainted
        // and the transcript rots as it scrolls.
        //
        // The tempting glyphs for a security warning -- ⚠ and 🛡 -- are both Neutral.
        // This test exists because the next person to reach for one will not know that.
        for glyph in ["👤", "🤖", "⛔", "🔒"] {
            assert_eq!(
                UnicodeWidthStr::width(glyph),
                2,
                "{glyph} is not measured as two cells; it will desynchronise the renderer"
            );
        }
    }

    #[test]
    fn a_shield_names_its_reason_rather_than_relying_on_the_glyph() {
        let mut entry = message(AgentPayload::None);
        if let EntryKind::Message(m) = &mut entry.kind {
            m.shield = Shield::Warning(heddle_matrix::ShieldReason::UnsignedDevice);
        }
        let out = render(
            std::slice::from_ref(&entry),
            &Theme::default(),
            &Options::default(),
            &Overrides::new(),
            None,
        );
        let text = text_of(&out);

        assert!(text.contains("⛔"));
        assert!(
            text.contains("unverified device"),
            "a glyph on its own gets read as decoration"
        );
    }

    #[test]
    fn a_human_and_an_agent_are_told_apart_at_a_glance() {
        let human = message(AgentPayload::None);
        let out = render(
            std::slice::from_ref(&human),
            &Theme::default(),
            &Options::default(),
            &Overrides::new(),
            None,
        );
        assert!(text_of(&out).contains("👤"));
    }

    #[test]
    fn undecryptable_events_say_so_rather_than_vanishing() {
        let entry = Entry {
            id: "i".into(),
            event_id: None,
            kind: EntryKind::UnableToDecrypt,
        };
        let out = render(
            &[entry],
            &Theme::default(),
            &Options::default(),
            &Overrides::new(),
            None,
        );
        assert!(text_of(&out).contains("unable to decrypt"));
    }

    #[test]
    fn date_dividers_convert_correctly() {
        // Cross-checked against Python's datetime.
        assert_eq!(format_date(1_785_628_800_000), "2026-08-02");
        assert_eq!(format_date(1_785_369_600_000), "2026-07-30");
        // The Unix epoch itself.
        assert_eq!(format_date(0), "1970-01-01");
        // A leap day, which is where naive civil-date maths usually breaks.
        assert_eq!(format_date(951_782_400_000), "2000-02-29");
    }

    #[test]
    fn events_are_anchored_to_their_first_row() {
        let entries = vec![message(AgentPayload::None), {
            let mut e = message(AgentPayload::None);
            e.event_id = Some("$e2".into());
            e
        }];
        let out = render(
            &entries,
            &Theme::default(),
            &Options::default(),
            &Overrides::new(),
            None,
        );
        assert_eq!(out.anchors.len(), 2);
        assert_eq!(out.anchors[0].row, 0);
        assert!(
            out.anchors[1].row > 0,
            "the second event must start below the first"
        );
        assert_eq!(out.anchors[1].event_id, "$e2");
    }

    #[test]
    fn selection_marks_the_gutter_without_reflowing_text() {
        let entry = message(AgentPayload::None);
        let theme = Theme::default();

        let plain = render(
            std::slice::from_ref(&entry),
            &theme,
            &Options::default(),
            &Overrides::new(),
            None,
        );
        let picked = render(
            &[entry],
            &theme,
            &Options::default(),
            &Overrides::new(),
            Some("$e1"),
        );

        assert_eq!(
            plain.lines.len(),
            picked.lines.len(),
            "selecting must not change the line count"
        );
        // Every line carries a gutter, so the body starts in the same column either way.
        for (a, b) in plain.lines.iter().zip(&picked.lines) {
            assert_eq!(a.spans.len(), b.spans.len());
        }
        assert!(text_of(&picked).contains(SELECTION_MARK));
        assert!(!text_of(&plain).contains(SELECTION_MARK));
    }

    #[test]
    fn code_blocks_survive_the_fallback_parser() {
        // The parser lifts fenced blocks out of the prose. They were then never drawn,
        // so an agent reply that was mostly code arrived almost empty.
        let mut parsed = heddle_agent::fallback::Parsed {
            prose: "here is the fix".into(),
            ..Default::default()
        };
        parsed.tools.push(Tool {
            name: "edit".into(),
            index: 0,
            args: None,
            preview: Some("x.rs".into()),
            status: ToolStatus::Ok,
            duration_ms: None,
            mime: None,
            body: None,
            truncated: false,
        });
        parsed.blocks.push(heddle_agent::fallback::CodeBlock {
            lang: Some("rust".into()),
            body: "fn verify() {}".into(),
        });

        let out = render(
            &[message(AgentPayload::Degraded {
                adapter: "test",
                parsed: Box::new(parsed),
            })],
            &Theme::default(),
            &Options::default(),
            &Overrides::new(),
            None,
        );
        let text = text_of(&out);
        assert!(text.contains("here is the fix"), "{text}");
        assert!(
            text.contains("fn verify()"),
            "the code must not vanish: {text}"
        );
    }

    #[test]
    fn a_message_is_never_rendered_as_nothing() {
        // An agent event kind we do not draw must not swallow the message with it.
        let event = AgentEvent {
            v: 1,
            session_id: "s".into(),
            turn_id: "t".into(),
            seq: 1,
            kind: Kind::MessageStop,
            agent: None,
            text: None,
            final_: None,
            tool: None,
            notice: None,
            approval: None,
            picker: None,
            usage: None,
        };
        let out = render(
            &[message(AgentPayload::Structured {
                adapter: "test",
                event: Box::new(event),
            })],
            &Theme::default(),
            &Options::default(),
            &Overrides::new(),
            None,
        );
        assert!(
            text_of(&out).contains("hello"),
            "the body must survive an undrawn event kind: {}",
            text_of(&out)
        );
    }

    #[test]
    fn entries_that_render_nothing_are_not_anchored() {
        // An empty notice produces no rows, so selecting it would move the caret to a
        // row that does not exist.
        let entry = Entry {
            id: "i".into(),
            event_id: Some("$empty".into()),
            kind: EntryKind::Notice(String::new()),
        };
        let out = render(
            &[entry],
            &Theme::default(),
            &Options::default(),
            &Overrides::new(),
            None,
        );
        assert!(out.anchors.is_empty());
    }

    #[test]
    fn a_human_posting_code_is_not_an_agent() {
        // Fenced code is not evidence of an agent. Treating it as evidence sent plain
        // messages down the lossy path and stamped them with the degraded marker.
        let parsed = heddle_agent::fallback::parse("look:\n```sh\nls -la\n```\n");
        assert!(
            parsed.is_empty(),
            "code alone must not count as agent chrome: {parsed:?}"
        );
    }
}
