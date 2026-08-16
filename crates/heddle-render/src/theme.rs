//! Colours and glyphs.
//!
//! Kept in one place so the whole UI can be re-themed from config without hunting for
//! literals, and so that tests can assert on semantics rather than on specific colours.

use heddle_agent::{AgentState, ToolStatus};
use ratatui::style::{Color, Modifier, Style};

/// A complete colour scheme.
#[derive(Debug, Clone)]
pub struct Theme {
    pub text: Color,
    pub dim: Color,
    pub accent: Color,
    pub working: Color,
    pub blocked: Color,
    pub done: Color,
    pub error: Color,
    pub added: Color,
    pub removed: Color,
    pub own: Color,
    pub border: Color,
    pub border_focused: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            text: Color::Reset,
            dim: Color::DarkGray,
            accent: Color::Cyan,
            working: Color::Cyan,
            blocked: Color::Yellow,
            done: Color::Green,
            error: Color::Red,
            added: Color::Green,
            removed: Color::Red,
            own: Color::Blue,
            border: Color::DarkGray,
            border_focused: Color::Cyan,
        }
    }
}

impl Theme {
    /// Style for an agent state badge.
    pub fn state(&self, state: AgentState) -> Style {
        let colour = match state {
            AgentState::Working => self.working,
            AgentState::Blocked => self.blocked,
            AgentState::Done => self.done,
            AgentState::Idle => self.dim,
        };
        let style = Style::default().fg(colour);
        // Blocked is the only state that should pull the eye across a full screen of
        // panes, so it is the only one emboldened.
        if state == AgentState::Blocked {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    }

    /// Style for a tab or workspace carrying unread messages.
    ///
    /// A mention is the only unread worth pulling the eye across the screen, so it
    /// alone is accented and emboldened. Plain unread does nothing more than stop the
    /// label being dim, which is enough to tell it apart from a quiet workspace
    /// without competing with a blocked agent for attention.
    pub fn unread(&self, highlight: bool) -> Style {
        if highlight {
            Style::default()
                .fg(self.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.text)
        }
    }

    /// Style for a tool card header.
    pub fn tool(&self, status: ToolStatus) -> Style {
        match status {
            ToolStatus::Running => Style::default().fg(self.working),
            ToolStatus::Ok => Style::default().fg(self.dim),
            ToolStatus::Error => Style::default().fg(self.error).add_modifier(Modifier::BOLD),
        }
    }

    pub fn dim_style(&self) -> Style {
        Style::default().fg(self.dim)
    }

    pub fn error_style(&self) -> Style {
        Style::default().fg(self.error).add_modifier(Modifier::BOLD)
    }

    pub fn accent_style(&self) -> Style {
        Style::default().fg(self.accent)
    }

    pub fn border_style(&self, focused: bool) -> Style {
        Style::default().fg(if focused {
            self.border_focused
        } else {
            self.border
        })
    }
}

/// Glyph for a tool's status.
pub fn tool_glyph(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Running => "◐",
        ToolStatus::Ok => "✓",
        ToolStatus::Error => "✗",
    }
}

/// Disclosure triangle for a collapsible block.
pub fn disclosure(expanded: bool) -> &'static str {
    if expanded {
        "▼"
    } else {
        "▸"
    }
}

/// Format a duration for a tool card header.
pub fn duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let secs = ms / 1000;
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    #[test]
    fn blocked_is_the_only_emboldened_state() {
        let t = Theme::default();
        assert!(t
            .state(AgentState::Blocked)
            .add_modifier
            .contains(Modifier::BOLD));
        for state in [AgentState::Working, AgentState::Done, AgentState::Idle] {
            assert!(!t.state(state).add_modifier.contains(Modifier::BOLD));
        }
    }

    #[test]
    fn plain_unread_is_visible_but_a_mention_is_loud() {
        let t = Theme::default();

        let plain = t.unread(false);
        assert_eq!(
            plain.fg,
            Some(t.text),
            "plain unread must at least stop the label being dim"
        );
        assert_ne!(plain.fg, Some(t.dim));
        assert!(!plain.add_modifier.contains(Modifier::BOLD));

        let mention = t.unread(true);
        assert_eq!(mention.fg, Some(t.accent));
        assert!(mention.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn durations_scale_their_units() {
        assert_eq!(duration(0), "0ms");
        assert_eq!(duration(999), "999ms");
        assert_eq!(duration(1_400), "1.4s");
        assert_eq!(duration(59_900), "59.9s");
        assert_eq!(duration(61_000), "1m01s");
        assert_eq!(duration(3_600_000), "60m00s");
    }
}
