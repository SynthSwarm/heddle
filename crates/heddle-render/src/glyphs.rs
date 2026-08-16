//! Every glyph heddle prints, and the width the renderer lays it out at.
//!
//! One table, so a glyph cannot reach the UI without the doctor learning to probe it.
//!
//! `unicode-width` decides how many cells ratatui reserves. A terminal that paints a
//! glyph wider leaves a cell the renderer believes it has written, so the row rots as
//! the transcript scrolls under it.
//!
//! The hazard is *disagreement*, not narrowness: `EAW=Wide` emoji measure two and paint
//! two, and text-presentation symbols measure one and paint one. Unsafe is a
//! `Neutral`/`Ambiguous` codepoint that terminals promote to emoji presentation, `⚠`
//! U+26A0 being the usual one.

/// A glyph heddle prints, what it means, and the width it is laid out at.
pub struct Glyph {
    pub glyph: &'static str,
    pub what: &'static str,
    /// Cells the renderer reserves. Always `UnicodeWidthStr::width(glyph)`, stated
    /// here so the assumption is checkable.
    pub cells: usize,
}

const fn g(glyph: &'static str, what: &'static str, cells: usize) -> Glyph {
    Glyph { glyph, what, cells }
}

/// Every glyph heddle prints.
///
/// Add a glyph to the UI, add it here: the doctor can only measure what it is given.
pub const PRINTED: &[Glyph] = &[
    // Wide: two cells, and terminals agree.
    g("\u{1F464}", "human sender", 2),
    g("\u{1F916}", "agent sender", 2),
    g("\u{26D4}", "unverified shield", 2),
    g("\u{1F512}", "encrypted room", 2),
    // Text presentation: one cell, and terminals agree.
    g("\u{00B7}", "idle badge", 1),
    g("\u{2713}", "done badge, tool ok", 1),
    g("\u{25CF}", "working badge", 1),
    g("\u{25B2}", "blocked badge", 1),
    g("\u{25D0}", "tool running", 1),
    g("\u{2717}", "tool error", 1),
    g("\u{25BC}", "disclosure open", 1),
    g("\u{25B8}", "disclosure closed", 1),
    g("\u{258E}", "selection mark", 1),
    g("\u{2937}", "thread arrow", 1),
    g("\u{250A}", "commentary gutter", 1),
    g("\u{2500}", "rule", 1),
    g("\u{2026}", "ellipsis", 1),
    g("~", "degraded marker", 1),
    g("!", "gap marker", 1),
];

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn no_glyph_is_laid_out_at_a_width_it_is_not() {
        // If the third column disagrees with `unicode-width`, the table and ratatui
        // are describing different screens.
        for Glyph { glyph, what, cells } in PRINTED {
            assert_eq!(
                UnicodeWidthStr::width(*glyph),
                *cells,
                "{glyph} ({what}) is laid out at {cells} but measures {}",
                UnicodeWidthStr::width(*glyph)
            );
        }
    }

    #[test]
    fn no_glyph_is_one_a_terminal_is_likely_to_paint_as_an_emoji() {
        // A codepoint `unicode-width` calls one cell and the terminal paints as a
        // two-cell emoji. These are the ones in the ranges heddle draws from.
        const EMOJI_PRONE: &[char] = &[
            '\u{26A0}', // ⚠ warning
            '\u{2757}', // ❗
            '\u{2753}', // ❓
            '\u{203C}', // ‼
            '\u{2049}', // ⁉
            '\u{26D4}', // ⛔ -- fine at width 2, listed to prove the test discriminates
            '\u{23F8}', // ⏸
            '\u{2B55}', // ⭕
        ];

        for Glyph { glyph, what, cells } in PRINTED {
            if *cells != 1 {
                continue;
            }
            for c in glyph.chars() {
                assert!(
                    !EMOJI_PRONE.contains(&c),
                    "{glyph} ({what}) is laid out as one cell but terminals commonly \
                     paint U+{:04X} as a two-cell emoji",
                    c as u32
                );
            }
        }
    }
}
