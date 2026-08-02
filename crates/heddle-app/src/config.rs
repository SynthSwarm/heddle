//! Configuration file handling.
//!
//! Credentials never live here — only the homeserver and user ID. The access token and
//! E2EE keys stay in the SDK store. See `docs/SPEC.md` §5.4.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub profile: BTreeMap<String, Profile>,
    pub agent: Agent,
    pub ui: Ui,
    pub notify: Notify,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub user_id: String,
    pub homeserver: String,
    /// Used when `--profile` is not given.
    pub default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Agent {
    /// Matrix user IDs treated as agents rather than humans.
    pub ids: Vec<String>,
    /// When to auto-expand tool cards: `never`, `running` or `always`.
    pub auto_expand: String,
    /// Recover agent structure from human-readable chrome when the
    /// `dev.hermes.agent.v1` extension is absent.
    pub fallback_parse: bool,
    /// Show reasoning/commentary blocks.
    pub show_commentary: bool,
}

impl Default for Agent {
    fn default() -> Self {
        Self {
            ids: Vec::new(),
            auto_expand: "running".into(),
            fallback_parse: true,
            show_commentary: true,
        }
    }
}

impl Agent {
    pub fn auto_expand(&self) -> heddle_render::AutoExpand {
        match self.auto_expand.as_str() {
            "never" => heddle_render::AutoExpand::Never,
            "always" => heddle_render::AutoExpand::Always,
            "running" => heddle_render::AutoExpand::Running,
            other => {
                tracing::warn!(value = %other, "unknown agent.auto_expand; using `running`");
                heddle_render::AutoExpand::Running
            }
        }
    }

    /// Whether a sender should be rendered as an agent.
    ///
    /// With no configured IDs every sender is a candidate, so that a freshly installed
    /// heddle still renders tool cards without being told about the bot first.
    pub fn is_agent(&self, user_id: &str) -> bool {
        self.ids.is_empty() || self.ids.iter().any(|id| id == user_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Ui {
    /// Prefix key, in crossterm-ish notation.
    pub prefix: String,
    pub theme: String,
    /// `auto`, `kitty`, `sixel`, `iterm2`, `blocks` or `off`.
    pub images: String,
    pub mouse: bool,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            // ctrl+a rather than ctrl+b, so heddle does not fight tmux or herdr when
            // nested inside one.
            prefix: "ctrl+a".into(),
            theme: "default".into(),
            images: "auto".into(),
            mouse: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Notify {
    pub enabled: bool,
    /// Any of `blocked`, `done`, `mention`.
    pub on: Vec<String>,
}

impl Default for Notify {
    fn default() -> Self {
        Self {
            enabled: true,
            on: vec!["blocked".into(), "mention".into()],
        }
    }
}

impl Config {
    /// Load from `path`, or return defaults when it does not exist.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// The profile to use, honouring an explicit `--profile`.
    pub fn resolve_profile(&self, requested: Option<&str>) -> Option<(String, Profile)> {
        if let Some(name) = requested {
            return self.profile.get(name).map(|p| (name.to_owned(), p.clone()));
        }
        // An explicit `default = true` wins; otherwise fall back to the only profile
        // present, which is the common single-account case.
        self.profile
            .iter()
            .find(|(_, p)| p.default)
            .or_else(|| {
                (self.profile.len() == 1)
                    .then(|| self.profile.iter().next())
                    .flatten()
            })
            .map(|(name, p)| (name.clone(), p.clone()))
    }
}

/// Standard locations, honouring the XDG spec.
pub struct Dirs {
    pub config: PathBuf,
    pub data: PathBuf,
    pub state: PathBuf,
}

impl Dirs {
    pub fn resolve() -> anyhow::Result<Self> {
        let dirs = directories::ProjectDirs::from("dev", "heddle", "heddle")
            .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
        Ok(Self {
            config: dirs.config_dir().to_path_buf(),
            data: dirs.data_dir().to_path_buf(),
            state: dirs
                .state_dir()
                .unwrap_or_else(|| dirs.data_dir())
                .to_path_buf(),
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.toml")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    #[test]
    fn missing_config_yields_defaults() {
        let cfg = Config::load(Path::new("/nonexistent/heddle.toml")).expect("defaults");
        assert!(cfg.profile.is_empty());
        assert!(cfg.agent.fallback_parse);
        assert_eq!(cfg.ui.prefix, "ctrl+a");
    }

    #[test]
    fn parses_a_realistic_config() {
        let text = r#"
            [profile.work]
            user_id = "@quintin:matrix.example.org"
            homeserver = "https://matrix.example.org"
            default = true

            [agent]
            ids = ["@hermes:matrix.example.org"]
            auto_expand = "always"

            [ui]
            prefix = "ctrl+b"
            mouse = false
        "#;
        let cfg: Config = toml::from_str(text).expect("parses");
        let (name, profile) = cfg.resolve_profile(None).expect("default profile");
        assert_eq!(name, "work");
        assert_eq!(profile.homeserver, "https://matrix.example.org");
        assert_eq!(cfg.agent.auto_expand(), heddle_render::AutoExpand::Always);
        assert!(!cfg.ui.mouse);
        // Unspecified keys keep their defaults rather than zeroing out.
        assert!(cfg.agent.fallback_parse);
        assert_eq!(cfg.ui.theme, "default");
    }

    #[test]
    fn a_single_profile_needs_no_default_flag() {
        let text = r#"
            [profile.only]
            user_id = "@a:b"
            homeserver = "https://b"
        "#;
        let cfg: Config = toml::from_str(text).expect("parses");
        assert_eq!(cfg.resolve_profile(None).expect("resolved").0, "only");
    }

    #[test]
    fn ambiguous_profiles_without_a_default_resolve_to_nothing() {
        let text = r#"
            [profile.a]
            user_id = "@a:b"
            homeserver = "https://b"
            [profile.c]
            user_id = "@c:d"
            homeserver = "https://d"
        "#;
        let cfg: Config = toml::from_str(text).expect("parses");
        assert!(
            cfg.resolve_profile(None).is_none(),
            "guessing between accounts would be worse than asking"
        );
        assert!(cfg.resolve_profile(Some("c")).is_some());
    }

    #[test]
    fn an_unknown_auto_expand_falls_back_rather_than_failing() {
        let agent = Agent {
            auto_expand: "nonsense".into(),
            ..Agent::default()
        };
        assert_eq!(agent.auto_expand(), heddle_render::AutoExpand::Running);
    }

    #[test]
    fn every_sender_is_an_agent_candidate_until_told_otherwise() {
        let agent = Agent::default();
        assert!(agent.is_agent("@anyone:x"));

        let configured = Agent {
            ids: vec!["@hermes:x".into()],
            ..Agent::default()
        };
        assert!(configured.is_agent("@hermes:x"));
        assert!(!configured.is_agent("@someone-else:x"));
    }
}
