//! The message composer.
//!
//! A text buffer with a caret, per-instance history, and enough editing to hold a
//! conversation. One composer per view, so a half-written message survives switching
//! room and coming back.
//!
//! Editing is grapheme-based rather than `char`-based throughout. `String::pop` would
//! delete the variation selector off `🕊️` and leave a broken cluster on screen, and
//! flag and skin-tone sequences are several `char`s each.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// How many sent messages to remember per view.
const HISTORY_LIMIT: usize = 100;

/// A text buffer with a caret.
#[derive(Debug, Clone, Default)]
pub struct Composer {
    text: String,
    /// Caret position, as a byte offset into `text`. Always on a grapheme boundary.
    cursor: usize,
    /// Previously sent messages, oldest first.
    history: Vec<String>,
    /// Position while browsing history. `None` means editing live text.
    browsing: Option<usize>,
    /// Live text, parked while browsing history so it can be restored.
    stashed: Option<String>,
}

impl Composer {
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Replace the whole buffer, putting the caret at the end.
    ///
    /// Used when an edit loads an existing message in for revision.
    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_owned();
        self.cursor = self.text.len();
        self.browsing = None;
        self.stashed = None;
    }

    /// Caret position as (row, column), both measured for display.
    ///
    /// The column is a display width, not a character count, so the caret lands in the
    /// right cell after a wide glyph.
    pub fn caret(&self) -> (u16, u16) {
        let before = &self.text[..self.cursor];
        let row = before.matches('\n').count() as u16;
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        let column = UnicodeWidthStr::width(&before[line_start..]) as u16;
        (row, column)
    }

    /// Number of lines, at least one.
    pub fn line_count(&self) -> u16 {
        (self.text.matches('\n').count() + 1) as u16
    }

    // ------------------------------------------------------------------- editing

    pub fn insert(&mut self, c: char) {
        self.stop_browsing();
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_newline(&mut self) {
        self.insert('\n');
    }

    /// Delete the grapheme before the caret.
    pub fn backspace(&mut self) {
        self.stop_browsing();
        let Some(start) = self.prev_boundary(self.cursor) else {
            return;
        };
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Delete the grapheme at the caret.
    pub fn delete(&mut self) {
        self.stop_browsing();
        let Some(end) = self.next_boundary(self.cursor) else {
            return;
        };
        self.text.replace_range(self.cursor..end, "");
    }

    /// Delete the word before the caret, and any whitespace attached to it.
    pub fn delete_word(&mut self) {
        self.stop_browsing();
        let start = self.word_start();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Delete from the start of the current line to the caret.
    pub fn delete_to_line_start(&mut self) {
        self.stop_browsing();
        let start = self.line_start();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Take the text for sending, recording it in history and clearing the buffer.
    ///
    /// Returns `None` when there is nothing but whitespace to send.
    pub fn take(&mut self) -> Option<String> {
        let body = self.text.trim().to_owned();
        self.text.clear();
        self.cursor = 0;
        self.stashed = None;
        self.browsing = None;
        if body.is_empty() {
            return None;
        }
        // Don't record an immediate repeat; holding enter on the same message should
        // not fill the history with it.
        if self.history.last() != Some(&body) {
            self.history.push(body.clone());
            if self.history.len() > HISTORY_LIMIT {
                self.history.remove(0);
            }
        }
        Some(body)
    }

    // ------------------------------------------------------------------ movement

    pub fn left(&mut self) {
        if let Some(i) = self.prev_boundary(self.cursor) {
            self.cursor = i;
        }
    }

    pub fn right(&mut self) {
        if let Some(i) = self.next_boundary(self.cursor) {
            self.cursor = i;
        }
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    /// Move up a line, keeping the display column where possible.
    ///
    /// Returns `false` when already on the first line, so the caller can treat the
    /// keypress as a history request instead.
    pub fn up(&mut self) -> bool {
        let start = self.line_start();
        if start == 0 {
            return false;
        }
        let column = self.column();
        let previous_start = self.text[..start - 1].rfind('\n').map_or(0, |i| i + 1);
        self.cursor =
            Self::offset_for_column(&self.text[previous_start..start - 1], column) + previous_start;
        true
    }

    /// Move down a line, keeping the display column where possible.
    ///
    /// Returns `false` when already on the last line.
    pub fn down(&mut self) -> bool {
        let end = self.line_end();
        if end == self.text.len() {
            return false;
        }
        let column = self.column();
        let next_start = end + 1;
        let next_end = self.text[next_start..]
            .find('\n')
            .map_or(self.text.len(), |i| next_start + i);
        self.cursor =
            Self::offset_for_column(&self.text[next_start..next_end], column) + next_start;
        true
    }

    // ------------------------------------------------------------------- history

    /// Step back through sent messages. Returns `false` when there is no older entry.
    pub fn history_prev(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let next = match self.browsing {
            None => {
                // Park whatever is being written so it comes back on the way down.
                self.stashed = Some(self.text.clone());
                self.history.len() - 1
            }
            Some(0) => return false,
            Some(i) => i - 1,
        };
        self.browsing = Some(next);
        self.text.clone_from(&self.history[next]);
        self.cursor = self.text.len();
        true
    }

    /// Step forward through sent messages, ending back at the parked draft.
    pub fn history_next(&mut self) -> bool {
        let Some(i) = self.browsing else {
            return false;
        };
        if i + 1 < self.history.len() {
            self.browsing = Some(i + 1);
            self.text.clone_from(&self.history[i + 1]);
        } else {
            self.browsing = None;
            self.text = self.stashed.take().unwrap_or_default();
        }
        self.cursor = self.text.len();
        true
    }

    /// Editing a recalled message detaches it from the history entry.
    fn stop_browsing(&mut self) {
        if self.browsing.take().is_some() {
            self.stashed = None;
        }
    }

    // ------------------------------------------------------------------ internals

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i)
    }

    /// Display column of the caret within its line.
    fn column(&self) -> usize {
        UnicodeWidthStr::width(&self.text[self.line_start()..self.cursor])
    }

    /// The byte offset in `line` closest to `column` without exceeding it.
    fn offset_for_column(line: &str, column: usize) -> usize {
        let mut width = 0;
        for (offset, grapheme) in line.grapheme_indices(true) {
            let next = width + UnicodeWidthStr::width(grapheme);
            if next > column {
                return offset;
            }
            width = next;
        }
        line.len()
    }

    fn prev_boundary(&self, at: usize) -> Option<usize> {
        self.text[..at]
            .grapheme_indices(true)
            .next_back()
            .map(|(i, _)| i)
    }

    fn next_boundary(&self, at: usize) -> Option<usize> {
        self.text[at..]
            .grapheme_indices(true)
            .next()
            .map(|(_, g)| at + g.len())
    }

    fn word_start(&self) -> usize {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end_matches(|c: char| c.is_whitespace() && c != '\n');
        match trimmed.rfind(|c: char| c.is_whitespace()) {
            Some(i) => i + 1,
            None => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    fn typed(s: &str) -> Composer {
        let mut c = Composer::default();
        for ch in s.chars() {
            c.insert(ch);
        }
        c
    }

    #[test]
    fn typing_and_sending_round_trips() {
        let mut c = typed("  hello  ");
        assert_eq!(c.take().as_deref(), Some("hello"));
        assert!(c.text().is_empty());
    }

    #[test]
    fn whitespace_alone_sends_nothing() {
        let mut c = typed("   \n  ");
        assert_eq!(c.take(), None);
    }

    #[test]
    fn backspace_deletes_a_grapheme_not_a_char() {
        // The dove is U+1F54A plus U+FE0F. Deleting one char would leave a bare dove
        // with a different width, which is exactly the mismatch that breaks layout.
        let mut c = typed("hi \u{1F54A}\u{FE0F}");
        c.backspace();
        assert_eq!(
            c.text(),
            "hi ",
            "one backspace must remove the whole cluster"
        );
    }

    #[test]
    fn the_caret_is_measured_in_display_columns() {
        // Two wide glyphs then a caret: column 4, not 2.
        let c = typed("\u{2705}\u{2705}");
        assert_eq!(c.caret(), (0, 4));
    }

    #[test]
    fn moving_left_and_right_steps_over_clusters() {
        let mut c = typed("a\u{1F54A}\u{FE0F}b");
        c.home();
        c.right();
        c.right();
        // Past 'a' and the whole dove cluster.
        assert_eq!(
            c.caret().1,
            UnicodeWidthStr::width("a\u{1F54A}\u{FE0F}") as u16
        );
        c.left();
        assert_eq!(c.caret().1, 1);
    }

    #[test]
    fn newlines_grow_the_composer() {
        let mut c = typed("one");
        c.insert_newline();
        for ch in "two".chars() {
            c.insert(ch);
        }
        assert_eq!(c.line_count(), 2);
        assert_eq!(c.caret(), (1, 3));
    }

    #[test]
    fn vertical_movement_keeps_the_column_and_reports_the_edges() {
        let mut c = typed("first\nsecond");
        assert_eq!(c.caret(), (1, 6));

        assert!(c.up(), "there is a line above");
        assert_eq!(c.caret(), (0, 5), "clamped to the shorter line");

        assert!(!c.up(), "already on the first line");
        assert!(c.down());
        assert!(!c.down(), "already on the last line");
    }

    #[test]
    fn home_and_end_work_per_line() {
        let mut c = typed("first\nsecond");
        c.home();
        assert_eq!(c.caret(), (1, 0));
        c.end();
        assert_eq!(c.caret(), (1, 6));
    }

    #[test]
    fn deleting_a_word_takes_its_trailing_space() {
        let mut c = typed("one two three");
        c.delete_word();
        assert_eq!(c.text(), "one two ");
        c.delete_word();
        assert_eq!(c.text(), "one ");
    }

    #[test]
    fn deleting_to_line_start_leaves_earlier_lines_alone() {
        let mut c = typed("keep\ndrop this");
        c.delete_to_line_start();
        assert_eq!(c.text(), "keep\n");
    }

    #[test]
    fn history_walks_back_and_returns_the_parked_draft() {
        let mut c = typed("first");
        c.take();
        for ch in "second".chars() {
            c.insert(ch);
        }
        c.take();
        for ch in "draft".chars() {
            c.insert(ch);
        }

        assert!(c.history_prev());
        assert_eq!(c.text(), "second");
        assert!(c.history_prev());
        assert_eq!(c.text(), "first");
        assert!(!c.history_prev(), "nothing older");

        assert!(c.history_next());
        assert_eq!(c.text(), "second");
        assert!(c.history_next());
        assert_eq!(c.text(), "draft", "the parked draft comes back");
    }

    #[test]
    fn editing_a_recalled_message_detaches_it() {
        let mut c = typed("original");
        c.take();
        c.history_prev();
        c.insert('!');
        assert_eq!(c.text(), "original!");
        // No longer browsing, so going forward does nothing rather than reverting.
        assert!(!c.history_next());
        assert_eq!(c.text(), "original!");
    }

    #[test]
    fn history_does_not_record_immediate_repeats() {
        let mut c = typed("same");
        c.take();
        for ch in "same".chars() {
            c.insert(ch);
        }
        c.take();
        assert!(c.history_prev());
        assert_eq!(c.text(), "same");
        assert!(!c.history_prev(), "the repeat must not be a second entry");
    }

    #[test]
    fn an_empty_history_is_not_browsable() {
        let mut c = typed("draft");
        assert!(!c.history_prev());
        assert_eq!(c.text(), "draft", "the draft must survive a failed recall");
    }
}
