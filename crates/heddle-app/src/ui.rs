//! Terminal drawing.
//!
//! Pure layout and painting. All decisions live in [`crate::app`]; this module only
//! turns state into cells.

use crate::app::App;
use crate::keymap::Mode;
use heddle_matrix::SyncState;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

/// Height of the composer, including its border.
const COMPOSER_HEIGHT: u16 = 3;

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
}

fn draw_workspace_bar(frame: &mut Frame, app: &App, area: Rect) {
    let focused_id = app.workspaces.focused().map(|w| w.id.clone());
    let mut spans = Vec::new();

    for workspace in &app.workspaces.items {
        let is_focused = Some(&workspace.id) == focused_id.as_ref();
        let state = workspace.state();
        let style = if is_focused {
            app.theme.state(state).add_modifier(Modifier::REVERSED)
        } else {
            app.theme.state(state)
        };
        spans.push(Span::styled(format!(" {} ", workspace.title), style));
    }

    if spans.is_empty() {
        spans.push(Span::styled(
            " connecting… ".to_owned(),
            app.theme.dim_style(),
        ));
    }

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

fn draw_tab_bar(frame: &mut Frame, app: &App, area: Rect) {
    let Some(workspace) = app.workspaces.focused() else {
        return;
    };
    let focused_room = workspace.focused_tab().map(|t| t.room_id.clone());

    let mut spans = Vec::new();
    for tab in &workspace.tabs {
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
        spans.push(Span::styled(format!(" {label} "), style));
    }

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
        frame.render_widget(block, placement.rect);

        // Only the focused pane shows the loaded transcript for now; per-pane
        // transcripts arrive with the M4 workspace work.
        if placement.is_focused {
            let height = inner.height;
            let total = lines.len() as u16;
            // `scroll` counts up from the bottom, so translate it to a top offset.
            let offset = total
                .saturating_sub(height)
                .saturating_sub(scroll)
                .min(total);
            frame.render_widget(
                Paragraph::new(lines.clone())
                    .wrap(Wrap { trim: false })
                    .scroll((offset, 0)),
                inner,
            );
        }
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
    let (sync_text, sync_style) = match app.sync {
        SyncState::Running => ("synced", app.theme.dim_style()),
        SyncState::Initial => ("syncing…", app.theme.accent_style()),
        SyncState::Offline => ("offline", Style::default().fg(app.theme.blocked)),
        SyncState::Terminated => ("stopped", Style::default().fg(app.theme.error)),
        SyncState::Idle => ("idle", app.theme.dim_style()),
    };

    let mut spans = vec![Span::styled(sync_text.to_owned(), sync_style)];
    if let Some(status) = &app.status {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(status.clone(), app.theme.dim_style()));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}
