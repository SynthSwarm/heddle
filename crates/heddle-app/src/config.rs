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
    /// Agent integrations to consult, in order. See `heddle_agent::adapter`.
    ///
    /// `heddle` reads the published structured schema from any agent that emits it;
    /// `hermes` reads Hermes' legacy key and recovers its tool chrome from plain text.
    pub adapters: Vec<String>,
    /// When to auto-expand tool cards: `never`, `running` or `always`.
    pub auto_expand: String,
    /// Recover agent structure from human-readable chrome when no structured extension
    /// is present. This is the only path in use until an agent emits the schema.
    pub fallback_parse: bool,
    /// Show reasoning/commentary blocks.
    pub show_commentary: bool,
}

impl Default for Agent {
    fn default() -> Self {
        Self {
            ids: Vec::new(),
            adapters: vec!["heddle".into(), "hermes".into()],
            auto_expand: "running".into(),
            fallback_parse: true,
            show_commentary: true,
        }
    }
}

impl Agent {
    pub fn adapters(&self) -> heddle_agent::Adapters {
        heddle_agent::Adapters::by_id(self.adapters.iter().map(String::as_str))
            .textual(self.fallback_parse)
    }
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
    ///
    /// The only rebindable key. Everything else is fixed; see `docs/SPEC.md` §5.3.
    pub prefix: String,
    pub mouse: bool,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            // ctrl+a rather than ctrl+b, so heddle does not fight tmux or herdr when
            // nested inside one.
            prefix: "ctrl+a".into(),
            mouse: true,
        }
    }
}

/// Every section and key heddle acts on.
///
/// Anything else in the file is warned about: a key read and ignored tells the user
/// their preference was applied when it was not. Warned rather than rejected, so a
/// stale key from an older version costs a log line rather than a refusal to start.
const KNOWN: &[(&str, &[&str])] = &[
    ("profile", &["user_id", "homeserver", "default"]),
    (
        "agent",
        &[
            "ids",
            "adapters",
            "auto_expand",
            "fallback_parse",
            "show_commentary",
        ],
    ),
    ("ui", &["prefix", "mouse"]),
];

/// Report keys heddle does not act on.
///
/// Returns human-readable complaints rather than logging directly, so the behaviour can
/// be tested without capturing a subscriber.
pub fn unknown_keys(text: &str) -> Vec<String> {
    // `toml::from_str`, not `str::parse`: in toml 0.9 the `FromStr` impl on `Value`
    // parses a bare value rather than a document, so parsing a config file through it
    // fails and this function silently approves of everything.
    let Ok(root) = toml::from_str::<toml::Table>(text) else {
        // The caller's own deserialisation reports this with a better message.
        return Vec::new();
    };

    let mut complaints = Vec::new();
    for (section, value) in &root {
        let Some(known) = KNOWN.iter().find(|(name, _)| name == section) else {
            complaints.push(format!("[{section}] is not a section heddle reads"));
            continue;
        };
        let Some(table) = value.as_table() else {
            continue;
        };

        // `[profile.<name>]` nests one level deeper than the others.
        let entries: Vec<(String, &toml::Value)> = if section == "profile" {
            table
                .iter()
                .filter_map(|(name, v)| v.as_table().map(|t| (name.clone(), t)))
                .flat_map(|(name, t)| {
                    t.iter()
                        .map(move |(k, v)| (format!("profile.{name}.{k}"), v))
                        .collect::<Vec<_>>()
                })
                .collect()
        } else {
            table
                .iter()
                .map(|(k, v)| (format!("{section}.{k}"), v))
                .collect()
        };

        for (path, _) in entries {
            let leaf = path.rsplit('.').next().unwrap_or(&path);
            if !known.1.contains(&leaf) {
                complaints.push(format!("`{path}` is not a setting heddle reads"));
            }
        }
    }
    complaints
}

impl Config {
    /// Load from `path`, or return defaults when it does not exist.
    ///
    /// Keys heddle does not act on are warned about; see [`KNOWN`].
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                for complaint in unknown_keys(&text) {
                    tracing::warn!(file = %path.display(), "{complaint}");
                }
                Ok(toml::from_str(&text)?)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// The profile to use, honouring an explicit `--profile`.
    pub fn resolve_profile(&self, requested: Option<&str>) -> Option<(String, Profile)> {
        if let Some(name) = requested {
            return self.profile.get(name).map(|p| (name.to_owned(), p.clone()));
        }
        // An explicit `default = true` wins; otherwise the only profile present.
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

    pub fn has_profiles(&self) -> bool {
        !self.profile.is_empty()
    }
}

/// Add a `[profile.<name>]` block to the config file unless one is already there.
///
/// Appended as text: a round trip through `toml::to_string` discards every comment and
/// any ordering the user chose. Returns whether a block was written, so the caller can
/// stay quiet on a re-login.
pub fn append_profile(path: &Path, name: &str, profile: &Profile) -> anyhow::Result<bool> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };

    // Parse first: appending to a file we cannot understand compounds the error.
    let parsed: Config = toml::from_str(&existing)?;
    if parsed.profile.contains_key(name) {
        return Ok(false);
    }

    // The first profile becomes the default, so a bare `heddle` works.
    let default = !parsed.has_profiles();

    let mut block = String::new();
    if !existing.is_empty() && !existing.ends_with('\n') {
        block.push('\n');
    }
    if !existing.is_empty() {
        block.push('\n');
    }
    block.push_str(&format!("[profile.{name}]\n"));
    block.push_str(&format!("user_id = {}\n", quote(&profile.user_id)));
    block.push_str(&format!("homeserver = {}\n", quote(&profile.homeserver)));
    if default {
        block.push_str("default = true\n");
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let updated = format!("{existing}{block}");
    std::fs::write(path, updated)?;

    Ok(true)
}

/// Quote a TOML basic string, escaping what the format requires.
fn quote(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
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

    /// Where a profile's pane arrangement is remembered.
    ///
    /// Discardable: everything in it is derived from what the user did last time. Per
    /// profile, since two accounts have different rooms.
    pub fn layout_file(&self, profile: &str) -> PathBuf {
        self.state.join("layout").join(format!("{profile}.json"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use tempfile::TempDir;

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
            user_id = "@you:example.org"
            homeserver = "https://matrix.example.org"
            default = true

            [agent]
            ids = ["@hermes:example.org"]
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
        assert_eq!(cfg.agent.adapters, vec!["heddle", "hermes"]);
    }

    #[test]
    fn every_documented_key_is_a_key_heddle_reads() {
        // The config example in SPEC.md §5.4 is what people copy. If it contains a key
        // that does nothing, that is the lie this whole table exists to prevent.
        let documented = r#"
            [profile.work]
            user_id = "@you:example.org"
            homeserver = "https://matrix.example.org"
            default = true

            [agent]
            ids = ["@hermes:example.org"]
            adapters = ["heddle", "hermes"]
            auto_expand = "running"
            fallback_parse = true
            show_commentary = true

            [ui]
            prefix = "ctrl+a"
            mouse = true
        "#;
        assert!(
            unknown_keys(documented).is_empty(),
            "{:?}",
            unknown_keys(documented)
        );
        toml::from_str::<Config>(documented).expect("the documented config must parse");
    }

    #[test]
    fn a_setting_heddle_does_not_act_on_is_complained_about() {
        // These three were parsed and silently ignored for the whole of M1 to M4:
        // theming, image protocols and desktop notifications were all configurable and
        // none of them did anything.
        let stale = r#"
            [ui]
            prefix = "ctrl+a"
            theme = "dracula"
            images = "kitty"

            [notify]
            enabled = true
        "#;
        let complaints = unknown_keys(stale);
        assert!(complaints.iter().any(|c| c.contains("ui.theme")));
        assert!(complaints.iter().any(|c| c.contains("ui.images")));
        assert!(complaints.iter().any(|c| c.contains("[notify]")));

        // And the file still loads: a stale key costs a warning, not a launch.
        toml::from_str::<Config>(stale).expect("still parses");
    }

    #[test]
    fn a_typo_in_a_profile_is_caught_too() {
        let text = r#"
            [profile.work]
            user_id = "@you:example.org"
            homserver = "https://matrix.example.org"
        "#;
        let complaints = unknown_keys(text);
        assert!(
            complaints
                .iter()
                .any(|c| c.contains("profile.work.homserver")),
            "{complaints:?}"
        );
    }

    #[test]
    fn adapters_are_resolved_from_config() {
        let cfg = Agent {
            adapters: vec!["hermes".into()],
            ..Agent::default()
        };
        assert_eq!(cfg.adapters().ids(), vec!["hermes"]);

        // Switching the lossy path off must not switch off the whole integration.
        let no_fallback = Agent {
            fallback_parse: false,
            ..Agent::default()
        };
        assert_eq!(no_fallback.adapters().ids(), vec!["heddle", "hermes"]);
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

    /// A config path in a directory that deletes itself. Returned together, because
    /// dropping the guard removes the file.
    fn scratch() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("scratch dir");
        let path = dir.path().join("config.toml");
        (dir, path)
    }

    fn profile(user: &str) -> Profile {
        Profile {
            user_id: user.into(),
            homeserver: "https://matrix.example.org".into(),
            default: false,
        }
    }

    #[test]
    fn logging_in_creates_a_usable_profile_from_nothing() {
        let (_scratch, path) = scratch();
        let _ = std::fs::remove_file(&path);

        assert!(append_profile(&path, "lab", &profile("@q:example.org")).expect("writes"));

        let cfg = Config::load(&path).expect("reloads");
        let (name, p) = cfg
            .resolve_profile(Some("lab"))
            .expect("the profile just written must resolve");
        assert_eq!(name, "lab");
        assert_eq!(p.user_id, "@q:example.org");
        // The whole point: a bare `heddle` has to work after a single login.
        assert_eq!(cfg.resolve_profile(None).expect("default").0, "lab");
    }

    #[test]
    fn a_second_profile_does_not_steal_the_default() {
        let (_scratch, path) = scratch();
        let _ = std::fs::remove_file(&path);

        append_profile(&path, "first", &profile("@a:example.org")).expect("first");
        append_profile(&path, "second", &profile("@b:example.org")).expect("second");

        let cfg = Config::load(&path).expect("reloads");
        assert_eq!(cfg.profile.len(), 2);
        assert!(cfg.profile["first"].default);
        assert!(
            !cfg.profile["second"].default,
            "adding an account must not silently redirect the bare `heddle` command"
        );
        assert_eq!(cfg.resolve_profile(None).expect("default").0, "first");
    }

    #[test]
    fn logging_in_again_changes_nothing() {
        let (_scratch, path) = scratch();
        let _ = std::fs::remove_file(&path);

        append_profile(&path, "lab", &profile("@q:example.org")).expect("first");
        let after_first = std::fs::read_to_string(&path).expect("read");

        assert!(
            !append_profile(&path, "lab", &profile("@q:example.org")).expect("second"),
            "a re-login must report that it wrote nothing"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            after_first,
            "re-login must not duplicate the block"
        );
    }

    #[test]
    fn hand_written_comments_and_settings_survive() {
        let (_scratch, path) = scratch();
        let original = "# my notes, kept by hand\n\
                        [ui]\n\
                        prefix = \"ctrl+b\"  # deliberate\n";
        std::fs::write(&path, original).expect("seed");

        append_profile(&path, "lab", &profile("@q:example.org")).expect("appends");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.starts_with(original),
            "existing bytes must be untouched"
        );
        assert!(text.contains("# my notes, kept by hand"));
        assert!(text.contains("# deliberate"));

        let cfg = Config::load(&path).expect("still parses");
        assert_eq!(cfg.ui.prefix, "ctrl+b");
        assert!(cfg.profile.contains_key("lab"));
    }

    #[test]
    fn a_broken_config_is_not_made_worse() {
        let (_scratch, path) = scratch();
        std::fs::write(&path, "[ui\nthis is not toml").expect("seed");

        assert!(
            append_profile(&path, "lab", &profile("@q:example.org")).is_err(),
            "appending to a file we cannot parse would compound the user's error"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "[ui\nthis is not toml",
            "the file must be left exactly as found"
        );
    }

    #[test]
    fn quoting_survives_a_hostile_display_name() {
        let (_scratch, path) = scratch();
        let _ = std::fs::remove_file(&path);

        let nasty = r#"@odd"user\name:example.org"#;
        append_profile(&path, "odd", &profile(nasty)).expect("writes");

        let cfg = Config::load(&path).expect("parses despite the quotes");
        assert_eq!(cfg.profile["odd"].user_id, nasty);
    }
}
