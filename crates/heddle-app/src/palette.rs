//! The command palette.
//!
//! A searchable list of the same commands the prefix bindings run, showing the key
//! beside each one.
//!
//! A table rather than a reflection of the keymap: not every documented binding is one
//! command -- `h j k l` is four -- and not every command needs a key. A test asserts
//! that each entry claiming a key really is what that key does.
//!
//! See `docs/SPEC.md` §5.3.

use crate::keymap::{Action, Prefix};
use heddle_layout::Dir;

/// One entry in the palette.
pub struct Command {
    /// Shown, and searched.
    pub name: &'static str,
    /// The equivalent key, without the prefix. Empty when the command has no binding.
    pub keys: &'static str,
    /// Whether `keys` needs the prefix pressed first.
    pub prefixed: bool,
    pub action: Action,
}

const fn c(name: &'static str, keys: &'static str, prefixed: bool, action: Action) -> Command {
    Command {
        name,
        keys,
        prefixed,
        action,
    }
}

/// Everything the palette can run, ordered by how often it is wanted -- which is also
/// the order shown before anything is typed. Excludes the composer's editing keys, which
/// are only meaningful while the palette is shut.
pub const COMMANDS: &[Command] = &[
    c("split right", "|", true, Action::Split(Dir::Right)),
    c("split down", "-", true, Action::Split(Dir::Down)),
    c("close pane", "x", true, Action::ClosePane),
    c("zoom pane", "z", true, Action::ZoomPane),
    c("focus pane left", "h", true, Action::FocusPane(Dir::Left)),
    c("focus pane down", "j", true, Action::FocusPane(Dir::Down)),
    c("focus pane up", "k", true, Action::FocusPane(Dir::Up)),
    c("focus pane right", "l", true, Action::FocusPane(Dir::Right)),
    c("resize pane left", "H", true, Action::ResizePane(Dir::Left)),
    c("resize pane down", "J", true, Action::ResizePane(Dir::Down)),
    c("resize pane up", "K", true, Action::ResizePane(Dir::Up)),
    c(
        "resize pane right",
        "L",
        true,
        Action::ResizePane(Dir::Right),
    ),
    c("new thread", "c", true, Action::NewThread),
    c("thread picker", "t", true, Action::OpenThreads),
    c("next room", "n", true, Action::NextTab),
    c("previous room", "p", true, Action::PrevTab),
    c("next workspace", "w", true, Action::NextWorkspace),
    c("previous workspace", "W", true, Action::PrevWorkspace),
    c("reply to selection", "r", false, Action::Reply),
    c("edit selection", "e", false, Action::EditMessage),
    c("delete selection", "D", false, Action::RedactMessage),
    c("react to selection", "r", true, Action::ReactToSelected),
    c("emoji into composer", "e", true, Action::EmojiIntoComposer),
    c("verify this device", "v", true, Action::StartVerification),
    c("recovery key", "R", true, Action::OpenRecovery),
    c("accept invitation", "a", true, Action::JoinRoom),
    c("leave room", "X", true, Action::LeaveRoom),
    c("keyboard help", "?", true, Action::ToggleHelp),
    c("redraw the screen", "^l", false, Action::Redraw),
    c("quit", "q", true, Action::Quit),
];

/// How many matches to show at once.
pub const MAX_ROWS: usize = 12;

#[derive(Debug, Clone)]
pub struct Palette {
    pub query: String,
    /// Indices into [`COMMANDS`], best first.
    pub matches: Vec<usize>,
    pub selected: usize,
}

impl Default for Palette {
    fn default() -> Self {
        Self::new()
    }
}

impl Palette {
    pub fn new() -> Self {
        let mut palette = Self {
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
        };
        palette.refilter();
        palette
    }

    /// Recompute the matches for the current query.
    ///
    /// Contiguous matches first, then the earliest, then the table's own order: "sp"
    /// puts "split right" above "previous workspace".
    pub fn refilter(&mut self) {
        self.selected = 0;

        if self.query.is_empty() {
            self.matches = (0..COMMANDS.len()).collect();
            return;
        }

        let query = self.query.to_lowercase();
        let mut scored: Vec<(u16, u16, u16, usize)> = COMMANDS
            .iter()
            .enumerate()
            .filter_map(|(i, command)| {
                score(command.name, &query).map(|s| (s.strays, s.gaps, s.start, i))
            })
            .collect();
        // Stable, so equally good matches keep the table's ordering by usefulness.
        scored.sort_by_key(|(strays, gaps, start, _)| (*strays, *gaps, *start));
        self.matches = scored.into_iter().map(|(_, _, _, i)| i).collect();
    }

    pub fn push(&mut self, ch: char) {
        self.query.push(ch);
        self.refilter();
    }

    pub fn pop(&mut self) {
        self.query.pop();
        self.refilter();
    }

    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn down(&mut self) {
        self.selected = (self.selected + 1).min(self.matches.len().saturating_sub(1));
    }

    /// The highlighted command, if the query matched anything.
    pub fn chosen(&self) -> Option<&'static Command> {
        self.matches
            .get(self.selected)
            .and_then(|i| COMMANDS.get(*i))
    }

    /// The commands to draw, as `(index into COMMANDS, is_selected)`.
    ///
    /// Scrolled so the highlight stays on screen once the selection walks past the
    /// visible rows.
    pub fn visible(&self, rows: usize) -> impl Iterator<Item = (&'static Command, bool)> + '_ {
        let first = self.selected.saturating_sub(rows.saturating_sub(1));
        self.matches
            .iter()
            .enumerate()
            .skip(first)
            .take(rows)
            .filter_map(move |(position, i)| {
                COMMANDS.get(*i).map(|c| (c, position == self.selected))
            })
    }
}

/// How well a query fits a name, as a sortable penalty. Lower is better.
///
/// Shared with the mention picker, so the two rank names by the same rules.
pub fn rank(name: &str, query: &str) -> Option<(u16, u16, u16)> {
    let query = query.to_lowercase();
    score(name, &query).map(|s| (s.strays, s.gaps, s.start))
}

/// How well a query fits a name. Every field is a penalty, so lower is better.
struct Score {
    /// Matched characters that neither start a word nor continue the previous match.
    /// This is what makes initials work: in "next workspace" both letters of "nw" begin
    /// a word, so it scores zero and beats "new thread", where the `w` is stranded in
    /// the middle of one.
    strays: u16,
    /// Breaks between matched runs. Separates a true substring from a scattered match.
    gaps: u16,
    /// Where the match begins, so an earlier one wins an otherwise exact tie.
    start: u16,
}

/// Score `name` against an already-lowercased `query`, or `None` if it does not match.
///
/// A subsequence match, so "nw" finds "next workspace" without typing the space, and
/// "split" finds "split right" the obvious way.
fn score(name: &str, query: &str) -> Option<Score> {
    let chars: Vec<char> = name.chars().map(|ch| ch.to_ascii_lowercase()).collect();
    let mut cursor = 0usize;
    let mut start = None;
    let mut strays = 0u16;
    let mut gaps = 0u16;
    let mut previous: Option<usize> = None;

    for wanted in query.chars() {
        let index = chars[cursor..].iter().position(|ch| *ch == wanted)? + cursor;

        let starts_a_word = index == 0 || matches!(chars.get(index - 1), Some(' ' | '-'));
        let continues_the_match = previous.is_some_and(|p| index == p + 1);
        if !starts_a_word && !continues_the_match {
            strays += 1;
        }
        if start.is_none() {
            start = Some(index);
        } else if !continues_the_match {
            gaps += 1;
        }

        previous = Some(index);
        cursor = index + 1;
    }

    Some(Score {
        strays,
        gaps,
        start: start.unwrap_or(0) as u16,
    })
}

/// The key that runs `command`, written the way the user would press it.
pub fn keys_for(command: &Command, prefix: Prefix) -> String {
    if command.keys.is_empty() {
        String::new()
    } else if command.prefixed {
        format!("{} {}", prefix.label(), command.keys)
    } else {
        command.keys.to_owned()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use crate::keymap::{map, Mode};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn typed(query: &str) -> Palette {
        let mut palette = Palette::new();
        for ch in query.chars() {
            palette.push(ch);
        }
        palette
    }

    #[test]
    fn an_unfiltered_palette_offers_everything() {
        let palette = Palette::new();
        assert_eq!(palette.matches.len(), COMMANDS.len());
        assert_eq!(palette.chosen().expect("first").name, "split right");
    }

    #[test]
    fn a_substring_outranks_a_scattered_match() {
        // "previous workspace" contains an s and a p, but not together.
        let palette = typed("sp");
        assert_eq!(palette.chosen().expect("match").name, "split right");
    }

    #[test]
    fn initials_find_a_command_without_typing_the_space() {
        assert_eq!(typed("nw").chosen().expect("match").name, "next workspace");
        assert_eq!(
            typed("pw").chosen().expect("match").name,
            "previous workspace"
        );
        assert_eq!(typed("sr").chosen().expect("match").name, "split right");
    }

    #[test]
    fn a_word_start_beats_a_letter_stranded_mid_word() {
        // "new thread" also contains an n then a w, but its w is in the middle of a
        // word. Getting this backwards is what makes a palette feel unusable.
        let palette = typed("nw");
        let names: Vec<_> = palette.matches.iter().map(|i| COMMANDS[*i].name).collect();
        let workspace = names.iter().position(|n| *n == "next workspace");
        let thread = names.iter().position(|n| *n == "new thread");
        assert!(workspace < thread, "ranked {names:?}");
    }

    #[test]
    fn searching_is_case_insensitive() {
        assert_eq!(typed("ZOOM").chosen().expect("match").name, "zoom pane");
    }

    #[test]
    fn a_query_that_matches_nothing_chooses_nothing() {
        let palette = typed("xyzzy");
        assert!(palette.matches.is_empty());
        assert!(palette.chosen().is_none());
    }

    #[test]
    fn deleting_the_query_restores_the_full_list() {
        let mut palette = typed("zoom");
        for _ in 0.."zoom".len() {
            palette.pop();
        }
        assert_eq!(palette.matches.len(), COMMANDS.len());
    }

    #[test]
    fn moving_the_selection_stops_at_both_ends() {
        let mut palette = Palette::new();
        palette.up();
        assert_eq!(palette.selected, 0);
        for _ in 0..COMMANDS.len() * 2 {
            palette.down();
        }
        assert_eq!(palette.selected, COMMANDS.len() - 1);
    }

    #[test]
    fn refiltering_returns_the_selection_to_the_top() {
        // Otherwise a narrowing query leaves the highlight pointing at whatever now
        // happens to occupy that row, and enter runs a command nobody chose.
        let mut palette = Palette::new();
        palette.down();
        palette.down();
        palette.push('q');
        assert_eq!(palette.selected, 0);
    }

    #[test]
    fn the_visible_window_follows_the_selection() {
        let mut palette = Palette::new();
        for _ in 0..MAX_ROWS + 2 {
            palette.down();
        }
        let shown: Vec<_> = palette.visible(MAX_ROWS).collect();
        assert_eq!(shown.len(), MAX_ROWS);
        assert!(
            shown.iter().any(|(_, selected)| *selected),
            "the highlight must stay on screen"
        );
    }

    #[test]
    fn every_command_that_claims_a_key_is_what_that_key_does() {
        // The palette teaches the keybinding beside each command. A row advertising a
        // key that does something else would teach the wrong thing.
        let prefix = Prefix::default();
        for command in COMMANDS {
            if command.keys.chars().count() != 1 {
                continue;
            }
            let ch = command.keys.chars().next().expect("one char");
            let mode = if command.prefixed {
                Mode::Prefix
            } else {
                Mode::Normal
            };
            let modifiers = if ch.is_ascii_uppercase() {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            let (action, _) = map(KeyEvent::new(KeyCode::Char(ch), modifiers), mode, prefix);
            assert_eq!(
                action, command.action,
                "the palette says `{}` runs {:?}, but that key runs {action:?}",
                command.name, command.action
            );
        }
    }

    #[test]
    fn keys_are_written_with_the_configured_prefix() {
        let prefix = Prefix::parse("alt+x").expect("alt+x");
        let split = COMMANDS
            .iter()
            .find(|c| c.name == "split right")
            .expect("split right");
        assert_eq!(keys_for(split, prefix), "⌥x |");

        let reply = COMMANDS
            .iter()
            .find(|c| c.name == "reply to selection")
            .expect("reply");
        assert_eq!(keys_for(reply, prefix), "r", "unprefixed keys stand alone");
    }

    #[test]
    fn no_command_is_listed_twice() {
        let mut names: Vec<_> = COMMANDS.iter().map(|c| c.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "a duplicate row is a confusing row");
    }
}
