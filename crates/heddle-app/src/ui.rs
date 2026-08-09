//! Terminal drawing.
//!
//! Pure layout and painting. All decisions live in [`crate::app`]; this module only
//! turns state into cells.

use crate::app::{App, Hit};
use crate::keymap::{self, Mode};
use heddle_matrix::SyncState;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

/// Height of the composer, including its border.
const COMPOSER_HEIGHT: u16 = 3;

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
            Constraint::Length(COMPOSER_HEIGHT),
            Constraint::Length(1), // status
        ])
        .split(frame.area());

    draw_workspace_bar(frame, app, chunks[0]);
    draw_tab_bar(frame, app, chunks[1]);
    draw_panes(frame, app, chunks[2]);
    draw_composer(frame, app, chunks[3]);
    draw_status(frame, app, chunks[4]);

    // Last, so it sits above everything.
    if app.help {
        draw_help(frame, app, frame.area());
    }
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
            let style = if is_focused {
                app.theme.state(state).add_modifier(Modifier::REVERSED)
            } else {
                app.theme.state(state)
            };
            (workspace.title.clone(), style)
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
            let mut style = app.theme.state(state);
            if is_focused {
                style = style.add_modifier(Modifier::REVERSED);
            }

            let mut label = tab.title.clone();
            if tab.is_encrypted {
                label.push_str(" 🔒");
            }
            if tab.highlight_count > 0 {
                label.push_str(&format!(" ({})", tab.highlight_count));
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

    // Snapshot what each pane needs before borrowing the frame, so drawing does not
    // fight the borrow checker over `app`.
    let panes: Vec<_> = placements
        .iter()
        .map(|placement| {
            let pane = app
                .workspaces
                .focused()
                .and_then(|w| w.focused_tab())
                .and_then(|t| t.panes.iter().find(|p| p.id == placement.id));
            (
                *placement,
                pane.map(|p| p.header()).unwrap_or_default(),
                pane.map(|p| p.state).unwrap_or_default(),
            )
        })
        .collect();

    let entries = app.focused_entries().to_vec();
    let lines = heddle_render::transcript::render(
        &entries,
        &app.theme,
        &app.render_options,
        &app.overrides,
    );
    let scroll = app
        .focused_view()
        .and_then(|v| app.scroll.get(&v).copied())
        .unwrap_or(0);

    for (placement, header, state) in panes {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(app.theme.border_style(placement.is_focused))
            .title(Span::styled(header, app.theme.state(state)));
        let inner = block.inner(placement.rect);

        // Only the focused pane shows the loaded transcript for now; per-pane
        // transcripts arrive with the M4 workspace work.
        if placement.is_focused {
            // Keep one column clear on the right. `unicode-width` and the terminal
            // disagree about emoji whose East Asian Width is Neutral but which render
            // as two cells — U+1F54A DOVE and friends. ratatui lays them out as one
            // column, the terminal paints two, and the overflow lands on whatever is to
            // the right. Without the gutter that is the border, which is why the edge
            // went dashed wherever such a glyph happened to end a line.
            let text_area = Rect {
                width: inner.width.saturating_sub(TRANSCRIPT_GUTTER),
                ..inner
            };

            let paragraph = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });

            // The *wrapped* row count, not `lines.len()`. A single long message can
            // occupy many rows, and scrolling is measured in rows, so counting
            // unwrapped lines under-reports the scrollable extent and strands the
            // bottom of the transcript out of reach.
            let total = paragraph.line_count(text_area.width) as u16;
            let height = text_area.height;

            // Hand the geometry back: only the renderer knows the wrapped extent at
            // this width, and both scroll clamping and pagination depend on it.
            app.rendered_lines = total;
            app.viewport_height = height;

            // `scroll` counts up from the bottom, so translate it to a top offset.
            let max_scroll = total.saturating_sub(height);
            let offset = max_scroll.saturating_sub(scroll.min(max_scroll));
            frame.render_widget(paragraph.scroll((offset, 0)), text_area);
        }

        // Border last, deliberately. Belt and braces alongside the gutter: whatever the
        // transcript contains, the chrome is painted over it rather than under it.
        frame.render_widget(block, placement.rect);
    }
}

fn draw_composer(frame: &mut Frame, app: &App, area: Rect) {
    let (label, style) = match app.mode {
        Mode::Insert => ("insert", app.theme.accent_style()),
        Mode::Prefix => ("prefix", app.theme.state(heddle_agent::AgentState::Blocked)),
        Mode::Normal => ("normal", app.theme.dim_style()),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.border_style(app.mode == Mode::Insert))
        .title(Span::styled(format!(" {label} "), style));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = if app.composer.is_empty() && app.mode != Mode::Insert {
        Span::styled("press i to write".to_owned(), app.theme.dim_style())
    } else {
        Span::raw(app.composer.clone())
    };
    frame.render_widget(Paragraph::new(Line::from(text)), inner);

    if app.mode == Mode::Insert {
        // Place the caret at the end of the last line of the composer.
        let last = app.composer.lines().last().unwrap_or_default();
        let x = inner.x + last.chars().count() as u16;
        let y = inner.y + app.composer.matches('\n').count() as u16;
        if x < inner.right() && y < inner.bottom() {
            frame.set_cursor_position((x, y));
        }
    }
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
