//! Modal input.
//!
//! Three modes, following tmux and herdr so that muscle memory transfers:
//!
//! * **Normal** — motions and single-key actions.
//! * **Prefix** — entered with the prefix key; the next key is one heddle action.
//! * **Insert** — keys go to the composer.
//!
//! See `docs/SPEC.md` §5.3.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use heddle_layout::Dir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    /// The prefix key was pressed; awaiting one action key.
    Prefix,
    Insert,
}

/// Everything the UI can be asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    // Modes
    EnterInsert,
    EnterNormal,

    // Panes
    Split(Dir),
    FocusPane(Dir),
    ResizePane(Dir),
    ZoomPane,
    ClosePane,
    NewThread,

    // Tabs and workspaces
    NextTab,
    PrevTab,
    WorkspaceSwitcher,
    FuzzyJump,
    CommandPalette,

    // Transcript
    ScrollUp(u16),
    ScrollDown(u16),
    ScrollTop,
    ScrollBottom,
    ToggleCard,

    // Prompts
    Approve,
    Deny,

    // Overlays
    ToggleHelp,
    CloseHelp,

    /// Force a full repaint. The conventional terminal escape hatch for a screen that
    /// has been corrupted by something outside the application's control.
    Redraw,

    // Composer
    Insert(char),
    Backspace,
    Submit,
    Newline,

    Quit,
    /// A key that means nothing in this mode. Swallowed.
    None,
}

/// A parsed prefix key, e.g. `ctrl+a`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prefix {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl Default for Prefix {
    fn default() -> Self {
        Self {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::CONTROL,
        }
    }
}

impl Prefix {
    /// Parse a binding such as `ctrl+a`, `alt+x` or `f1`.
    ///
    /// Returns `None` for anything unparseable so the caller can warn and fall back
    /// rather than silently binding the wrong key.
    pub fn parse(spec: &str) -> Option<Self> {
        let spec = spec.trim().to_ascii_lowercase();
        let mut modifiers = KeyModifiers::NONE;
        let mut rest = spec.as_str();

        while let Some((head, tail)) = rest.split_once('+') {
            match head {
                "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
                "alt" | "meta" => modifiers |= KeyModifiers::ALT,
                "shift" => modifiers |= KeyModifiers::SHIFT,
                _ => return None,
            }
            rest = tail;
        }

        let code = match rest {
            "" => return None,
            "esc" | "escape" => KeyCode::Esc,
            "tab" => KeyCode::Tab,
            "space" => KeyCode::Char(' '),
            "enter" | "return" => KeyCode::Enter,
            f if f.starts_with('f') && f[1..].parse::<u8>().is_ok() => {
                KeyCode::F(f[1..].parse().ok()?)
            }
            other => {
                let mut chars = other.chars();
                let c = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                KeyCode::Char(c)
            }
        };

        Some(Self { code, modifiers })
    }

    fn matches(&self, key: &KeyEvent) -> bool {
        // Compare only the modifiers we care about; terminals set extras such as
        // KEYPAD or NONE inconsistently.
        const RELEVANT: KeyModifiers = KeyModifiers::CONTROL
            .union(KeyModifiers::ALT)
            .union(KeyModifiers::SHIFT);
        key.code == self.code && (key.modifiers & RELEVANT) == (self.modifiers & RELEVANT)
    }

    /// Short label for the status bar, e.g. `^a`.
    ///
    /// Derived from the parsed binding rather than hardcoded, so a custom `ui.prefix`
    /// cannot leave the hints advertising a key that does nothing.
    pub fn label(&self) -> String {
        let mut out = String::new();
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            out.push('^');
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            out.push('⌥');
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            out.push('⇧');
        }
        match self.code {
            KeyCode::Char(' ') => out.push_str("spc"),
            KeyCode::Char(c) => out.push(c),
            KeyCode::Esc => out.push_str("esc"),
            KeyCode::Tab => out.push_str("tab"),
            KeyCode::Enter => out.push_str("ret"),
            KeyCode::F(n) => {
                out.push('f');
                out.push_str(&n.to_string());
            }
            _ => out.push('?'),
        }
        out
    }
}

/// One documented binding.
///
/// The `<prefix> ?` overlay and the status-bar hints both read these tables, which sit
/// beside `map_prefix` and `map_normal` so they cannot quietly drift from the bindings
/// themselves.
pub struct Binding {
    /// Keys, without the prefix.
    pub keys: &'static str,
    pub action: &'static str,
    /// Whether the prefix must be pressed first.
    pub prefixed: bool,
}

/// Shorthand for a table entry.
const fn b(keys: &'static str, action: &'static str, prefixed: bool) -> Binding {
    Binding {
        keys,
        action,
        prefixed,
    }
}

/// Every binding worth documenting, in the order the overlay lists them.
pub const BINDINGS: &[Binding] = &[
    b("i", "write a message", false),
    b("enter", "send", false),
    b("shift+enter", "newline", false),
    b("esc", "normal mode", false),
    b("y / n", "approve / deny", false),
    b("tab", "toggle tool card", false),
    b("j / k", "scroll", false),
    b("^u / ^d", "half-page scroll", false),
    b("g / G", "top / bottom", false),
    b(":", "command palette", false),
    b("^l", "redraw the screen", false),
    b("n / p", "next / prev tab", true),
    b("| / -", "split right / down", true),
    b("h j k l", "focus pane", true),
    b("H J K L", "resize pane", true),
    b("z", "zoom pane", true),
    b("x", "close pane", true),
    b("c", "new thread", true),
    b("w", "workspace switcher", true),
    b("f", "fuzzy jump", true),
    b("?", "this help", true),
    b("q", "quit", true),
];

/// The handful of bindings terse enough for the status bar, most useful first.
///
/// Separate from [`BINDINGS`] because a status bar and a reference table want different
/// prose: `next / prev tab` reads correctly in the overlay and is far too wide for one
/// line beside four others. The overlay documents everything; this is the teaser.
///
/// Ordered by usefulness, because a narrow terminal drops them from the end.
pub const HINTS: &[Binding] = &[
    b("i", "write", false),
    b("n", "tab", true),
    b("?", "help", true),
    b("|", "split", true),
    b("f", "jump", true),
];

/// Translate a key press into an [`Action`], given the current mode.
///
/// Returns the action and the mode to switch to.
pub fn map(key: KeyEvent, mode: Mode, prefix: Prefix) -> (Action, Mode) {
    // The prefix key is live in Normal and Insert alike; otherwise there would be no
    // way to split a pane without first leaving the composer.
    if mode != Mode::Prefix && prefix.matches(&key) {
        return (Action::None, Mode::Prefix);
    }

    match mode {
        Mode::Prefix => (map_prefix(key), Mode::Normal),
        Mode::Normal => map_normal(key),
        Mode::Insert => map_insert(key),
    }
}

fn map_prefix(key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('|') | KeyCode::Char('\\') => Action::Split(Dir::Right),
        KeyCode::Char('-') | KeyCode::Char('_') => Action::Split(Dir::Down),

        KeyCode::Char('h') => Action::FocusPane(Dir::Left),
        KeyCode::Char('j') => Action::FocusPane(Dir::Down),
        KeyCode::Char('k') => Action::FocusPane(Dir::Up),
        KeyCode::Char('l') => Action::FocusPane(Dir::Right),

        KeyCode::Char('H') => Action::ResizePane(Dir::Left),
        KeyCode::Char('J') => Action::ResizePane(Dir::Down),
        KeyCode::Char('K') => Action::ResizePane(Dir::Up),
        KeyCode::Char('L') => Action::ResizePane(Dir::Right),

        KeyCode::Char('z') => Action::ZoomPane,
        KeyCode::Char('x') => Action::ClosePane,
        KeyCode::Char('c') => Action::NewThread,

        KeyCode::Char('n') => Action::NextTab,
        KeyCode::Char('p') => Action::PrevTab,
        KeyCode::Char('w') => Action::WorkspaceSwitcher,
        KeyCode::Char('f') => Action::FuzzyJump,
        KeyCode::Char('?') | KeyCode::Char('/') => Action::ToggleHelp,

        KeyCode::Char('q') => Action::Quit,
        _ => Action::None,
    }
}

fn map_normal(key: KeyEvent) -> (Action, Mode) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        // Escape closes the help overlay. Harmless when nothing is open.
        KeyCode::Esc => (Action::CloseHelp, Mode::Normal),
        KeyCode::Char('i') => (Action::EnterInsert, Mode::Insert),
        KeyCode::Char(':') => (Action::CommandPalette, Mode::Normal),

        KeyCode::Char('y') => (Action::Approve, Mode::Normal),
        KeyCode::Char('n') => (Action::Deny, Mode::Normal),

        KeyCode::Tab => (Action::ToggleCard, Mode::Normal),

        KeyCode::Char('l') if ctrl => (Action::Redraw, Mode::Normal),
        KeyCode::Char('u') if ctrl => (Action::ScrollUp(10), Mode::Normal),
        KeyCode::Char('d') if ctrl => (Action::ScrollDown(10), Mode::Normal),
        KeyCode::Char('k') | KeyCode::Up => (Action::ScrollUp(1), Mode::Normal),
        KeyCode::Char('j') | KeyCode::Down => (Action::ScrollDown(1), Mode::Normal),
        KeyCode::PageUp => (Action::ScrollUp(20), Mode::Normal),
        KeyCode::PageDown => (Action::ScrollDown(20), Mode::Normal),
        KeyCode::Char('g') => (Action::ScrollTop, Mode::Normal),
        KeyCode::Char('G') => (Action::ScrollBottom, Mode::Normal),

        KeyCode::Char('q') => (Action::Quit, Mode::Normal),
        _ => (Action::None, Mode::Normal),
    }
}

fn map_insert(key: KeyEvent) -> (Action, Mode) {
    match key.code {
        KeyCode::Esc => (Action::EnterNormal, Mode::Normal),
        KeyCode::Backspace => (Action::Backspace, Mode::Insert),
        // Shift+Enter inserts a newline; plain Enter sends. Matches every chat client
        // and every coding agent REPL.
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            (Action::Newline, Mode::Insert)
        }
        KeyCode::Enter => (Action::Submit, Mode::Insert),
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            (Action::Insert(c), Mode::Insert)
        }
        _ => (Action::None, Mode::Insert),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn parses_prefix_specs() {
        assert_eq!(
            Prefix::parse("ctrl+a"),
            Some(Prefix {
                code: KeyCode::Char('a'),
                modifiers: KeyModifiers::CONTROL
            })
        );
        assert_eq!(
            Prefix::parse("ALT+X"),
            Some(Prefix {
                code: KeyCode::Char('x'),
                modifiers: KeyModifiers::ALT
            })
        );
        assert_eq!(Prefix::parse("f1").expect("f1").code, KeyCode::F(1));
    }

    #[test]
    fn rejects_unparseable_prefixes_rather_than_guessing() {
        assert_eq!(Prefix::parse("hyper+a"), None);
        assert_eq!(Prefix::parse("ctrl+"), None);
        assert_eq!(Prefix::parse("ctrl+abc"), None);
        assert_eq!(Prefix::parse(""), None);
    }

    #[test]
    fn default_prefix_avoids_the_tmux_and_herdr_binding() {
        // ctrl+b belongs to whatever multiplexer heddle is nested inside.
        assert_eq!(Prefix::default().code, KeyCode::Char('a'));
        assert_ne!(Prefix::default().code, KeyCode::Char('b'));
    }

    #[test]
    fn prefix_then_action_splits_a_pane() {
        let p = Prefix::default();
        let (action, mode) = map(ctrl('a'), Mode::Normal, p);
        assert_eq!(action, Action::None);
        assert_eq!(mode, Mode::Prefix);

        let (action, mode) = map(key('|'), Mode::Prefix, p);
        assert_eq!(action, Action::Split(Dir::Right));
        assert_eq!(mode, Mode::Normal, "prefix mode is single-shot");
    }

    #[test]
    fn the_prefix_works_from_inside_the_composer() {
        // Otherwise you would have to leave insert mode to split a pane.
        let p = Prefix::default();
        let (_, mode) = map(ctrl('a'), Mode::Insert, p);
        assert_eq!(mode, Mode::Prefix);
    }

    #[test]
    fn an_unbound_prefix_key_is_swallowed_not_inserted() {
        let p = Prefix::default();
        let (action, mode) = map(key('%'), Mode::Prefix, p);
        assert_eq!(action, Action::None);
        assert_eq!(mode, Mode::Normal);
    }

    #[test]
    fn insert_mode_types_characters_that_are_normal_mode_commands() {
        let p = Prefix::default();
        for c in ['q', 'y', 'n', 'i', 'g'] {
            assert_eq!(
                map(key(c), Mode::Insert, p),
                (Action::Insert(c), Mode::Insert),
                "typing {c:?} must not trigger a command"
            );
        }
    }

    #[test]
    fn enter_sends_and_shift_enter_breaks_the_line() {
        let p = Prefix::default();
        assert_eq!(
            map(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                Mode::Insert,
                p
            ),
            (Action::Submit, Mode::Insert)
        );
        assert_eq!(
            map(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
                Mode::Insert,
                p
            ),
            (Action::Newline, Mode::Insert)
        );
    }

    #[test]
    fn approvals_are_one_keypress_in_normal_mode() {
        let p = Prefix::default();
        assert_eq!(map(key('y'), Mode::Normal, p).0, Action::Approve);
        assert_eq!(map(key('n'), Mode::Normal, p).0, Action::Deny);
    }

    #[test]
    fn escape_leaves_insert_mode() {
        let p = Prefix::default();
        let (action, mode) = map(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            Mode::Insert,
            p,
        );
        assert_eq!(action, Action::EnterNormal);
        assert_eq!(mode, Mode::Normal);
    }

    #[test]
    fn focus_and_resize_use_the_same_letters_in_different_cases() {
        let p = Prefix::default();
        assert_eq!(
            map(key('h'), Mode::Prefix, p).0,
            Action::FocusPane(Dir::Left)
        );
        assert_eq!(
            map(
                KeyEvent::new(KeyCode::Char('H'), KeyModifiers::SHIFT),
                Mode::Prefix,
                p
            )
            .0,
            Action::ResizePane(Dir::Left)
        );
    }

    #[test]
    fn a_custom_prefix_is_honoured() {
        let p = Prefix::parse("alt+space").unwrap_or_default();
        let alt_space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::ALT);
        assert_eq!(map(alt_space, Mode::Normal, p).1, Mode::Prefix);
        // And the old default no longer triggers it.
        assert_eq!(map(ctrl('a'), Mode::Normal, p).1, Mode::Normal);
    }

    #[test]
    fn the_prefix_label_follows_the_binding() {
        // The status bar advertises this, so a stale label would send users to a key
        // that does nothing.
        assert_eq!(Prefix::default().label(), "^a");
        assert_eq!(Prefix::parse("alt+x").expect("alt+x").label(), "⌥x");
        assert_eq!(Prefix::parse("ctrl+space").expect("spc").label(), "^spc");
        assert_eq!(Prefix::parse("f5").expect("f5").label(), "f5");
    }

    #[test]
    fn the_help_overlay_is_reachable_and_dismissable() {
        let p = Prefix::default();
        assert_eq!(map(key('?'), Mode::Prefix, p).0, Action::ToggleHelp);
        assert_eq!(
            map(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                Mode::Normal,
                p
            )
            .0,
            Action::CloseHelp
        );
    }

    #[test]
    fn every_documented_binding_is_actually_bound() {
        // The overlay and the status hints read these tables. A row that no key
        // produces would be a lie told in the UI.
        let p = Prefix::default();
        for binding in BINDINGS.iter().chain(HINTS) {
            // Only single-key prefixed rows are mechanically checkable; the rest
            // document chords and ranges.
            if !binding.prefixed || binding.keys.chars().count() != 1 {
                continue;
            }
            let c = binding.keys.chars().next().expect("one char");
            assert_ne!(
                map(key(c), Mode::Prefix, p).0,
                Action::None,
                "a table documents `{c}` but map_prefix ignores it"
            );
        }
    }

    #[test]
    fn status_hints_stay_short_enough_to_fit() {
        // Five hints on one line beside the sync glyph. The overlay carries the prose;
        // if these grow, the status bar silently sheds them instead.
        assert!(HINTS.len() <= 5, "the status bar is not a reference table");
        for hint in HINTS {
            assert!(
                hint.action.len() <= 6,
                "`{}` is overlay prose, not a status hint",
                hint.action
            );
        }
        assert!(
            HINTS.iter().any(|h| h.keys == "?"),
            "the discoverability hint must itself be discoverable"
        );
    }
}
