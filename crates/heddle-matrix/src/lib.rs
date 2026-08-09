//! Matrix transport for heddle.
//!
//! The [`worker`] owns every SDK handle and communicates with the render thread over a
//! pair of channels carrying the plain types in [`model`]. Agent decoding happens here,
//! on the worker, so that parsing never runs during a frame.
//!
//! Pre-flight note: this crate requires the homeserver to support native sliding sync
//! (MSC4186, advertised as `org.matrix.simplified_msc3575`). [`matrix_sdk_ui`]'s
//! `RoomListService` has no `/sync` fallback. See [`check_homeserver`] and
//! `docs/PLAN.md` M0.

// The matrix-sdk async state machines nest deeply enough to exceed the default limit
// when combined with our own futures.
#![recursion_limit = "512"]

pub mod model;
pub mod session;
pub mod worker;

pub use model::{
    AgentPayload, Command, Entry, EntryKind, Message, RoomSummary, SyncState, ThreadSummary, View,
    WorkerEvent,
};
pub use session::{Paths, SavedSession, SessionError};
pub use worker::{spawn, Handle};

/// Check that a homeserver advertises everything heddle needs.
///
/// Run by `heddle --check`. Sliding sync is the hard requirement; threads and
/// cross-signing being absent only degrade features.
pub async fn check_homeserver(homeserver: &str) -> anyhow::Result<Capabilities> {
    let url = format!(
        "{}/_matrix/client/versions",
        homeserver.trim_end_matches('/')
    );
    let body: serde_json::Value = reqwest_get_json(&url).await?;

    let feature = |name: &str| -> bool {
        body.get("unstable_features")
            .and_then(|f| f.get(name))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    };

    Ok(Capabilities {
        sliding_sync: feature("org.matrix.simplified_msc3575"),
        threads: feature("org.matrix.msc3440.stable"),
        cross_signing: feature("org.matrix.e2e_cross_signing"),
        versions: body
            .get("versions")
            .and_then(serde_json::Value::as_array)
            .map(|v| {
                v.iter()
                    .filter_map(|s| s.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

async fn reqwest_get_json(url: &str) -> anyhow::Result<serde_json::Value> {
    // `reqwest`'s `json` feature is not enabled in the SDK's dependency, so decode the
    // body ourselves rather than turning on a feature we do not otherwise need.
    let bytes = matrix_sdk::reqwest::get(url).await?.bytes().await?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// What a homeserver supports.
#[derive(Debug, Clone)]
pub struct Capabilities {
    /// MSC4186. **Required** — `RoomListService` cannot work without it.
    pub sliding_sync: bool,
    /// MSC3440. Without it, thread-per-agent-session degrades to one pane per room.
    pub threads: bool,
    pub cross_signing: bool,
    pub versions: Vec<String>,
}

impl Capabilities {
    /// Whether heddle can run against this homeserver at all.
    pub fn is_usable(&self) -> bool {
        self.sliding_sync
    }

    /// Human-readable problems, worst first.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.sliding_sync {
            out.push(
                "homeserver does not advertise org.matrix.simplified_msc3575 (MSC4186); \
                 heddle requires native sliding sync"
                    .to_owned(),
            );
        }
        if !self.threads {
            out.push(
                "homeserver does not advertise org.matrix.msc3440.stable; \
                 thread-per-agent-session will be unavailable"
                    .to_owned(),
            );
        }
        if !self.cross_signing {
            out.push(
                "homeserver does not advertise org.matrix.e2e_cross_signing; \
                 device verification will be unavailable"
                    .to_owned(),
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    #[test]
    fn sliding_sync_is_the_only_hard_requirement() {
        let caps = Capabilities {
            sliding_sync: true,
            threads: false,
            cross_signing: false,
            versions: vec!["v1.12".into()],
        };
        assert!(caps.is_usable());
        assert_eq!(caps.problems().len(), 2);
    }

    #[test]
    fn missing_sliding_sync_is_disqualifying() {
        let caps = Capabilities {
            sliding_sync: false,
            threads: true,
            cross_signing: true,
            versions: vec![],
        };
        assert!(!caps.is_usable());
        assert!(caps.problems()[0].contains("MSC4186"));
    }
}
