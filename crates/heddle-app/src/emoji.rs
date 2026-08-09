//! The emoji picker.
//!
//! One overlay serving two jobs: reacting to a message, and putting an emoji in the
//! composer. They differ only in what happens on accept, so they share the searching and
//! the list rather than growing two near-identical overlays.

use emojis::Emoji;

/// How many matches the overlay will show. Searching "s" matches most of the set, and a
/// list longer than the screen is not a list anyone reads.
pub const MAX_MATCHES: usize = 60;

/// The starting set, before anything is typed.
///
/// A curated shortlist rather than the first sixty of the full table, which is smileys in
/// codepoint order and no use to anyone. These are what a chat about work actually uses;
/// searching reaches the rest.
const COMMON: &[&str] = &[
    "👍", "👎", "✅", "❌", "🎉", "🚀", "🔥", "👀", "🤔", "🙏", "💯", "😄", "😂", "😅", "🙌", "👏",
    "💡", "🐛", "✨", "📌", "⏳", "🧵", "🔧", "📝",
];

/// What the picker will do with the emoji once one is chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Insert it into the composer at the caret.
    Composer,
    /// React to this event.
    Reaction { event_id: String },
}

#[derive(Debug, Clone)]
pub struct Picker {
    pub target: Target,
    pub query: String,
    pub matches: Vec<&'static Emoji>,
    pub selected: usize,
}

impl Picker {
    pub fn new(target: Target) -> Self {
        let mut picker = Self {
            target,
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
        };
        picker.refilter();
        picker
    }

    /// Recompute the matches for the current query.
    ///
    /// Matching is a case-insensitive substring of the name or any shortcode, which is
    /// what makes both "thumb" and "+1" find 👍. Shortcode matches sort first: typing an
    /// exact shortcode should not be beaten by some longer name that merely contains it.
    pub fn refilter(&mut self) {
        self.selected = 0;

        if self.query.is_empty() {
            self.matches = COMMON.iter().filter_map(|e| emojis::get(e)).collect();
            return;
        }

        let query = self.query.to_lowercase();
        let mut exact = Vec::new();
        let mut partial = Vec::new();

        for emoji in emojis::iter() {
            let shortcode_hit = emoji.shortcodes().any(|s| s.contains(&query));
            let name_hit = emoji.name().to_lowercase().contains(&query);
            if !shortcode_hit && !name_hit {
                continue;
            }
            if emoji.shortcodes().any(|s| s == query) {
                exact.push(emoji);
            } else {
                partial.push(emoji);
            }
            if exact.len() + partial.len() >= MAX_MATCHES {
                break;
            }
        }

        exact.append(&mut partial);
        self.matches = exact;
    }

    pub fn push(&mut self, c: char) {
        self.query.push(c);
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

    /// The highlighted emoji, if the query matched anything.
    pub fn chosen(&self) -> Option<&'static str> {
        self.matches.get(self.selected).map(|e| e.as_str())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn the_starting_list_is_the_common_set_and_every_entry_resolves() {
        let picker = Picker::new(Target::Composer);
        assert_eq!(
            picker.matches.len(),
            COMMON.len(),
            "a typo in the curated list would silently shorten it"
        );
        assert_eq!(picker.chosen(), Some("👍"));
    }

    #[test]
    fn searching_finds_by_name_and_by_shortcode() {
        let mut picker = Picker::new(Target::Composer);
        for c in "thumbsup".chars() {
            picker.push(c);
        }
        assert_eq!(picker.chosen(), Some("👍"));

        let mut picker = Picker::new(Target::Composer);
        for c in "rocket".chars() {
            picker.push(c);
        }
        assert_eq!(picker.chosen(), Some("🚀"));
    }

    #[test]
    fn an_exact_shortcode_wins_over_a_longer_name_containing_it() {
        // ":+1:" is the shortcode for 👍; several other names contain "+1" as a substring
        // and would otherwise be free to come first.
        let mut picker = Picker::new(Target::Composer);
        for c in "+1".chars() {
            picker.push(c);
        }
        assert_eq!(picker.chosen(), Some("👍"));
    }

    #[test]
    fn a_query_that_matches_nothing_chooses_nothing() {
        let mut picker = Picker::new(Target::Composer);
        for c in "notanemojiatall".chars() {
            picker.push(c);
        }
        assert!(picker.matches.is_empty());
        assert_eq!(picker.chosen(), None);
    }

    #[test]
    fn deleting_the_query_returns_to_the_starting_list() {
        let mut picker = Picker::new(Target::Composer);
        picker.push('r');
        picker.pop();
        assert_eq!(picker.query, "");
        assert_eq!(picker.chosen(), Some("👍"));
    }

    #[test]
    fn moving_the_selection_stops_at_both_ends() {
        let mut picker = Picker::new(Target::Composer);
        picker.up();
        assert_eq!(picker.selected, 0, "cannot move above the first match");

        for _ in 0..COMMON.len() * 2 {
            picker.down();
        }
        assert_eq!(picker.selected, COMMON.len() - 1, "nor past the last");
    }
}
