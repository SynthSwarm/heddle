//! Saving and restoring the pane arrangement.
//!
//! A layout is *not* data: every room, thread and message in it comes back from the
//! homeserver on the next sync. What cannot be recovered is the arrangement the user
//! built by hand — which threads they had side by side, how wide they made them, which
//! one they were looking at. That is what this file persists, and nothing else.
//!
//! Cheap to rebuild, so cheap to throw away: every failure path here degrades to one
//! pane per room rather than to an error blocking the user from their messages.
//!
//! See `docs/SPEC.md` §4.1.

use crate::model::{Pane, PaneKind, Tab, Workspaces};
use crate::tiling::{Tiling, TilingSnapshot};
use ratatui_hypertile::PaneId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Schema version of the layout file.
///
/// A file from a different version is discarded, not migrated: the cost of a wrong
/// migration is someone's arrangement silently rearranged, and the cost of discarding is
/// one relaunch spent re-splitting.
pub const VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("writing {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("serialising the layout: {0}")]
    Encode(#[source] serde_json::Error),
}

/// One pane's content. The geometry lives in the tiling snapshot beside it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedPane {
    /// Pane id, matching a leaf of the tiling tree.
    pub id: u64,
    pub kind: PaneKind,
    /// Cached only so a restored pane has a header before its timeline arrives.
    pub title: String,
}

/// One room's pane arrangement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedTab {
    pub tiling: TilingSnapshot,
    /// In the order the tab held them, which is the order they were created in. Pane
    /// order is not cosmetic: it decides which pane a new thread splits off.
    pub panes: Vec<SavedPane>,
}

/// The whole persisted layout for one profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layout {
    pub version: u32,
    /// Workspace the user was in, by Space id or [`crate::ORPHAN_WORKSPACE`].
    pub workspace: Option<String>,
    /// The focused tab of each workspace: workspace id to room id. Kept per workspace
    /// rather than only for the focused one, so cycling back to a workspace returns to
    /// the room you left it on.
    pub tabs: BTreeMap<String, String>,
    /// Pane arrangement per room.
    pub rooms: BTreeMap<String, SavedTab>,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            version: VERSION,
            workspace: None,
            tabs: BTreeMap::new(),
            rooms: BTreeMap::new(),
        }
    }
}

impl Layout {
    /// Capture the current arrangement.
    ///
    /// Single-pane rooms are recorded too, so "never opened" stays distinguishable from
    /// "deliberately left with one pane".
    pub fn capture(workspaces: &Workspaces, tilings: &HashMap<String, Tiling>) -> Self {
        let mut tabs = BTreeMap::new();
        let mut rooms = BTreeMap::new();

        for workspace in &workspaces.items {
            if let Some(tab) = workspace.focused_tab() {
                tabs.insert(workspace.id.clone(), tab.room_id.clone());
            }
            for tab in &workspace.tabs {
                let Some(tiling) = tilings.get(&tab.room_id) else {
                    continue;
                };
                rooms.insert(
                    tab.room_id.clone(),
                    SavedTab {
                        tiling: tiling.snapshot(),
                        panes: tab
                            .panes
                            .iter()
                            .map(|pane| SavedPane {
                                id: pane.id.get(),
                                kind: pane.kind.clone(),
                                title: pane.title.clone(),
                            })
                            .collect(),
                    },
                );
            }
        }

        Self {
            version: VERSION,
            workspace: workspaces.focused().map(|w| w.id.clone()),
            tabs,
            rooms,
        }
    }

    /// Take a room's saved arrangement, rebuilding its tiling and panes.
    ///
    /// Consuming: sync re-sends summaries, so a room appears in the room list more than
    /// once and must be restored only the first time.
    ///
    /// `None` when nothing is saved, the tree will not load, or the panes and the tree
    /// disagree about which panes exist. The caller then starts a fresh single-pane
    /// tab.
    pub fn take_room(&mut self, room_id: &str, tab: &mut Tab) -> Option<Tiling> {
        let saved = self.rooms.remove(room_id)?;
        if saved.panes.is_empty() {
            return None;
        }

        let tiling = match Tiling::restore(&saved.tiling) {
            Ok(tiling) => tiling,
            Err(error) => {
                tracing::warn!(room = %room_id, ?error, "discarding an unloadable saved layout");
                return None;
            }
        };

        // A pane that is not a leaf would be drawn nowhere; a leaf with no pane would
        // be drawn empty for ever.
        let leaves = tiling.pane_ids();
        if leaves.len() != saved.panes.len()
            || !saved
                .panes
                .iter()
                .all(|p| leaves.contains(&PaneId::new(p.id)))
        {
            tracing::warn!(
                room = %room_id,
                leaves = leaves.len(),
                panes = saved.panes.len(),
                "discarding a saved layout whose panes and tree disagree"
            );
            return None;
        }

        for pane in saved.panes {
            tab.push_pane(Pane::new(PaneId::new(pane.id), pane.kind, pane.title));
        }
        if let Some(focused) = tiling.focused() {
            tab.focus(focused);
        }
        Some(tiling)
    }

    /// Load from `path`, or return an empty layout when there is nothing usable there.
    ///
    /// A missing file is the ordinary first run. A corrupt or stale one is logged and
    /// ignored.
    pub fn load(path: &Path) -> Self {
        match Self::read(path) {
            Ok(layout) => layout,
            Err(error) => {
                tracing::warn!(?error, "ignoring the saved layout");
                Self::default()
            }
        }
    }

    fn read(path: &Path) -> Result<Self, LayoutError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(LayoutError::Read {
                    path: path.to_path_buf(),
                    source,
                })
            }
        };

        let layout: Self = serde_json::from_str(&text).map_err(|source| LayoutError::Parse {
            path: path.to_path_buf(),
            source,
        })?;

        if layout.version != VERSION {
            tracing::info!(
                found = layout.version,
                expected = VERSION,
                "layout file is from another version; starting fresh"
            );
            return Ok(Self::default());
        }
        Ok(layout)
    }

    /// Write to `path`, creating parent directories.
    ///
    /// Via a temporary file and a rename, so an interrupted write cannot leave a
    /// half-written file that the next launch would then reject.
    pub fn save(&self, path: &Path) -> Result<(), LayoutError> {
        let json = serde_json::to_string_pretty(self).map_err(LayoutError::Encode)?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| LayoutError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json).map_err(|source| LayoutError::Write {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, path).map_err(|source| LayoutError::Write {
            path: path.to_path_buf(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use crate::model::Workspace;
    use crate::tiling::Dir;
    use ratatui::layout::Rect;
    use tempfile::TempDir;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 24,
    };

    /// A workspace holding one room with a room pane and two thread panes, arranged the
    /// way `open_thread_pane` arranges them.
    fn arranged() -> (Workspaces, HashMap<String, Tiling>) {
        let mut tiling = Tiling::new();
        let placements = tiling.layout(AREA);
        let root = placements[0].id;

        let mut tab = Tab::new("!room:x", "#backend");
        tab.push_pane(Pane::new(
            root,
            PaneKind::Room {
                room_id: "!room:x".into(),
            },
            "#backend",
        ));

        for root_event in ["$one", "$two"] {
            let id = tiling.split(Dir::Right).expect("splits");
            tab.push_pane(Pane::new(
                id,
                PaneKind::Thread {
                    room_id: "!room:x".into(),
                    root: root_event.into(),
                },
                root_event,
            ));
        }
        tiling.layout(AREA);

        let mut workspaces = Workspaces::new();
        workspaces.entry("!space:x", "hermes").tabs.push(tab);

        let mut tilings = HashMap::new();
        tilings.insert("!room:x".to_owned(), tiling);
        (workspaces, tilings)
    }

    #[test]
    fn a_captured_layout_comes_back_pane_for_pane() {
        let (workspaces, mut tilings) = arranged();
        let before = tilings.get_mut("!room:x").expect("tiling").layout(AREA);

        let json = serde_json::to_string(&Layout::capture(&workspaces, &tilings)).expect("encodes");
        let mut layout: Layout = serde_json::from_str(&json).expect("decodes");

        let mut tab = Tab::new("!room:x", "#backend");
        let mut restored = layout.take_room("!room:x", &mut tab).expect("restores");

        assert_eq!(tab.panes.len(), 3);
        assert_eq!(
            tab.panes.iter().map(|p| p.kind.clone()).collect::<Vec<_>>(),
            workspaces.items[0].tabs[0]
                .panes
                .iter()
                .map(|p| p.kind.clone())
                .collect::<Vec<_>>(),
            "pane order decides where the next thread splits, so it must survive"
        );
        assert_eq!(
            restored.layout(AREA),
            before,
            "geometry must come back identical, not merely similar"
        );
    }

    #[test]
    fn a_resized_border_survives_the_round_trip() {
        // The bug this guards: ratios used to be shadowed in a side table that started
        // empty on restore, so the first resize after a relaunch snapped the border
        // back to the middle.
        let (workspaces, mut tilings) = arranged();
        let tiling = tilings.get_mut("!room:x").expect("tiling");
        tiling.resize(Dir::Left, 0.2);
        let before = tiling.layout(AREA);

        let mut layout = Layout::capture(&workspaces, &tilings);
        let mut tab = Tab::new("!room:x", "#backend");
        let mut restored = layout.take_room("!room:x", &mut tab).expect("restores");
        assert_eq!(restored.layout(AREA), before);

        // And a further resize continues from where it was rather than from 0.5.
        restored.resize(Dir::Left, 0.2);
        assert_ne!(restored.layout(AREA), before);
    }

    #[test]
    fn focus_and_zoom_come_back() {
        let (workspaces, mut tilings) = arranged();
        let tiling = tilings.get_mut("!room:x").expect("tiling");
        tiling.focus_dir(Dir::Left);
        let focused = tiling.focused().expect("focused");
        tiling.toggle_zoom();

        let mut layout = Layout::capture(&workspaces, &tilings);
        let mut tab = Tab::new("!room:x", "#backend");
        let restored = layout.take_room("!room:x", &mut tab).expect("restores");

        assert_eq!(restored.focused(), Some(focused));
        assert!(restored.is_zoomed());
        assert_eq!(tab.focused_pane().expect("pane").id, focused);
    }

    #[test]
    fn the_focused_workspace_and_its_tab_are_recorded() {
        let (mut workspaces, tilings) = arranged();
        workspaces
            .entry("!other:x", "other")
            .tabs
            .push(Tab::new("!elsewhere:x", "elsewhere"));

        let layout = Layout::capture(&workspaces, &tilings);
        assert_eq!(layout.workspace.as_deref(), Some("!space:x"));
        assert_eq!(layout.tabs["!space:x"], "!room:x");
        assert_eq!(
            layout.tabs["!other:x"], "!elsewhere:x",
            "an unfocused workspace still remembers where you left it"
        );
    }

    #[test]
    fn a_room_is_only_restored_once() {
        let (workspaces, tilings) = arranged();
        let mut layout = Layout::capture(&workspaces, &tilings);

        let mut first = Tab::new("!room:x", "#backend");
        assert!(layout.take_room("!room:x", &mut first).is_some());

        // Sync re-sends room summaries. A second restore would stack a second copy of
        // every pane onto the tab.
        let mut second = Tab::new("!room:x", "#backend");
        assert!(layout.take_room("!room:x", &mut second).is_none());
        assert!(second.panes.is_empty());
    }

    #[test]
    fn a_layout_whose_panes_and_tree_disagree_is_refused() {
        let (workspaces, tilings) = arranged();
        let mut layout = Layout::capture(&workspaces, &tilings);
        layout
            .rooms
            .get_mut("!room:x")
            .expect("room")
            .panes
            .pop()
            .expect("a pane to drop");

        let mut tab = Tab::new("!room:x", "#backend");
        assert!(
            layout.take_room("!room:x", &mut tab).is_none(),
            "a leaf with no pane would be drawn empty for ever"
        );
        assert!(
            tab.panes.is_empty(),
            "a refused restore must leave no trace"
        );
    }

    #[test]
    fn nothing_saved_for_a_room_is_not_an_error() {
        let mut layout = Layout::default();
        let mut tab = Tab::new("!new:x", "new");
        assert!(layout.take_room("!new:x", &mut tab).is_none());
    }

    /// A layout path in a directory that deletes itself. Returned together, because
    /// dropping the guard removes the file.
    fn scratch() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("scratch dir");
        let path = dir.path().join("layout.json");
        (dir, path)
    }

    #[test]
    fn a_missing_file_is_the_ordinary_first_run() {
        let (_scratch, path) = scratch();
        let _ = std::fs::remove_file(&path);
        assert!(Layout::load(&path).rooms.is_empty());
    }

    #[test]
    fn a_layout_survives_a_trip_through_the_filesystem() {
        let (_scratch, path) = scratch();
        let (workspaces, tilings) = arranged();
        Layout::capture(&workspaces, &tilings)
            .save(&path)
            .expect("saves");

        let mut loaded = Layout::load(&path);
        assert_eq!(loaded.workspace.as_deref(), Some("!space:x"));
        let mut tab = Tab::new("!room:x", "#backend");
        assert!(loaded.take_room("!room:x", &mut tab).is_some());
        assert_eq!(tab.panes.len(), 3);
    }

    #[test]
    fn a_corrupt_file_costs_a_layout_not_a_launch() {
        let (_scratch, path) = scratch();
        std::fs::write(&path, "{ this is not json").expect("seed");
        assert!(Layout::load(&path).rooms.is_empty());
    }

    #[test]
    fn a_file_from_another_version_is_discarded_not_guessed_at() {
        let (_scratch, path) = scratch();
        let (workspaces, tilings) = arranged();
        let mut layout = Layout::capture(&workspaces, &tilings);
        layout.version = VERSION + 1;
        layout.save(&path).expect("saves");

        assert!(
            Layout::load(&path).rooms.is_empty(),
            "guessing at an unknown schema risks rearranging panes silently"
        );
    }

    #[test]
    fn saving_does_not_leave_a_temporary_file_behind() {
        let (_scratch, path) = scratch();
        let (workspaces, tilings) = arranged();
        Layout::capture(&workspaces, &tilings)
            .save(&path)
            .expect("saves");
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn an_empty_workspace_set_captures_cleanly() {
        let layout = Layout::capture(&Workspaces::new(), &HashMap::new());
        assert_eq!(layout.version, VERSION);
        assert!(layout.workspace.is_none());
        assert!(layout.rooms.is_empty());
    }

    #[test]
    fn a_workspace_with_no_tiling_is_skipped_rather_than_half_saved() {
        let mut workspaces = Workspaces::new();
        let w: &mut Workspace = workspaces.entry("!s:x", "s");
        w.tabs.push(Tab::new("!untiled:x", "untiled"));

        let layout = Layout::capture(&workspaces, &HashMap::new());
        assert!(layout.rooms.is_empty());
        assert_eq!(layout.tabs["!s:x"], "!untiled:x");
    }
}
