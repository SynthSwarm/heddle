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

    /// Lay the buffer out at `width`, mapping the caret through the wrap.
    ///
    /// The caret has to be computed here rather than by the renderer: it is a byte
    /// offset into the buffer, and only the wrap knows which display row that offset
    /// ended up on.
    pub fn wrapped(&self, width: u16) -> Wrapped<'_> {
        let width = width.max(1) as usize;
        let mut lines = Vec::new();
        let mut caret = (0, 0);
        let mut placed = false;

        let mut base = 0;
        for logical in self.text.split('\n') {
            let segments = wrap_segments(logical, width);
            let last = segments.len() - 1;

            for (n, (from, to)) in segments.into_iter().enumerate() {
                let (start, end) = (base + from, base + to);
                let row = lines.len() as u16;
                lines.push(&self.text[start..end]);

                if placed || self.cursor < start {
                    continue;
                }
                // On a soft break the caret belongs at the start of the next row, so a
                // segment only claims an offset sitting exactly on its end when it is
                // the last of its logical line.
                if self.cursor < end || (self.cursor == end && n == last) {
                    let column = UnicodeWidthStr::width(&self.text[start..self.cursor]) as u16;
                    caret = (row, column);
                    placed = true;
                }
            }
            base += logical.len() + 1; // past the '\n'
        }

        Wrapped { lines, caret }
    }

    // ------------------------------------------------------------------- editing

    pub fn insert(&mut self, c: char) {
        self.stop_browsing();
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// The `@mention` being typed at the caret, if there is one.
    ///
    /// Returns the byte offset of the `@` and the text between it and the caret, so the
    /// caller can filter on the one and replace from the other.
    ///
    /// The `@` has to start a word. Without that rule an email address arms the picker
    /// halfway through being typed, and every `a@b` in a code snippet is a false start.
    /// A mention also cannot span a line break or contain a space, which is what makes
    /// [`Self::replace_mention`] and the send-time scan agree about where one ends.
    pub fn mention_query(&self) -> Option<(usize, &str)> {
        let before = &self.text[..self.cursor];
        // Everything back to the nearest whitespace: the run the caret sits inside.
        let start = before.rfind(char::is_whitespace).map_or(0, |i| i + 1);
        let run = &before[start..];
        let rest = run.strip_prefix('@')?;
        // A second `@` means this is no longer a name being typed.
        if rest.contains('@') {
            return None;
        }
        Some((start, rest))
    }

    /// Replace the mention starting at `from` with `text`, and leave the caret after it.
    ///
    /// A trailing space is the caller's business: it is part of what gets inserted, so
    /// that accepting a completion and carrying on typing does not need a second key.
    pub fn replace_mention(&mut self, from: usize, text: &str) {
        if from > self.cursor || !self.text.is_char_boundary(from) {
            return;
        }
        self.stop_browsing();
        self.text.replace_range(from..self.cursor, text);
        self.cursor = from + text.len();
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

    /// Move to the start of the previous word, stopping at the line start.
    pub fn word_left(&mut self) {
        self.cursor = self.word_start();
    }

    /// Move past the end of the next word, stopping at the line end.
    pub fn word_right(&mut self) {
        self.cursor = self.word_end();
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

    /// Offset just past the next word, without crossing a line break.
    fn word_end(&self) -> usize {
        let after = &self.text[self.cursor..];
        // Skip any run of spaces first, so the caret lands past a word rather than at
        // the front of the gap before it.
        let gap = after.len() - after.trim_start_matches([' ', '\t']).len();
        let rest = &after[gap..];
        let word = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
        self.cursor + gap + word
    }
}

/// Split one logical line into display rows no wider than `width`.
///
/// Returns byte ranges into `line`. Breaks after a space where there is one, and
/// mid-word only when a single word is wider than the pane, since the alternative is
/// text that never appears.
fn wrap_segments(line: &str, width: usize) -> Vec<(usize, usize)> {
    if line.is_empty() {
        // One empty row: a blank line still occupies a row and can hold the caret.
        return vec![(0, 0)];
    }

    let mut out = Vec::new();
    let mut start = 0;
    let mut used = 0;
    let mut last_break: Option<usize> = None;

    for (i, grapheme) in line.grapheme_indices(true) {
        let w = UnicodeWidthStr::width(grapheme);
        if used + w > width && i > start {
            let brk = last_break.filter(|b| *b > start).unwrap_or(i);
            out.push((start, brk));
            start = brk;
            used = UnicodeWidthStr::width(&line[start..i]) + w;
            last_break = None;
        } else {
            used += w;
        }
        if grapheme == " " {
            last_break = Some(i + grapheme.len());
        }
    }

    out.push((start, line.len()));
    out
}

/// The buffer laid out at a given width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wrapped<'a> {
    pub lines: Vec<&'a str>,
    /// Caret in wrapped coordinates: display row and display column.
    pub caret: (u16, u16),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    /// Caret at a width wide enough that nothing wraps, i.e. the logical position.
    fn caret(c: &Composer) -> (u16, u16) {
        c.wrapped(200).caret
    }

    fn rows(c: &Composer) -> usize {
        c.wrapped(200).lines.len()
    }

    fn typed(s: &str) -> Composer {
        let mut c = Composer::default();
        for ch in s.chars() {
            c.insert(ch);
        }
        c
    }

    #[test]
    fn a_mention_arms_at_the_start_of_a_word() {
        assert_eq!(typed("@qu").mention_query(), Some((0, "qu")));
        assert_eq!(typed("hi @qu").mention_query(), Some((3, "qu")));
    }

    #[test]
    fn a_bare_at_offers_everyone() {
        // The picker has to open on the sigil alone, or it never opens: nobody types a
        // name before deciding to mention someone.
        assert_eq!(typed("@").mention_query(), Some((0, "")));
    }

    #[test]
    fn an_email_address_does_not_arm_a_mention() {
        // The whole reason the sigil has to start a word.
        assert_eq!(typed("mail bob@example").mention_query(), None);
        assert_eq!(typed("@bob@example").mention_query(), None);
    }

    #[test]
    fn a_mention_ends_at_a_space() {
        assert_eq!(typed("@bob ").mention_query(), None);
    }

    #[test]
    fn a_mention_does_not_reach_across_a_line_break() {
        let mut c = typed("@bob");
        c.insert_newline();
        assert_eq!(c.mention_query(), None);
    }

    #[test]
    fn a_mention_is_read_from_the_caret_not_the_end() {
        // Someone who moved back to fix a name is still typing that name.
        let mut c = typed("@quintin and @wright");
        for _ in 0.." and @wright".len() {
            c.left();
        }
        assert_eq!(c.mention_query(), Some((0, "quintin")));
    }

    #[test]
    fn accepting_a_completion_replaces_only_the_mention() {
        let mut c = typed("hi @qu");
        let (from, _) = c.mention_query().expect("armed");
        c.replace_mention(from, "@quintin ");
        assert_eq!(c.text(), "hi @quintin ");
        // The caret follows the insertion, so typing carries straight on.
        c.insert('o');
        assert_eq!(c.text(), "hi @quintin o");
        // And the picker is disarmed by the trailing space rather than re-matching.
        assert_eq!(c.mention_query(), None);
    }

    #[test]
    fn accepting_a_completion_keeps_what_came_after_the_caret() {
        let mut c = typed("@qu, morning");
        for _ in 0..", morning".len() {
            c.left();
        }
        let (from, _) = c.mention_query().expect("armed");
        c.replace_mention(from, "@quintin");
        assert_eq!(c.text(), "@quintin, morning");
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
        assert_eq!(caret(&c), (0, 4));
    }

    #[test]
    fn moving_left_and_right_steps_over_clusters() {
        let mut c = typed("a\u{1F54A}\u{FE0F}b");
        c.home();
        c.right();
        c.right();
        // Past 'a' and the whole dove cluster.
        assert_eq!(
            caret(&c).1,
            UnicodeWidthStr::width("a\u{1F54A}\u{FE0F}") as u16
        );
        c.left();
        assert_eq!(caret(&c).1, 1);
    }

    #[test]
    fn newlines_grow_the_composer() {
        let mut c = typed("one");
        c.insert_newline();
        for ch in "two".chars() {
            c.insert(ch);
        }
        assert_eq!(rows(&c), 2);
        assert_eq!(caret(&c), (1, 3));
    }

    #[test]
    fn vertical_movement_keeps_the_column_and_reports_the_edges() {
        let mut c = typed("first\nsecond");
        assert_eq!(caret(&c), (1, 6));

        assert!(c.up(), "there is a line above");
        assert_eq!(caret(&c), (0, 5), "clamped to the shorter line");

        assert!(!c.up(), "already on the first line");
        assert!(c.down());
        assert!(!c.down(), "already on the last line");
    }

    #[test]
    fn home_and_end_work_per_line() {
        let mut c = typed("first\nsecond");
        c.home();
        assert_eq!(caret(&c), (1, 0));
        c.end();
        assert_eq!(caret(&c), (1, 6));
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

    #[test]
    fn word_movement_stops_at_line_ends() {
        let mut c = typed("alpha beta gamma");
        c.word_left();
        assert_eq!(c.text()[..c.cursor].to_owned(), "alpha beta ");
        c.word_left();
        assert_eq!(c.text()[..c.cursor].to_owned(), "alpha ");
        c.word_left();
        assert_eq!(c.cursor, 0);
        c.word_left();
        assert_eq!(c.cursor, 0, "already at the start");

        c.word_right();
        assert_eq!(c.text()[..c.cursor].to_owned(), "alpha");
        c.word_right();
        assert_eq!(c.text()[..c.cursor].to_owned(), "alpha beta");
        c.word_right();
        c.word_right();
        assert_eq!(c.cursor, c.text().len(), "already at the end");
    }

    #[test]
    fn word_movement_does_not_cross_a_newline() {
        let mut c = typed("one two\nthree four");
        c.home();
        assert_eq!(caret(&c), (1, 0));
        c.word_left();
        assert_eq!(caret(&c), (1, 0), "must not jump up to the previous line");

        c.end();
        c.word_right();
        assert_eq!(caret(&c).0, 1, "must not fall through to the next line");
    }

    #[test]
    fn wrapping_breaks_at_spaces() {
        let c = typed("the quick brown fox");
        let w = c.wrapped(10);
        assert_eq!(w.lines, vec!["the quick ", "brown fox"]);
    }

    #[test]
    fn a_word_longer_than_the_pane_is_broken_rather_than_hidden() {
        let c = typed("supercalifragilistic");
        let w = c.wrapped(8);
        assert_eq!(w.lines, vec!["supercal", "ifragili", "stic"]);
    }

    #[test]
    fn wrapping_keeps_hard_line_breaks() {
        let c = typed("short\nalso short");
        let w = c.wrapped(40);
        assert_eq!(w.lines, vec!["short", "also short"]);
    }

    #[test]
    fn a_blank_line_still_occupies_a_row() {
        let c = typed("a\n\nb");
        let w = c.wrapped(10);
        assert_eq!(w.lines, vec!["a", "", "b"]);
    }

    #[test]
    fn the_caret_follows_the_text_across_a_soft_break() {
        let mut c = typed("the quick brown fox");
        // Caret at the very end: last row, after "brown fox".
        assert_eq!(c.wrapped(10).caret, (1, 9));

        c.home();
        assert_eq!(c.wrapped(10).caret, (0, 0), "home goes to the logical line");

        // Sitting exactly on a soft break belongs at the start of the next row, not
        // hanging off the end of the previous one.
        for _ in 0..10 {
            c.right();
        }
        assert_eq!(c.wrapped(10).caret, (1, 0));
    }

    #[test]
    fn the_caret_accounts_for_wide_glyphs_when_wrapped() {
        let c = typed("\u{2705}\u{2705}\u{2705}");
        // Three two-cell glyphs at width 5: two fit, the third wraps.
        let w = c.wrapped(5);
        assert_eq!(w.lines.len(), 2);
        assert_eq!(w.caret, (1, 2), "column is cells, not characters");
    }

    #[test]
    fn wrapping_a_trailing_newline_leaves_an_empty_last_row() {
        let mut c = typed("done");
        c.insert_newline();
        let w = c.wrapped(10);
        assert_eq!(w.lines, vec!["done", ""]);
        assert_eq!(w.caret, (1, 0));
    }
}
