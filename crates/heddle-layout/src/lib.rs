//! Workspace model and tiling for heddle.
//!
//! [`model`] holds the Space -> Room -> Thread hierarchy heddle presents as
//! workspaces, tabs and panes. [`tiling`] is a thin facade over the BSP engine, kept
//! deliberately small so the engine can be swapped.

pub mod model;
pub mod tiling;

pub use model::{Pane, PaneKind, Tab, Workspace, Workspaces, ORPHAN_WORKSPACE};
pub use ratatui_hypertile::PaneId;
pub use tiling::{Dir, Placement, Tiling};
