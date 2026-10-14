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

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Settings {
    #[serde(default)]
    username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memory_mib: Option<u32>,
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

    fn load(&self) -> Result<Settings, SettingsError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(Settings::default());
            }
            Err(source) => {
                return Err(SettingsError::Read {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        serde_json::from_slice(&bytes).map_err(|source| SettingsError::InvalidJson {
            path: self.path.clone(),
            source,
        })
    }

    fn save(&self, settings: &Settings) -> Result<(), SettingsError> {
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
        serde_json::to_writer_pretty(&mut file, settings).map_err(|source| {
            SettingsError::Serialize {
                path: self.path.clone(),
                source,
            }
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
    current: RefCell<Settings>,
}

impl ProfileController {
    pub fn load(store: SettingsStore) -> (Self, Option<OfflineProfile>) {
        let settings = match store.load() {
            Ok(settings) => settings,
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %store.path().display(),
                    "ignoring invalid launcher settings"
                );
                Settings::default()
            }
        };
        let profile = if settings.username.is_empty() {
            None
        } else {
            match OfflineProfile::new(settings.username.clone()) {
                Ok(profile) => Some(profile),
                Err(error) => {
                    tracing::warn!(%error, "ignoring invalid saved username");
                    None
                }
            }
        };

        (
            Self {
                store,
                current: RefCell::new(settings),
            },
            profile,
        )
    }

    pub fn accept_username(
        &self,
        username: impl Into<String>,
    ) -> Result<OfflineProfile, OfflineProfileError> {
        let profile = OfflineProfile::new(username)?;
        let changed = self.current.borrow().username != profile.username();
        if changed {
            let mut updated = self.current.borrow().clone();
            updated.username = profile.username().to_owned();
            match self.store.save(&updated) {
                Ok(()) => {
                    self.current.replace(updated);
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

    pub fn memory_mib(&self) -> Option<u32> {
        self.current.borrow().memory_mib
    }

    pub fn save_memory(&self, memory_mib: u32) -> Result<(), SettingsError> {
        let mut updated = self.current.borrow().clone();
        updated.memory_mib = Some(memory_mib);
        self.store.save(&updated)?;
        self.current.replace(updated);
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum SettingsError {
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

    #[test]
    fn memory_and_username_updates_preserve_each_other_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let settings = store(root.path());
        fs::create_dir_all(settings.path().parent().unwrap()).unwrap();
        fs::write(settings.path(), r#"{"username":"OldPlayer"}"#).unwrap();
        let (controller, _) = ProfileController::load(settings.clone());
        assert_eq!(controller.memory_mib(), None);
        controller.save_memory(6143).unwrap();
        controller.accept_username("NewPlayer").unwrap();
        controller.save_memory(6144).unwrap();

        let (restored, profile) = ProfileController::load(settings);
        assert_eq!(restored.memory_mib(), Some(6144));
        assert_eq!(profile.unwrap().username(), "NewPlayer");
    }

    #[test]
    fn failed_memory_write_does_not_change_current_settings() {
        let root = tempfile::tempdir().unwrap();
        let settings = store(root.path());
        let (controller, _) = ProfileController::load(settings.clone());
        controller.save_memory(4096).unwrap();
        fs::remove_file(settings.path()).unwrap();
        fs::create_dir(settings.path()).unwrap();

        assert!(controller.save_memory(6144).is_err());
        assert_eq!(controller.memory_mib(), Some(4096));
    }

    #[test]
    fn invalid_saved_username_does_not_discard_memory() {
        let root = tempfile::tempdir().unwrap();
        let settings = store(root.path());
        fs::create_dir_all(settings.path().parent().unwrap()).unwrap();
        fs::write(
            settings.path(),
            r#"{"username":"bad-name","memory_mib":8192}"#,
        )
        .unwrap();
        let (controller, profile) = ProfileController::load(settings);
        assert!(profile.is_none());
        assert_eq!(controller.memory_mib(), Some(8192));
    }
}
