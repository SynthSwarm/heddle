//! Session lifecycle: store layout, login, and restore.
//!
//! heddle never puts credentials in the config file. The access token and the E2EE
//! crypto state live together in the SDK's SQLite store under `$XDG_DATA_HOME/heddle`,
//! and the store directory is created with restrictive permissions.

use matrix_sdk::{authentication::matrix::MatrixSession, store::RoomLoadSettings, Client};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Errors raised while establishing a session.
///
/// The SDK's own error types are large, so they are boxed: `SessionError` appears in
/// the `Err` arm of every setup function and an unboxed variant would inflate every
/// `Result` in this module.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("no saved session for profile `{0}`; run `heddle login`")]
    NoSavedSession(String),
    #[error("saved session for profile `{profile}` is unreadable: {source}")]
    CorruptSession {
        profile: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("matrix error: {0}")]
    Matrix(#[from] Box<matrix_sdk::Error>),
    #[error("failed to build client: {0}")]
    Build(#[from] Box<matrix_sdk::ClientBuildError>),
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl From<matrix_sdk::Error> for SessionError {
    fn from(e: matrix_sdk::Error) -> Self {
        Self::Matrix(Box::new(e))
    }
}

impl From<matrix_sdk::ClientBuildError> for SessionError {
    fn from(e: matrix_sdk::ClientBuildError) -> Self {
        Self::Build(Box::new(e))
    }
}

/// Where a profile's persistent state lives.
#[derive(Debug, Clone)]
pub struct Paths {
    /// SDK state + crypto store.
    pub store: PathBuf,
    /// Serialised [`MatrixSession`].
    pub session: PathBuf,
}

impl Paths {
    /// Resolve the paths for a named profile beneath a data directory.
    pub fn for_profile(data_dir: &Path, profile: &str) -> Self {
        let root = data_dir.join("profiles").join(profile);
        Self {
            store: root.join("store"),
            session: root.join("session.json"),
        }
    }

    /// Create the directories, restricting them to the current user.
    ///
    /// The store holds the access token and every Megolm key this device has seen, so a
    /// group- or world-readable directory would be a meaningful leak.
    pub fn ensure(&self) -> Result<(), SessionError> {
        std::fs::create_dir_all(&self.store).map_err(|source| SessionError::Io {
            path: self.store.clone(),
            source,
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let restrict = |p: &Path| -> Result<(), SessionError> {
                std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).map_err(
                    |source| SessionError::Io {
                        path: p.to_path_buf(),
                        source,
                    },
                )
            };
            restrict(&self.store)?;
            if let Some(parent) = self.session.parent() {
                restrict(parent)?;
            }
        }

        Ok(())
    }

    /// Whether a saved session exists.
    pub fn has_session(&self) -> bool {
        self.session.is_file()
    }
}

/// A saved session, plus the homeserver needed to rebuild a client for it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSession {
    pub homeserver: String,
    #[serde(flatten)]
    pub session: MatrixSession,
}

/// Build a client pointed at `homeserver`, backed by the profile's SQLite store.
///
/// `handle_refresh_tokens` is enabled so a homeserver issuing short-lived tokens does
/// not force a re-login mid-session.
pub async fn build_client(homeserver: &str, paths: &Paths) -> Result<Client, SessionError> {
    paths.ensure()?;
    let client = Client::builder()
        .server_name_or_homeserver_url(homeserver)
        .sqlite_store(&paths.store, None)
        .handle_refresh_tokens()
        .build()
        .await?;
    Ok(client)
}

/// Log in with a password and persist the resulting session.
///
/// `device_name` is what other clients show in the device list; a stable, recognisable
/// name matters because the user will be verifying this device by hand.
pub async fn login_password(
    homeserver: &str,
    user: &str,
    password: &str,
    device_name: &str,
    paths: &Paths,
) -> Result<Client, SessionError> {
    let client = build_client(homeserver, paths).await?;

    let response = client
        .matrix_auth()
        .login_username(user, password)
        .initial_device_display_name(device_name)
        .send()
        .await?;

    let saved = SavedSession {
        homeserver: homeserver.to_owned(),
        session: MatrixSession::from(&response),
    };
    save(&saved, paths)?;

    Ok(client)
}

/// Restore a previously saved session.
pub async fn restore(profile: &str, paths: &Paths) -> Result<Client, SessionError> {
    let saved = load(profile, paths)?;
    let client = build_client(&saved.homeserver, paths).await?;
    client
        .matrix_auth()
        .restore_session(saved.session, RoomLoadSettings::default())
        .await?;
    Ok(client)
}

/// Read a saved session from disk.
pub fn load(profile: &str, paths: &Paths) -> Result<SavedSession, SessionError> {
    if !paths.has_session() {
        return Err(SessionError::NoSavedSession(profile.to_owned()));
    }
    let bytes = std::fs::read(&paths.session).map_err(|source| SessionError::Io {
        path: paths.session.clone(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| SessionError::CorruptSession {
        profile: profile.to_owned(),
        source,
    })
}

/// Persist a session, replacing any existing one.
///
/// Written via a temporary file and renamed, so an interrupted write cannot leave a
/// truncated session that would force the user to log in again.
pub fn save(saved: &SavedSession, paths: &Paths) -> Result<(), SessionError> {
    paths.ensure()?;

    let json = serde_json::to_vec_pretty(saved).map_err(|source| SessionError::CorruptSession {
        profile: saved.session.meta.user_id.to_string(),
        source,
    })?;

    let tmp = paths.session.with_extension("json.tmp");
    std::fs::write(&tmp, &json).map_err(|source| SessionError::Io {
        path: tmp.clone(),
        source,
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| SessionError::Io {
                path: tmp.clone(),
                source,
            },
        )?;
    }

    std::fs::rename(&tmp, &paths.session).map_err(|source| SessionError::Io {
        path: paths.session.clone(),
        source,
    })
}

/// Forget a saved session. The crypto store is left alone so that logging back in on
/// the same device keeps its Megolm history.
pub fn forget(paths: &Paths) -> Result<(), SessionError> {
    match std::fs::remove_file(&paths.session) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(SessionError::Io {
            path: paths.session.clone(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    #[test]
    fn profile_paths_are_namespaced() {
        let p = Paths::for_profile(Path::new("/data"), "work");
        assert_eq!(p.store, Path::new("/data/profiles/work/store"));
        assert_eq!(
            p.session,
            Path::new("/data/profiles/work/session.json")
        );
    }

    #[test]
    fn profiles_do_not_share_state() {
        let a = Paths::for_profile(Path::new("/data"), "work");
        let b = Paths::for_profile(Path::new("/data"), "personal");
        assert_ne!(a.store, b.store);
        assert_ne!(a.session, b.session);
    }

    #[test]
    fn missing_session_is_reported_as_such() {
        let dir = std::env::temp_dir().join(format!("heddle-test-{}", std::process::id()));
        let paths = Paths::for_profile(&dir, "nobody");
        assert!(!paths.has_session());
        match load("nobody", &paths) {
            Err(SessionError::NoSavedSession(p)) => assert_eq!(p, "nobody"),
            other => panic!("expected NoSavedSession, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn store_directory_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("heddle-perm-{}", std::process::id()));
        let paths = Paths::for_profile(&dir, "p");
        paths.ensure().expect("creates dirs");

        let mode = std::fs::metadata(&paths.store)
            .expect("store exists")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "store must not be group/world readable"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("heddle-idem-{}", std::process::id()));
        let paths = Paths::for_profile(&dir, "p");
        paths.ensure().expect("first");
        paths.ensure().expect("second");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forgetting_a_missing_session_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!("heddle-forget-{}", std::process::id()));
        let paths = Paths::for_profile(&dir, "p");
        assert!(forget(&paths).is_ok());
    }
}
