//! Session lifecycle: store layout, login, and restore.
//!
//! heddle never puts credentials in the config file. The access token and the E2EE
//! crypto state live together in the SDK's SQLite store under `$XDG_DATA_HOME/heddle`,
//! and the store directory is created with restrictive permissions.

use matrix_sdk::{
    authentication::matrix::MatrixSession,
    encryption::{BackupDownloadStrategy, EncryptionSettings},
    store::RoomLoadSettings,
    Client, ThreadingSupport,
};
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
///
/// Room keys are fetched from backup on a decryption failure rather than in one sweep.
/// The SDK's default is to fetch nothing at all, which quietly makes a key backup
/// useless: the keys sit on the server and the client never asks. `OneShot` is the other
/// option and pulls the entire backup the moment the key arrives, which for a busy
/// account is a large download the user did not ask for. Fetching on failure is bounded,
/// pays only for history actually looked at, and closes a loop with the late-key retry
/// in the worker: a failure fetches the key, the key arrives on the received stream, and
/// the row that could not be read is decrypted in place.
///
/// Backups are not auto-created. Creating one silently would leave the user with a
/// recovery key they have never seen and cannot write down, which is a backup in name
/// only.
///
/// Threading support is off by default in the SDK, and without it the event cache never
/// files an incoming threaded event under its thread: `post_process_new_events` only
/// populates `new_events_by_thread` when `enabled_thread_support` is set. Thread-focused
/// timelines subscribe to exactly that mapping, so with it off a thread pane shows the
/// backfilled history and our own local echoes, and silently misses every reply that
/// arrives over sync. Reactions and redactions still land, because aggregations against
/// an event already in the timeline take a different path -- which is what made this look
/// like an agent that reacts but never answers.
///
/// `with_subscriptions` stays false: MSC4306/MSC4308 subscriptions are a separate feature
/// needing server support, and routing is all we are after.
pub async fn build_client(homeserver: &str, paths: &Paths) -> Result<Client, SessionError> {
    paths.ensure()?;
    let client = Client::builder()
        .server_name_or_homeserver_url(homeserver)
        .sqlite_store(&paths.store, None)
        .handle_refresh_tokens()
        .with_encryption_settings(EncryptionSettings {
            backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
            auto_enable_backups: false,
            auto_enable_cross_signing: false,
        })
        .with_threading_support(ThreadingSupport::Enabled {
            with_subscriptions: false,
        })
        .build()
        .await?;
    Ok(client)
}

/// What a cross-signing bootstrap did, so the caller can say so plainly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossSigning {
    /// The account already had an identity; nothing was uploaded.
    AlreadyPresent,
    /// A new identity was created, signed by this device, and uploaded.
    Created,
    /// The homeserver demanded an auth flow a password cannot satisfy.
    ///
    /// Not an error: the session is valid and usable, it simply has no cross-signing
    /// identity yet, and one will have to be set up from a client that can drive the
    /// server's chosen flow.
    NeedsInteractiveAuth,
}

/// Log in with a password and persist the resulting session.
///
/// `device_name` is what other clients show in the device list; a stable, recognisable
/// name matters because the user will be verifying this device by hand.
///
/// Cross-signing is bootstrapped here rather than later because uploading the signing
/// keys is a user-interactive-auth endpoint, and login is the one moment heddle legit-
/// imately holds the password. Deferring it to the TUI would mean prompting for the
/// password a second time, which trains exactly the habit an E2EE client should not.
pub async fn login_password(
    homeserver: &str,
    user: &str,
    password: &str,
    device_name: &str,
    paths: &Paths,
) -> Result<(Client, CrossSigning), SessionError> {
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

    let outcome = bootstrap_cross_signing(&client, user, password).await?;

    Ok((client, outcome))
}

/// Create this account's cross-signing identity if it has none.
///
/// `bootstrap_cross_signing_if_needed` is used rather than the unconditional form for a
/// reason worth stating: the unconditional call *replaces* an existing identity, which
/// would invalidate every verification the user has ever done from every other client.
/// The `_if_needed` variant runs an initial key query first, so an account that already
/// has an identity is left alone rather than being judged absent merely because this
/// brand-new device has not yet asked the server.
///
/// The first attempt deliberately passes no auth data: the endpoint always rejects that
/// with a UIAA challenge, and the response carries the session id the real attempt must
/// quote back. A server offering only SSO or another non-password flow is reported, not
/// failed, because a session without cross-signing still works for unencrypted rooms.
pub async fn bootstrap_cross_signing(
    client: &Client,
    user: &str,
    password: &str,
) -> Result<CrossSigning, SessionError> {
    use matrix_sdk::ruma::api::client::uiaa;

    let encryption = client.encryption();

    let error = match encryption.bootstrap_cross_signing_if_needed(None).await {
        // Either the identity was already there, or the server took the upload without
        // asking us to prove anything. Both leave the account cross-signed.
        Ok(()) => return Ok(already_or_created(client).await),
        Err(e) => e,
    };

    let Some(challenge) = error.as_uiaa_response() else {
        return Err(SessionError::Matrix(Box::new(error)));
    };

    let mut auth = uiaa::Password::new(
        uiaa::UserIdentifier::Matrix(uiaa::MatrixUserIdentifier::new(user.to_owned())),
        password.to_owned(),
    );
    auth.session = challenge.session.clone();

    match encryption
        .bootstrap_cross_signing_if_needed(Some(uiaa::AuthData::Password(auth)))
        .await
    {
        Ok(()) => Ok(CrossSigning::Created),
        Err(e) if e.as_uiaa_response().is_some() => Ok(CrossSigning::NeedsInteractiveAuth),
        Err(e) => Err(SessionError::Matrix(Box::new(e))),
    }
}

/// Decide what to report when the bootstrap call succeeded without a challenge.
///
/// Holding all three secret halves locally means this device minted the identity; an
/// account that merely already had one leaves the private keys on whichever device did.
async fn already_or_created(client: &Client) -> CrossSigning {
    match client.encryption().cross_signing_status().await {
        Some(status) if status.has_master && status.has_self_signing && status.has_user_signing => {
            CrossSigning::Created
        }
        _ => CrossSigning::AlreadyPresent,
    }
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
        assert_eq!(p.session, Path::new("/data/profiles/work/session.json"));
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
