//! Terminal drawing.
//!
//! Pure layout and painting. All decisions live in [`crate::app`]; this module only
//! turns state into cells.

use crate::app::{App, Hit, Pending, RecoveryPanel};
use crate::composer::Composer;
use crate::keymap::{self, Mode};
use crate::palette::keys_for;
use heddle_agent::AgentState;
use heddle_matrix::{MemberSummary, SyncState, View};
use heddle_render::transcript::Anchor;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

/// Composer height, including its border, when it holds a single line.
const COMPOSER_MIN_HEIGHT: u16 = 3;

/// Most lines the composer will grow to before it scrolls internally. Beyond this the
/// transcript is being squeezed for a message nobody reads while typing.
const COMPOSER_MAX_LINES: u16 = 8;

/// Floor width for a tab label, so short room names still occupy a tab-sized slot.
const MIN_TAB_WIDTH: usize = 10;

/// Drawn between tabs and at both ends of the bar.
const TAB_SEPARATOR: &str = "│";

/// Blank columns between key hints in the status bar.
const HINT_GAP: usize = 3;

/// Columns kept clear on the right of a transcript.
///
/// Absorbs the overflow when a glyph paints wider than `unicode-width` measured it, so
/// the spill lands on a blank cell instead of the pane border.
const TRANSCRIPT_GUTTER: u16 = 1;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // workspace bar
            Constraint::Length(1), // tab bar
            Constraint::Min(3),    // panes
            Constraint::Length(composer_height(app, frame.area().width)),
            Constraint::Length(1), // status
        ])
        .split(frame.area());

    draw_workspace_bar(frame, app, chunks[0]);
    draw_tab_bar(frame, app, chunks[1]);
    draw_panes(frame, app, chunks[2]);
    let caret = draw_composer(frame, app, chunks[3]);
    draw_status(frame, app, chunks[4]);

    // Before the modal overlays: this one belongs to the composer, and a palette or a
    // verification opened over the top of it must cover it rather than sit under it.
    if app.mentions.is_some() {
        if let Some(caret) = caret {
            draw_mentions(frame, app, frame.area(), caret);
        }
    }

    if app.threads.is_some() {
        draw_threads(frame, app, frame.area());
    }

    if app.emoji.is_some() {
        draw_emoji(frame, app, frame.area());
    }

    if app.palette.is_some() {
        draw_palette(frame, app, frame.area());
    }

    // Last, so it sits above everything.
    if app.help {
        draw_help(frame, app, frame.area());
    }

    // Above even the help: a security prompt that something else can obscure is a
    // security prompt the user can be tricked into answering blind.
    if app.verification.is_some() {
        draw_verification(frame, app, frame.area());
    }

    if app.recovery_prompt.is_some() {
        draw_recovery(frame, app, frame.area());
    }
}

/// The interactive verification panel.
///
/// The emoji are drawn one per line with their names spelled out, rather than in a row.
/// Other clients use a grid, but heddle cannot: terminals disagree with `unicode-width`
/// about how many cells several of these glyphs occupy, and a row that wraps wrongly
/// puts the seventh emoji under the first. Comparing a misaligned grid against a phone
/// is exactly the moment a user gives up and presses yes. The name beside each symbol
/// also settles any ambiguity the font introduces.
fn draw_verification(frame: &mut Frame, app: &App, area: Rect) {
    use heddle_matrix::Verification;

    let Some(state) = &app.verification else {
        return;
    };

    let (title, mut lines, hint) = match state {
        Verification::Requested { other_device } => (
            " verification requested ",
            vec![
                Line::from(Span::styled(
                    format!("{other_device} wants to verify this device."),
                    Style::default(),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Accepting only starts the comparison; you confirm the keys next.".to_owned(),
                    app.theme.dim_style(),
                )),
            ],
            "y accept   n reject",
        ),
        Verification::Negotiating { other_device } => (
            " verifying ",
            vec![Line::from(Span::styled(
                format!("agreeing on a method with {other_device}…"),
                app.theme.dim_style(),
            ))],
            "esc cancel",
        ),
        Verification::Compare {
            other_device,
            emoji,
        } => {
            let mut lines = vec![
                Line::from(Span::styled(
                    format!("Do these match what {other_device} shows?"),
                    Style::default(),
                )),
                Line::from(""),
            ];
            for (symbol, description) in emoji {
                lines.push(Line::from(vec![
                    // The symbol carries the accent; the name is ordinary text beside
                    // it, so the eye lands on the glyph being compared.
                    Span::styled(format!(" {symbol} "), app.theme.accent_style()),
                    Span::styled("  ".to_owned(), Style::default()),
                    Span::styled(description.clone(), Style::default()),
                ]));
            }
            (
                " compare emoji ",
                lines,
                "y they match   n they do NOT match",
            )
        }
        Verification::WaitingForOther { other_device } => (
            " waiting ",
            vec![Line::from(Span::styled(
                format!("confirmed here; waiting for {other_device}…"),
                app.theme.dim_style(),
            ))],
            "esc cancel",
        ),
        // Both terminal states clear `app.verification`, so the panel is already gone.
        Verification::Done | Verification::Cancelled { .. } => return,
    };

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        hint.to_owned(),
        app.theme.dim_style(),
    )));

    let width = lines
        .iter()
        .map(ratatui::text::Line::width)
        .max()
        .unwrap_or(20)
        .saturating_add(4)
        .try_into()
        .unwrap_or(u16::MAX)
        .clamp(24, area.width);
    let height = (lines.len() as u16 + 2).min(area.height);

    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(app.theme.accent_style())
                .title(title),
        ),
        popup,
    );
}

/// The recovery panel.
///
/// The key being *asked for* is masked; the key being *given out* is not. They are
/// opposite situations: one is a secret the user already holds and is re-entering, where
/// echoing it only helps a shoulder, and the other is a secret they have never seen and
/// must copy down exactly, where hiding it would defeat the entire exercise.
fn draw_recovery(frame: &mut Frame, app: &App, area: Rect) {
    let Some(panel) = &app.recovery_prompt else {
        return;
    };

    let (title, lines, width) = match panel {
        RecoveryPanel::AskKey {
            key,
            submitted,
            error,
        } => {
            let mut lines = vec![
                Line::from(Span::styled(
                    "Enter your recovery key".to_owned(),
                    Style::default(),
                )),
                Line::from(""),
            ];
            if *submitted {
                lines.push(Line::from(Span::styled(
                    "unlocking…".to_owned(),
                    app.theme.dim_style(),
                )));
            } else {
                if let Some(error) = error {
                    lines.push(Line::from(Span::styled(
                        format!("that key did not work: {error}"),
                        app.theme.error_style(),
                    )));
                    lines.push(Line::from(""));
                }
                lines.push(Line::from(vec![
                    Span::styled(
                        "\u{2022}".repeat(key.chars().count()),
                        app.theme.accent_style(),
                    ),
                    Span::styled("\u{2588}".to_owned(), app.theme.dim_style()),
                ]));
                lines.push(Line::from(""));
                // The count is what makes a paste visibly land, given the masking.
                lines.push(Line::from(Span::styled(
                    format!("{} characters", key.chars().count()),
                    app.theme.dim_style(),
                )));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "enter unlock   esc cancel".to_owned(),
                    app.theme.dim_style(),
                )));
            }
            (" recovery ", lines, 44)
        }

        RecoveryPanel::OfferEnable => (
            " set up recovery ",
            vec![
                Line::from(Span::styled(
                    "This account has no recovery set up.".to_owned(),
                    Style::default(),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Without it, messages you can read today become unreadable if you \
                     lose every device you own."
                        .to_owned(),
                    app.theme.dim_style(),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "You will be shown a recovery key once. Keep it somewhere safe.".to_owned(),
                    app.theme.dim_style(),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "y set it up   n not now".to_owned(),
                    app.theme.dim_style(),
                )),
            ],
            60,
        ),

        RecoveryPanel::ConfirmReset => (
            " replace recovery key ",
            vec![
                Line::from(Span::styled(
                    "Recovery is already set up and unlocked here.".to_owned(),
                    Style::default(),
                )),
                Line::from(""),
                // Said plainly and in the warning colour, because the cost lands on
                // devices that are not in front of the user to be reassured.
                Line::from(Span::styled(
                    "Replacing the key makes your current one useless. Any device \
                     holding it will need the new one."
                        .to_owned(),
                    app.theme.error_style(),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "y replace it   n keep the one I have".to_owned(),
                    app.theme.dim_style(),
                )),
            ],
            60,
        ),

        RecoveryPanel::Busy(what) => (
            " recovery ",
            vec![Line::from(Span::styled(
                (*what).to_owned(),
                app.theme.dim_style(),
            ))],
            44,
        ),

        RecoveryPanel::ShowKey { key } => (
            " your recovery key ",
            vec![
                Line::from(Span::styled(
                    "Write this down now. It is not shown again and the server \
                     keeps no copy."
                        .to_owned(),
                    app.theme.error_style(),
                )),
                Line::from(""),
                Line::from(Span::styled(key.clone(), app.theme.accent_style())),
                Line::from(""),
                Line::from(Span::styled(
                    "enter I have saved it".to_owned(),
                    app.theme.dim_style(),
                )),
            ],
            72,
        ),
    };

    let width = (width as u16).min(area.width);
    // Wrapped, so a long sentence in a narrow terminal is not simply cut off mid-warning.
    let paragraph = Paragraph::new(lines)
        .wrap(ratatui::widgets::Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(app.theme.accent_style())
                .title(title),
        );
    let height = paragraph
        .line_count(width.saturating_sub(2))
        .try_into()
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(area.height);

    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);
    frame.render_widget(paragraph, popup);
}

/// The `<prefix> t` thread picker.
///
/// For a Hermes room this is the list of agent sessions, which is why it exists before
/// any of the other overlays.
fn draw_threads(frame: &mut Frame, app: &App, area: Rect) {
    let Some(picker) = &app.threads else {
        return;
    };

    let width = (area.width * 3 / 4).clamp(20, area.width);
    let height = ((picker.threads.len() + 2) as u16).clamp(4, area.height.min(20));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let lines: Vec<Line> = if picker.loading {
        vec![Line::from(Span::styled(
            " listing threads…".to_owned(),
            app.theme.dim_style(),
        ))]
    } else if picker.threads.is_empty() {
        vec![Line::from(Span::styled(
            " no threads in this room".to_owned(),
            app.theme.dim_style(),
        ))]
    } else {
        picker
            .threads
            .iter()
            .enumerate()
            .map(|(i, thread)| {
                let marker = if i == picker.selected {
                    "\u{258e}"
                } else {
                    " "
                };
                let style = if i == picker.selected {
                    app.theme.accent_style()
                } else {
                    app.theme.dim_style()
                };
                Line::from(vec![
                    Span::styled(marker.to_owned(), app.theme.accent_style()),
                    Span::styled(format!("{:<12}", thread.sender_display), style),
                    Span::styled(thread.preview.clone(), app.theme.dim_style()),
                ])
            })
            .collect()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.border_style(true))
        .title(Span::styled(" threads ", app.theme.accent_style()));

    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

/// The emoji picker, for `<prefix> e` and `<prefix> r`.
///
/// A list rather than a grid. Terminals disagree with `unicode-width` about how many
/// cells some emoji occupy, and in a grid that error compounds across every column; one
/// per row keeps the damage to the row that caused it. See the width note in PLAN.md.
fn draw_emoji(frame: &mut Frame, app: &App, area: Rect) {
    let Some(picker) = &app.emoji else {
        return;
    };

    let width = (area.width / 2).clamp(24, area.width);
    let rows = (picker.matches.len() as u16).clamp(1, 12);
    // Query line, list, borders.
    let height = (rows + 3).min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let title = match &picker.target {
        crate::emoji::Target::Composer => " emoji ",
        crate::emoji::Target::Reaction { .. } => " react ",
    };

    let mut lines = vec![Line::from(vec![
        Span::styled("search ".to_owned(), app.theme.dim_style()),
        Span::styled(picker.query.clone(), app.theme.accent_style()),
        // A block caret, so an empty query still shows where typing goes.
        Span::styled("\u{2588}".to_owned(), app.theme.dim_style()),
    ])];

    if picker.matches.is_empty() {
        lines.push(Line::from(Span::styled(
            " nothing matches".to_owned(),
            app.theme.dim_style(),
        )));
    } else {
        // Keep the highlight on screen once the selection walks past the visible rows.
        let first = picker
            .selected
            .saturating_sub(rows.saturating_sub(1) as usize);
        for (i, emoji) in picker
            .matches
            .iter()
            .enumerate()
            .skip(first)
            .take(rows as usize)
        {
            let (marker, style) = if i == picker.selected {
                ("\u{258e}", app.theme.accent_style())
            } else {
                (" ", app.theme.dim_style())
            };
            lines.push(Line::from(vec![
                Span::styled(marker.to_owned(), app.theme.accent_style()),
                Span::styled(format!("{}  ", emoji.as_str()), style),
                Span::styled(emoji.name().to_owned(), app.theme.dim_style()),
            ]));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.border_style(true))
        .title(Span::styled(title, app.theme.accent_style()));

    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

/// The command palette, for `:`.
///
/// Each row carries the key that does the same thing, so that the palette is a way of
/// learning the bindings rather than a permanent substitute for them. Keys are written
/// with the configured prefix rather than a hardcoded `^a`, since a custom `ui.prefix`
/// would otherwise be taught wrongly on every row.
fn draw_palette(frame: &mut Frame, app: &App, area: Rect) {
    let Some(palette) = &app.palette else {
        return;
    };

    let width = (area.width * 2 / 3).clamp(30, area.width);
    let rows = palette.matches.len().clamp(1, crate::palette::MAX_ROWS);
    // Query line, list, borders.
    let height = (rows as u16 + 3).min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let mut lines = vec![Line::from(vec![
        Span::styled(":".to_owned(), app.theme.accent_style()),
        Span::styled(palette.query.clone(), app.theme.accent_style()),
        // A block caret, so an empty query still shows where typing goes.
        Span::styled("\u{2588}".to_owned(), app.theme.dim_style()),
    ])];

    if palette.matches.is_empty() {
        lines.push(Line::from(Span::styled(
            " nothing matches".to_owned(),
            app.theme.dim_style(),
        )));
    } else {
        // Widest key column, so the names line up without a fixed guess that a custom
        // prefix would overflow.
        let key_width = palette
            .visible(rows)
            .map(|(command, _)| UnicodeWidthStr::width(keys_for(command, app.prefix).as_str()))
            .max()
            .unwrap_or(0);

        for (command, is_selected) in palette.visible(rows) {
            let (marker, style) = if is_selected {
                ("\u{258e}", app.theme.accent_style())
            } else {
                (" ", app.theme.dim_style())
            };
            lines.push(Line::from(vec![
                Span::styled(marker.to_owned(), app.theme.accent_style()),
                Span::styled(
                    format!("{:<key_width$}  ", keys_for(command, app.prefix)),
                    app.theme.dim_style(),
                ),
                Span::styled(command.name.to_owned(), style),
            ]));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.border_style(true))
        .title(Span::styled(" commands ", app.theme.accent_style()));

    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

/// Rows the mention popup will show at most. Enough to choose from, short enough to
/// leave the conversation visible behind it.
const MENTION_ROWS: usize = 8;

/// Place a popup against a point rather than in the middle of the screen.
///
/// The first anchored overlay in heddle: every other one is a modal dialogue, and the
/// middle of the screen is the right place for those. A completion list is not a
/// dialogue, it is an annotation on the word being typed, and it has to be next to it.
///
/// Sits above the anchor by preference, because the anchor is in the composer and the
/// composer is at the bottom; drops below only when there is genuinely no room, and
/// slides left rather than overflowing the right edge.
fn anchored(area: Rect, anchor: (u16, u16), width: u16, height: u16) -> Rect {
    let (ax, ay) = anchor;
    let width = width.min(area.width);
    let height = height.min(area.height);

    let x = ax.min(area.right().saturating_sub(width)).max(area.x);
    let y = if ay >= area.y + height {
        ay - height
    } else {
        // No room above: below the anchor, still inside the screen.
        (ay + 1).min(area.bottom().saturating_sub(height))
    };

    Rect {
        x,
        y,
        width,
        height,
    }
}

/// The mention picker, anchored to the word being typed.
fn draw_mentions(frame: &mut Frame, app: &App, area: Rect, caret: (u16, u16)) {
    let Some(picker) = &app.mentions else {
        return;
    };
    let matches = app.mention_matches();
    // An armed `@` that matches nobody draws nothing at all. A box saying "no matches"
    // under every word beginning with @ would be an overlay that punishes typing.
    if matches.is_empty() {
        return;
    }

    // Scrolled so the highlight stays visible once the selection walks past the rows,
    // the same way the palette and the emoji picker do it.
    let rows = matches.len().min(MENTION_ROWS);
    let first = picker.selected.saturating_sub(rows.saturating_sub(1));

    let visible: Vec<(usize, &&MemberSummary)> =
        matches.iter().enumerate().skip(first).take(rows).collect();

    let lines: Vec<Line> = visible
        .iter()
        .map(|(i, member)| {
            let (marker, style) = if *i == picker.selected {
                ("\u{258e}", app.theme.accent_style())
            } else {
                (" ", app.theme.dim_style())
            };
            let mut spans = vec![
                Span::styled(marker.to_owned(), app.theme.accent_style()),
                Span::styled(member.display_name.clone(), style),
            ];
            // The full ID only where the name alone would not say who this is. Showing
            // it always would bury the names it is there to disambiguate.
            if member.ambiguous {
                spans.push(Span::styled(
                    format!("  {}", member.user_id),
                    app.theme.dim_style(),
                ));
            }
            Line::from(spans)
        })
        .collect();

    let widest = visible
        .iter()
        .map(|(_, member)| {
            UnicodeWidthStr::width(member.display_name.as_str())
                + if member.ambiguous {
                    UnicodeWidthStr::width(member.user_id.as_str()) + 2
                } else {
                    0
                }
        })
        .max()
        .unwrap_or(0);

    // Marker, text, and the two borders.
    let width = (widest as u16).saturating_add(4).clamp(12, area.width);
    let height = (rows as u16).saturating_add(2);
    // One column left of the caret, so the list lines up under the word rather than
    // under the letter after it.
    let popup = anchored(area, (caret.0.saturating_sub(1), caret.1), width, height);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.border_style(true))
        .title(Span::styled(" mention ", app.theme.accent_style()));

    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

/// How tall the composer needs to be, borders included.
///
/// Measured after wrapping, not by counting newlines: a single long paragraph occupies
/// several rows and should be visible while it is written rather than scrolling out of
/// a one-line slot.
fn composer_height(app: &App, total_width: u16) -> u16 {
    let width = total_width.saturating_sub(2);
    let rows = app
        .composer()
        .map_or(1, |c| c.wrapped(width).lines.len() as u16);
    COMPOSER_MIN_HEIGHT + rows.clamp(1, COMPOSER_MAX_LINES) - 1
}

/// The `<prefix> ?` key overlay.
fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    let prefix = app.prefix.label();

    let rows: Vec<(String, &str)> = keymap::BINDINGS
        .iter()
        .map(|b| {
            let keys = if b.prefixed {
                format!("{prefix} {}", b.keys)
            } else {
                b.keys.to_owned()
            };
            (keys, b.action)
        })
        .collect();

    let key_width = rows
        .iter()
        .map(|(k, _)| UnicodeWidthStr::width(k.as_str()))
        .max()
        .unwrap_or(0);
    let action_width = rows
        .iter()
        .map(|(_, a)| UnicodeWidthStr::width(*a))
        .max()
        .unwrap_or(0);

    // Two spaces of padding each side, two between the columns, two for the border.
    let width = (key_width + action_width + 8).min(area.width as usize) as u16;
    let height = (rows.len() + 2).min(area.height as usize) as u16;
    if width < 8 || height < 4 {
        return;
    }

    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let lines: Vec<Line> = rows
        .iter()
        .map(|(keys, action)| {
            let pad = key_width.saturating_sub(UnicodeWidthStr::width(keys.as_str()));
            Line::from(vec![
                Span::raw(" ".repeat(pad + 1)),
                Span::styled(keys.clone(), app.theme.accent_style()),
                Span::raw("  "),
                Span::styled((*action).to_owned(), app.theme.dim_style()),
            ])
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.border_style(true))
        .title(Span::styled(" keys ", app.theme.accent_style()));

    // Blank the cells underneath: the overlay is opaque, not a tint.
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

/// Re-express anchors in the units the scroll offset counts.
///
/// The renderer numbers each anchor by its line in the unwrapped transcript, because it
/// does not know how wide the pane will be. `scroll` and `rendered_lines` count *wrapped*
/// lines. The two agree only while nothing wraps; every wrapped line above an anchor
/// pushes the real row further down than its recorded one, so scroll-to-selection
/// undershoots by the accumulated difference and, for a selection already inside the
/// stale window, decides no scrolling is needed at all.
///
/// Measuring one line at a time is what makes this exact under ratatui's word wrapping,
/// which no arithmetic on string widths reproduces. It costs a measurement per line of
/// the focused pane per frame; worth revisiting if transcripts get long enough to notice.
fn wrapped_anchors(lines: &[Line<'static>], anchors: &[Anchor], width: u16) -> Vec<Anchor> {
    if width == 0 || anchors.is_empty() {
        return anchors.to_vec();
    }

    let mut offsets = Vec::with_capacity(lines.len() + 1);
    let mut total: u16 = 0;
    for line in lines {
        offsets.push(total);
        let height = Paragraph::new(line.clone())
            .wrap(Wrap { trim: false })
            .line_count(width) as u16;
        total = total.saturating_add(height.max(1));
    }
    offsets.push(total);

    anchors
        .iter()
        .map(|anchor| Anchor {
            event_id: anchor.event_id.clone(),
            row: *offsets.get(anchor.row as usize).unwrap_or(&total),
        })
        .collect()
}

/// Build a strip of tab-like cells.
///
/// Both bars go through this so a workspace and a room are visually the same kind of
/// thing: a separated, padded, selectable cell. Without the padding and separators a
/// short label renders as a lone highlighted word, which reads as a heading.
///
/// Returns the spans, the clickable span of each cell, and the column just past the
/// trailing separator.
fn tab_strip(
    cells: Vec<(String, Style)>,
    separator: Style,
    origin_x: u16,
) -> (Vec<Span<'static>>, Vec<Hit>, u16) {
    let separator_width = UnicodeWidthStr::width(TAB_SEPARATOR) as u16;
    let mut spans = Vec::with_capacity(cells.len() * 2 + 1);
    let mut hits = Vec::with_capacity(cells.len());
    let mut x = origin_x;

    for (index, (mut label, style)) in cells.into_iter().enumerate() {
        let width = UnicodeWidthStr::width(label.as_str());
        if width < MIN_TAB_WIDTH {
            label.push_str(&" ".repeat(MIN_TAB_WIDTH - width));
        }

        spans.push(Span::styled(TAB_SEPARATOR, separator));
        x += separator_width;

        let cell = format!(" {label} ");
        let cell_width = UnicodeWidthStr::width(cell.as_str()) as u16;
        hits.push(Hit {
            x0: x,
            x1: x + cell_width,
            index,
        });
        x += cell_width;
        spans.push(Span::styled(cell, style));
    }

    spans.push(Span::styled(TAB_SEPARATOR, separator));
    x += separator_width;
    (spans, hits, x)
}

fn draw_workspace_bar(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused_id = app.workspaces.focused().map(|w| w.id.clone());

    let cells: Vec<(String, Style)> = app
        .workspaces
        .items
        .iter()
        .map(|workspace| {
            let is_focused = Some(&workspace.id) == focused_id.as_ref();
            let state = workspace.state();
            let unread = workspace.unread();

            // Agent state wins when there is one, because it is the thing this client
            // exists to surface. Unread only has to beat "dim", so that a workspace
            // holding traffic never looks the same as an empty one.
            let mut style = if state == AgentState::Idle && unread.any() {
                app.theme.unread(unread.is_highlight())
            } else {
                app.theme.state(state)
            };
            if is_focused {
                style = style.add_modifier(Modifier::REVERSED);
            }

            let mut label = workspace.title.clone();
            // The focused workspace's own counts are about to be read, so a badge on it
            // is noise; every other workspace is out of sight and needs one.
            if let Some(badge) = unread.label().filter(|_| !is_focused) {
                label.push(' ');
                label.push_str(&badge);
            }
            (label, style)
        })
        .collect();

    let spans = if cells.is_empty() {
        app.bars.workspaces = Vec::new();
        vec![Span::styled(
            " connecting… ".to_owned(),
            app.theme.dim_style(),
        )]
    } else {
        // No `+` here: creating a Space is out of scope (SPEC.md §7), so an affordance
        // suggesting otherwise would be a lie.
        let (spans, hits, _) = tab_strip(cells, app.theme.dim_style(), area.x);
        app.bars.workspaces = hits;
        spans
    };
    app.bars.workspace_row = area.y;

    let (state, count) = app.badge();
    let badge = if count > 0 && state.is_notable() {
        format!(" {} {count} ", state.glyph())
    } else {
        String::new()
    };

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    if !badge.is_empty() {
        let width = badge.chars().count() as u16;
        if area.width > width {
            let right = Rect {
                x: area.x + area.width - width,
                width,
                ..area
            };
            frame.render_widget(
                Paragraph::new(Span::styled(badge, app.theme.state(state))),
                right,
            );
        }
    }
}

fn draw_tab_bar(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(workspace) = app.workspaces.focused() else {
        app.bars.tabs = Vec::new();
        app.bars.new_tab = None;
        return;
    };
    let focused_room = workspace.focused_tab().map(|t| t.room_id.clone());

    let cells: Vec<(String, Style)> = workspace
        .tabs
        .iter()
        .map(|tab| {
            let is_focused = Some(&tab.room_id) == focused_room.as_ref();
            let state = tab.state();
            let mut style = if state == AgentState::Idle && tab.unread.any() {
                app.theme.unread(tab.unread.is_highlight())
            } else {
                app.theme.state(state)
            };
            if is_focused {
                style = style.add_modifier(Modifier::REVERSED);
            }

            let mut label = tab.title.clone();
            if tab.is_encrypted {
                label.push_str(" 🔒");
            }
            // A padlock says "encrypted", which is not the same as "trustworthy", and
            // a room can be both encrypted and carrying messages we cannot vouch for.
            if app.unverified_rooms.contains(&tab.room_id) {
                label.push_str(" ⛔");
            }
            // Both counts, not just mentions: a busy room you have not opened is worth
            // seeing even when nobody said your name.
            if let Some(badge) = tab.unread.label().filter(|_| !is_focused) {
                label.push(' ');
                label.push_str(&badge);
            }
            (label, style)
        })
        .collect();

    let (mut spans, hits, x) = tab_strip(cells, app.theme.dim_style(), area.x);

    // A `+` so the strip is self-describing as tabs with a "new" affordance.
    let plus = " + ";
    let plus_width = UnicodeWidthStr::width(plus) as u16;
    spans.push(Span::styled(plus, app.theme.dim_style()));

    app.bars.tab_row = area.y;
    app.bars.tabs = hits;
    app.bars.new_tab = Some((x, x + plus_width));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_panes(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(room_id) = app
        .workspaces
        .focused()
        .and_then(|w| w.focused_tab())
        .map(|t| t.room_id.clone())
    else {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "no rooms yet".to_owned(),
                app.theme.dim_style(),
            )),
            area,
        );
        return;
    };

    let placements = app.tilings.entry(room_id).or_default().layout(area);

    // Everything each pane needs, gathered before the frame is borrowed mutably.
    let panes: Vec<_> = placements
        .iter()
        .map(|placement| {
            let pane = app
                .workspaces
                .focused()
                .and_then(|w| w.focused_tab())
                .and_then(|t| t.panes.iter().find(|p| p.id == placement.id));
            let view = pane.map(|p| match p.kind.thread_root() {
                Some(root) => View::thread(p.kind.room_id(), root),
                None => View::room(p.kind.room_id()),
            });
            (
                *placement,
                pane.map(|p| p.header()).unwrap_or_default(),
                pane.map(|p| p.state).unwrap_or_default(),
                view,
            )
        })
        .collect();

    let selected = app.selected_event().map(ToOwned::to_owned);

    // Render every pane, not just the focused one. A pane that goes blank the moment it
    // loses focus destroys the reason for having panes at all.
    let drawn: Vec<_> = panes
        .iter()
        .map(|(placement, header, state, view)| {
            let inner = inner_of(placement.rect);
            let entries = view
                .as_ref()
                .and_then(|v| app.timelines.get(v))
                .map(Vec::as_slice)
                .unwrap_or(&[]);

            let rendered = heddle_render::transcript::render(
                entries,
                &app.theme,
                &app.render_options,
                &app.overrides,
                // The selection belongs to the focused view; marking it in a background
                // pane would claim a message is selected there too.
                if placement.is_focused {
                    selected.as_deref()
                } else {
                    None
                },
            );

            let scroll = view
                .as_ref()
                .and_then(|v| app.scroll.get(v).copied())
                .unwrap_or(0);

            (*placement, header.clone(), *state, inner, rendered, scroll)
        })
        .collect();

    // Hand back the focused pane's geometry: scroll clamping, pagination and
    // scroll-to-selection all measure against it.
    if let Some((_, _, _, inner, rendered, _)) =
        drawn.iter().find(|(placement, ..)| placement.is_focused)
    {
        let width = inner.width.saturating_sub(TRANSCRIPT_GUTTER);
        let paragraph = Paragraph::new(rendered.lines.clone()).wrap(Wrap { trim: false });
        app.rendered_lines = paragraph.line_count(width) as u16;
        app.viewport_height = inner.height;
        app.anchors = wrapped_anchors(&rendered.lines, &rendered.anchors, width);
    }

    for (placement, header, state, inner, rendered, scroll) in drawn {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(app.theme.border_style(placement.is_focused))
            .title(Span::styled(header, app.theme.state(state)));

        // Keep one column clear on the right. `unicode-width` and the terminal disagree
        // about emoji whose East Asian Width is Neutral but which render as two cells —
        // U+1F54A DOVE and friends. ratatui lays them out as one column, the terminal
        // paints two, and the overflow lands on whatever is to the right. Without the
        // gutter that is the border, which is why the edge went dashed wherever such a
        // glyph happened to end a line.
        let text_area = Rect {
            width: inner.width.saturating_sub(TRANSCRIPT_GUTTER),
            ..inner
        };

        let lines = if placement.is_focused {
            rendered.lines
        } else {
            // Dim rather than blank. The text is still context worth reading; it just is
            // not where the keyboard is pointing.
            dimmed(rendered.lines)
        };

        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let total = paragraph.line_count(text_area.width) as u16;
        let max_scroll = total.saturating_sub(text_area.height);
        let offset = max_scroll.saturating_sub(scroll.min(max_scroll));
        frame.render_widget(paragraph.scroll((offset, 0)), text_area);

        // Border last, deliberately. Belt and braces alongside the gutter: whatever the
        // transcript contains, the chrome is painted over it rather than under it.
        frame.render_widget(block, placement.rect);
    }
}

/// The area inside a pane's border.
fn inner_of(rect: Rect) -> Rect {
    Rect {
        x: rect.x.saturating_add(1),
        y: rect.y.saturating_add(1),
        width: rect.width.saturating_sub(2),
        height: rect.height.saturating_sub(2),
    }
}

/// Dim a rendered transcript without flattening its colours.
///
/// `Modifier::DIM` is a patch, so markdown highlighting and sender colours survive; the
/// whole pane just recedes.
fn dimmed(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|line| {
            Line::from(
                line.spans
                    .into_iter()
                    .map(|span| {
                        let style = span.style.add_modifier(Modifier::DIM);
                        Span::styled(span.content, style)
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

/// Draw the composer, and report where the caret landed in absolute screen columns.
///
/// The caret position is returned rather than recomputed by the caller because only the
/// wrap knows which display row a byte offset fell on, and the mention popup has to
/// point at the word being typed.
fn draw_composer(frame: &mut Frame, app: &App, area: Rect) -> Option<(u16, u16)> {
    // What the composer is about to do outranks which mode it is in: sending an edit
    // when you meant to send a message is not recoverable.
    let (label, style) = match (&app.composing, app.mode) {
        (Some(Pending::Reply(_)), _) => ("reply", app.theme.accent_style()),
        (Some(Pending::Edit(_)), _) => ("edit", app.theme.state(heddle_agent::AgentState::Blocked)),
        (None, Mode::Insert) => ("insert", app.theme.accent_style()),
        (None, Mode::Prefix) => ("prefix", app.theme.state(heddle_agent::AgentState::Blocked)),
        (None, Mode::Normal) => ("normal", app.theme.dim_style()),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.border_style(app.mode == Mode::Insert))
        .title(Span::styled(format!(" {label} "), style));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = app.composer().map(Composer::text).unwrap_or("");

    if text.is_empty() && app.mode != Mode::Insert {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "press i to write".to_owned(),
                app.theme.dim_style(),
            )),
            inner,
        );
        return None;
    }

    // Wrap in the composer rather than leaving it to Paragraph, because the caret is a
    // byte offset and only the wrap knows which display row it landed on. Letting the
    // widget wrap would put the text in one place and the caret in another.
    let wrapped = app.composer().map(|c| c.wrapped(inner.width))?;

    // Keep the caret in view when the message is taller than the box.
    let height = inner.height.max(1);
    let first = wrapped.caret.0.saturating_sub(height - 1);

    let lines: Vec<Line> = wrapped
        .lines
        .iter()
        .skip(first as usize)
        .map(|l| Line::raw((*l).to_owned()))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);

    let (row, column) = wrapped.caret;
    let x = inner.x + column;
    let y = inner.y + row.saturating_sub(first);
    let on_screen = x < inner.right() && y < inner.bottom();

    if app.mode == Mode::Insert && on_screen {
        frame.set_cursor_position((x, y));
    }

    on_screen.then_some((x, y))
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    // A glyph rather than a word: the healthy states are the common case and do not
    // deserve a sentence. Colour carries the meaning, matching the agent badges.
    let (glyph, word, style) = match app.sync {
        SyncState::Running => ("●", None, Style::default().fg(app.theme.done)),
        SyncState::Initial => ("◐", None, app.theme.accent_style()),
        SyncState::Idle => ("○", None, app.theme.dim_style()),
        // The unhealthy states keep their word: a dim circle is not enough to explain
        // why nothing is arriving.
        SyncState::Offline => ("⚠", Some("offline"), Style::default().fg(app.theme.blocked)),
        SyncState::Terminated => ("✖", Some("stopped"), Style::default().fg(app.theme.error)),
    };

    let mut left = vec![Span::styled(format!(" {glyph} "), style)];
    let mut left_width = 3;
    if let Some(word) = word {
        left.push(Span::styled(word.to_owned(), style));
        left_width += UnicodeWidthStr::width(word);
    }
    // Persistent, not a passing status: an unverified device cannot be sent keys by
    // anyone else, so every silence it causes looks like a bug somewhere else. It stays
    // on screen until it is no longer true.
    if app.device_verified == Some(false) {
        const WARNING: &str = "unverified device — ^a v";
        left.push(Span::raw("  "));
        left.push(Span::styled(
            WARNING.to_owned(),
            Style::default().fg(app.theme.error),
        ));
        left_width += 2 + UnicodeWidthStr::width(WARNING);
    }
    if let Some(status) = &app.status {
        left.push(Span::raw("  "));
        left.push(Span::styled(status.clone(), app.theme.dim_style()));
        left_width += 2 + UnicodeWidthStr::width(status.as_str());
    }
    frame.render_widget(Paragraph::new(Line::from(left)), area);

    // Key hints, evenly spaced and flush right. Drawn from the same module as the
    // bindings themselves, so they cannot advertise a key that does nothing.
    let prefix = app.prefix.label();
    let mut hints: Vec<(String, &str)> = keymap::HINTS
        .iter()
        .map(|b| {
            let keys = if b.prefixed {
                format!("{prefix} {}", b.keys)
            } else {
                b.keys.to_owned()
            };
            (keys, b.action)
        })
        .collect();

    // Shed hints from the least useful end until the row fits, rather than dropping the
    // lot. A narrow terminal should still get `i write`.
    let (cell, total) = loop {
        if hints.is_empty() {
            return;
        }
        let cell = hints
            .iter()
            .map(|(key, label)| {
                UnicodeWidthStr::width(key.as_str()) + 1 + UnicodeWidthStr::width(*label)
            })
            .max()
            .unwrap_or(0)
            + HINT_GAP;
        let total = (cell * hints.len()) as u16;
        if area.width >= total + left_width as u16 {
            break (cell, total);
        }
        hints.pop();
    };

    let mut spans = Vec::with_capacity(hints.len() * 4);
    for (key, label) in &hints {
        let used = UnicodeWidthStr::width(key.as_str()) + 1 + UnicodeWidthStr::width(*label);
        // Pad in front, so the last cell finishes flush against the right edge.
        spans.push(Span::raw(" ".repeat(cell.saturating_sub(used))));
        spans.push(Span::styled(key.clone(), app.theme.accent_style()));
        spans.push(Span::raw(" "));
        spans.push(Span::styled((*label).to_owned(), app.theme.dim_style()));
    }

    let right = Rect {
        x: area.x + area.width - total,
        width: total,
        ..area
    };
    frame.render_widget(Paragraph::new(Line::from(spans)), right);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    fn anchor(event_id: &str, row: u16) -> Anchor {
        Anchor {
            event_id: event_id.into(),
            row,
        }
    }

    #[test]
    fn anchors_move_down_by_the_wrapping_above_them() {
        // The middle line needs three rows at this width, so anything below it sits
        // lower than its unwrapped row claims. Getting this wrong is invisible in a
        // narrow test and obvious in a real pane, where most messages wrap.
        let lines = vec![
            Line::from("short"),
            Line::from("a considerably longer line that has to wrap several times over"),
            Line::from("also short"),
        ];
        let anchors = vec![anchor("$a", 0), anchor("$b", 1), anchor("$c", 2)];

        let mapped = wrapped_anchors(&lines, &anchors, 10);

        assert_eq!(mapped[0].row, 0, "nothing wraps above the first line");
        assert_eq!(mapped[1].row, 1, "one unwrapped line above");
        assert!(
            mapped[2].row > 2,
            "the wrapped line must push the last anchor down, got {}",
            mapped[2].row
        );
    }

    #[test]
    fn anchors_are_left_alone_when_the_width_is_unknown() {
        // Width zero happens on the first frame, before layout has run. Measuring
        // against it would collapse every anchor onto row zero.
        let lines = vec![Line::from("anything")];
        let anchors = vec![anchor("$a", 7)];

        assert_eq!(wrapped_anchors(&lines, &anchors, 0), anchors);
    }
}
