//! Rendering for heddle.
//!
//! Pure functions from data to [`ratatui::text::Line`]s. No terminal, no I/O, no
//! widgets that own state — which is what makes the interesting behaviour (card
//! folding, diff stats, degradation markers) testable without a terminal.

pub mod card;
pub mod diff;
pub mod glyphs;
pub mod theme;
pub mod transcript;

pub use card::AutoExpand;
pub use glyphs::{Glyph, PRINTED};
pub use theme::Theme;
pub use transcript::{Options, Overrides};
