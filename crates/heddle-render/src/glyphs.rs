//! Every glyph heddle prints, and the width the renderer lays it out at.
//!
//! One table, because there were three and they disagreed. The transcript had a test
//! called `every_glyph_the_transcript_prints_is_measured_as_it_is_painted` that checked
//! four hardcoded emoji; the doctor had a `PRINTED_GLYPHS` list of five, one of which
//! (`❓`) the transcript explicitly refuses to draw; and `SPEC.md` claimed the client
//! printed nothing but East\_Asian\_Width=Wide glyphs. None of the three could catch a
//! new glyph being added, which is the only failure they existed to prevent.
//!
//! # Why the width matters
//!
//! `unicode-width` is what ratatui uses to decide how many cells a span occupies. If
//! the terminal paints a glyph wider than that, the extra cell is one the renderer
//! believes it has already written, so it is never cleared and the row rots as the
//! transcript scrolls under it.
//!
//! The hazard is not "narrow glyphs". It is *disagreement*. Two kinds of glyph are
//! safe:
//!
//! - `EAW=Wide` emoji. `unicode-width` says two cells and terminals paint two.
//! - Text-presentation symbols and box drawing (`EAW=Neutral` or `Ambiguous`).
//!   `unicode-width` says one cell and terminals paint one, outside a CJK locale.
//!
//! What is not safe is a `Neutral`/`Ambiguous` codepoint that terminals promote to
//! *emoji* presentation and paint at two cells while `unicode-width` still says one.
//! `⚠` U+26A0 is the canonical example, and it was the `Blocked` badge in every pane,
//! tab and workspace header -- while a comment in `transcript.rs` named it as a glyph
//! that "would rot the transcript". The comment was right; the usage was wrong.

/// A glyph heddle prints, what it means, and the width it is laid out at.
pub struct Glyph {
    pub glyph: &'static str,
    pub what: &'static str,
    /// Cells the renderer reserves. Always `UnicodeWidthStr::width(glyph)`; carried
    /// explicitly so the table states the assumption rather than restating the call.
    pub cells: usize,
}

const fn g(glyph: &'static str, what: &'static str, cells: usize) -> Glyph {
    Glyph { glyph, what, cells }
}

/// Every glyph heddle prints.
///
/// Add a glyph to the UI, add it here. `no_glyph_is_laid_out_at_a_width_it_is_not`
/// keeps the third column honest, and the doctor measures the whole table against the
/// user's actual terminal.
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
];

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn no_glyph_is_laid_out_at_a_width_it_is_not() {
        // The table's third column is what the renderer reserves. If it disagrees with
        // `unicode-width`, ratatui and the table are describing different screens.
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
        // The failure this whole module exists for: a codepoint `unicode-width` calls
        // one cell that the terminal paints as a two-cell emoji. These are the ones in
        // the ranges heddle draws from that have an Emoji_Presentation or a widely
        // implemented emoji fallback, and none of them may be used at width 1.
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
