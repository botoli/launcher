use std::{
    cell::RefCell,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use atomic_write_file::AtomicWriteFile;
use launcher_core::{OfflineProfile, OfflineProfileError};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Deserialize, Serialize)]
struct Settings {
    username: String,
}

#[derive(Clone)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load_profile(&self) -> Result<Option<OfflineProfile>, SettingsError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(SettingsError::Read {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let settings: Settings =
            serde_json::from_slice(&bytes).map_err(|source| SettingsError::InvalidJson {
                path: self.path.clone(),
                source,
            })?;
        let profile = OfflineProfile::new(settings.username).map_err(|source| {
            SettingsError::InvalidUsername {
                path: self.path.clone(),
                source,
            }
        })?;
        Ok(Some(profile))
    }

    fn save_profile(&self, profile: &OfflineProfile) -> Result<(), SettingsError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| SettingsError::MissingParent {
                path: self.path.clone(),
            })?;
        fs::create_dir_all(parent).map_err(|source| SettingsError::CreateDirectory {
            path: parent.to_owned(),
            source,
        })?;

        let mut file = AtomicWriteFile::open(&self.path).map_err(|source| SettingsError::Open {
            path: self.path.clone(),
            source,
        })?;
        serde_json::to_writer_pretty(
            &mut file,
            &Settings {
                username: profile.username().to_owned(),
            },
        )
        .map_err(|source| SettingsError::Serialize {
            path: self.path.clone(),
            source,
        })?;
        file.write_all(b"\n")
            .and_then(|()| file.flush())
            .and_then(|()| file.sync_all())
            .map_err(|source| SettingsError::Write {
                path: self.path.clone(),
                source,
            })?;
        file.commit().map_err(|source| SettingsError::Commit {
            path: self.path.clone(),
            source,
        })
    }
}

pub struct ProfileController {
    store: SettingsStore,
    persisted_username: RefCell<Option<String>>,
}

impl ProfileController {
    pub fn load(store: SettingsStore) -> (Self, Option<OfflineProfile>) {
        let profile = match store.load_profile() {
            Ok(profile) => profile,
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %store.path().display(),
                    "ignoring invalid launcher settings"
                );
                None
            }
        };
        let persisted_username = profile
            .as_ref()
            .map(OfflineProfile::username)
            .map(str::to_owned);

        (
            Self {
                store,
                persisted_username: RefCell::new(persisted_username),
            },
            profile,
        )
    }

    pub fn accept_username(
        &self,
        username: impl Into<String>,
    ) -> Result<OfflineProfile, OfflineProfileError> {
        let profile = OfflineProfile::new(username)?;
        let changed = self.persisted_username.borrow().as_deref() != Some(profile.username());
        if changed {
            match self.store.save_profile(&profile) {
                Ok(()) => {
                    self.persisted_username
                        .replace(Some(profile.username().to_owned()));
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        path = %self.store.path().display(),
                        "failed to persist offline username"
                    );
                }
            }
        }
        Ok(profile)
    }
}

#[derive(Debug, Error)]
enum SettingsError {
    #[error("failed to read launcher settings")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("launcher settings contain invalid JSON")]
    InvalidJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("launcher settings contain an invalid username")]
    InvalidUsername {
        path: PathBuf,
        #[source]
        source: OfflineProfileError,
    },
    #[error("launcher settings path has no parent directory")]
    MissingParent { path: PathBuf },
    #[error("failed to create launcher settings directory")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to open atomic launcher settings file")]
    Open {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to serialize launcher settings")]
    Serialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to write launcher settings")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to commit launcher settings")]
    Commit {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(root: &Path) -> SettingsStore {
        SettingsStore::new(root.join("state/settings.json"))
    }

    #[test]
    fn missing_settings_returns_no_profile() {
        let root = tempfile::tempdir().unwrap();

        let (_controller, profile) = ProfileController::load(store(root.path()));

        assert!(profile.is_none());
    }

    #[test]
    fn valid_settings_restore_username() {
        let root = tempfile::tempdir().unwrap();
        let settings = store(root.path());
        fs::create_dir_all(settings.path().parent().unwrap()).unwrap();
        fs::write(settings.path(), r#"{"username":"Player_123"}"#).unwrap();

        let (_controller, profile) = ProfileController::load(settings);

        assert_eq!(
            profile.as_ref().map(OfflineProfile::username),
            Some("Player_123")
        );
    }

    #[test]
    fn corrupt_json_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        let settings = store(root.path());
        fs::create_dir_all(settings.path().parent().unwrap()).unwrap();
        fs::write(settings.path(), "{not json").unwrap();

        let (_controller, profile) = ProfileController::load(settings);

        assert!(profile.is_none());
    }

    #[test]
    fn invalid_username_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        let settings = store(root.path());
        fs::create_dir_all(settings.path().parent().unwrap()).unwrap();
        fs::write(settings.path(), r#"{"username":"bad-name"}"#).unwrap();

        let (_controller, profile) = ProfileController::load(settings);

        assert!(profile.is_none());
    }

    #[test]
    fn settings_write_atomically_replaces_existing_file() {
        let root = tempfile::tempdir().unwrap();
        let settings = store(root.path());
        fs::create_dir_all(settings.path().parent().unwrap()).unwrap();
        fs::write(settings.path(), r#"{"username":"OldPlayer"}"#).unwrap();
        let (controller, _) = ProfileController::load(settings.clone());

        controller.accept_username("NewPlayer").unwrap();

        let stored: Settings = serde_json::from_slice(&fs::read(settings.path()).unwrap()).unwrap();
        assert_eq!(stored.username, "NewPlayer");
        let entries = fs::read_dir(settings.path().parent().unwrap())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_name(), "settings.json");
    }

    #[test]
    fn successful_validation_persists_username() {
        let root = tempfile::tempdir().unwrap();
        let settings = store(root.path());
        let (controller, _) = ProfileController::load(settings.clone());

        let profile = controller.accept_username("Player_123").unwrap();

        assert_eq!(profile.username(), "Player_123");
        let stored: Settings = serde_json::from_slice(&fs::read(settings.path()).unwrap()).unwrap();
        assert_eq!(stored.username, "Player_123");
    }

    #[test]
    fn failed_validation_does_not_create_settings() {
        let root = tempfile::tempdir().unwrap();
        let settings = store(root.path());
        let (controller, _) = ProfileController::load(settings.clone());

        assert!(controller.accept_username("bad-name").is_err());
        assert!(!settings.path().exists());
    }

    #[test]
    fn recreated_controller_restores_persisted_username() {
        let root = tempfile::tempdir().unwrap();
        let settings = store(root.path());
        let (controller, _) = ProfileController::load(settings.clone());
        controller.accept_username("RestartPlayer").unwrap();
        drop(controller);

        let (_recreated, profile) = ProfileController::load(settings);

        assert_eq!(
            profile.as_ref().map(OfflineProfile::username),
            Some("RestartPlayer")
        );
    }
}
