use std::{
    env::{self, VarError},
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
};

use launcher_core::{LauncherBackendConfig, LauncherBackendConfigError};
use thiserror::Error;

pub const PRODUCTION_BACKEND_URL: &str = "http://104.143.218.8:3000";

const APPLICATION_DIRECTORY: &str = "EquestrianDreamLauncher";
const SERVER_URL_ENV: &str = "LAUNCHER_SERVER_URL";
const DATA_DIRECTORY_ENV: &str = "LAUNCHER_DATA_DIR";
const LOCAL_APP_DATA_ENV: &str = "LOCALAPPDATA";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplicationPaths {
    root: PathBuf,
    game: PathBuf,
    state: PathBuf,
    logs: PathBuf,
}

impl ApplicationPaths {
    fn resolve(values: &EnvironmentValues) -> Result<Self, AppConfigError> {
        let root = match values.data_directory.as_deref() {
            Some(value) => required_path(value, DATA_DIRECTORY_ENV)?,
            None => required_path(
                values
                    .local_app_data
                    .as_deref()
                    .ok_or(AppConfigError::MissingVariable {
                        name: LOCAL_APP_DATA_ENV,
                    })?,
                LOCAL_APP_DATA_ENV,
            )?
            .join(APPLICATION_DIRECTORY),
        };

        Ok(Self {
            game: root.join("game"),
            state: root.join("state"),
            logs: root.join("logs"),
            root,
        })
    }

    pub fn prepare(&self) -> Result<(), AppConfigError> {
        for path in [&self.game, &self.state, &self.logs] {
            fs::create_dir_all(path).map_err(|source| AppConfigError::PrepareDirectory {
                path: path.clone(),
                source,
            })?;
        }
        Ok(())
    }

    pub fn game(&self) -> &Path {
        &self.game
    }

    pub fn logs(&self) -> &Path {
        &self.logs
    }

    pub fn settings_file(&self) -> PathBuf {
        self.state.join("settings.json")
    }
}

pub struct DesktopAppConfig {
    pub backend: LauncherBackendConfig,
    pub paths: ApplicationPaths,
}

impl DesktopAppConfig {
    pub fn load() -> Result<Self, AppConfigError> {
        Self::resolve(EnvironmentValues::from_process()?)
    }

    fn resolve(values: EnvironmentValues) -> Result<Self, AppConfigError> {
        let server_url = values
            .server_url
            .as_deref()
            .unwrap_or(PRODUCTION_BACKEND_URL);
        if server_url.trim().is_empty() {
            return Err(AppConfigError::EmptyVariable {
                name: SERVER_URL_ENV,
            });
        }
        let server_url = server_url.parse().map_err(|_| AppConfigError::InvalidUrl {
            name: SERVER_URL_ENV,
        })?;
        let backend = LauncherBackendConfig::new(server_url)
            .map_err(|source| AppConfigError::InvalidBackend { source })?;
        let paths = ApplicationPaths::resolve(&values)?;

        Ok(Self { backend, paths })
    }
}

#[derive(Default)]
struct EnvironmentValues {
    server_url: Option<String>,
    data_directory: Option<OsString>,
    local_app_data: Option<OsString>,
}

impl EnvironmentValues {
    fn from_process() -> Result<Self, AppConfigError> {
        Ok(Self {
            server_url: read_optional_text_environment(SERVER_URL_ENV)?,
            data_directory: env::var_os(DATA_DIRECTORY_ENV),
            local_app_data: env::var_os(LOCAL_APP_DATA_ENV),
        })
    }
}

fn read_optional_text_environment(name: &'static str) -> Result<Option<String>, AppConfigError> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(AppConfigError::InvalidEnvironmentEncoding { name }),
    }
}

fn required_path(value: &OsStr, name: &'static str) -> Result<PathBuf, AppConfigError> {
    if value.is_empty() {
        return Err(AppConfigError::EmptyVariable { name });
    }
    Ok(PathBuf::from(value))
}

#[derive(Debug, Error)]
pub enum AppConfigError {
    #[error("environment variable {name} is missing")]
    MissingVariable { name: &'static str },
    #[error("environment variable {name} is not valid Unicode")]
    InvalidEnvironmentEncoding { name: &'static str },
    #[error("environment variable {name} is empty")]
    EmptyVariable { name: &'static str },
    #[error("environment variable {name} is not a valid URL")]
    InvalidUrl { name: &'static str },
    #[error("invalid launcher backend configuration: {source}")]
    InvalidBackend {
        #[source]
        source: LauncherBackendConfigError,
    },
    #[error("failed to prepare application directory {path}")]
    PrepareDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> EnvironmentValues {
        EnvironmentValues {
            server_url: None,
            data_directory: None,
            local_app_data: Some(OsString::from(r"C:\Users\Test\AppData\Local")),
        }
    }

    #[test]
    fn uses_production_backend_by_default() {
        let config = DesktopAppConfig::resolve(values()).unwrap();

        assert_eq!(PRODUCTION_BACKEND_URL, "http://104.143.218.8:3000");
        assert_eq!(
            config.backend.server_url().as_str(),
            "http://104.143.218.8:3000/"
        );
        assert_eq!(
            config.backend.client_config_url().as_str(),
            "http://104.143.218.8:3000/api/client-config"
        );
    }

    #[test]
    fn environment_backend_overrides_production_default() {
        let mut environment = values();
        environment.server_url = Some("http://127.0.0.1:3000".to_owned());

        let config = DesktopAppConfig::resolve(environment).unwrap();

        assert_eq!(
            config.backend.client_config_url().as_str(),
            "http://127.0.0.1:3000/api/client-config"
        );
    }

    #[test]
    fn resolves_windows_local_app_data_layout() {
        let config = DesktopAppConfig::resolve(values()).unwrap();
        let root = PathBuf::from(r"C:\Users\Test\AppData\Local\EquestrianDreamLauncher");

        assert_eq!(config.paths.root, root);
        assert_eq!(config.paths.game(), root.join("game"));
        assert_eq!(config.paths.state, root.join("state"));
        assert_eq!(config.paths.logs, root.join("logs"));
    }

    #[test]
    fn data_directory_environment_overrides_application_root() {
        let mut environment = values();
        environment.data_directory = Some(OsString::from(r"D:\Launcher Test"));

        let config = DesktopAppConfig::resolve(environment).unwrap();
        let root = PathBuf::from(r"D:\Launcher Test");

        assert_eq!(config.paths.root, root);
        assert_eq!(config.paths.game(), root.join("game"));
        assert_eq!(
            config.paths.settings_file(),
            root.join("state/settings.json")
        );
        assert_eq!(
            config.paths.logs.join("launcher.log"),
            root.join("logs/launcher.log")
        );
    }

    #[test]
    fn directory_preparation_is_isolated_to_a_temporary_override() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("launcher-data");
        let mut environment = values();
        environment.data_directory = Some(root.clone().into_os_string());
        let config = DesktopAppConfig::resolve(environment).unwrap();

        config.paths.prepare().unwrap();

        assert!(root.join("game").is_dir());
        assert!(root.join("state").is_dir());
        assert!(root.join("logs").is_dir());
        assert_eq!(
            root.parent().unwrap(),
            temporary.path(),
            "the test layout must stay inside tempfile"
        );
    }
}
