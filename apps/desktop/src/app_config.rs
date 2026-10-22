use std::{
    env::{self, VarError},
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
};

use launcher_core::{LauncherBackendConfig, LauncherBackendConfigError};
use serde::Deserialize;
use thiserror::Error;

const CONFIG_FILE: &str = "launcher-config.json";
const DEFAULT_CONFIG: &str = include_str!("../../../launcher-config.json");

#[derive(Deserialize)]
struct FileConfig {
    server_url: String,
    #[serde(default)]
    java21_msi_url: Option<String>,
}

fn read_file_config(path: &Path) -> Result<FileConfig, AppConfigError> {
    let json = match fs::read_to_string(path) {
        Ok(json) => json,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => DEFAULT_CONFIG.to_owned(),
        Err(source) => {
            return Err(AppConfigError::ReadConfig {
                path: path.to_owned(),
                source,
            });
        }
    };
    serde_json::from_str(&json).map_err(AppConfigError::InvalidConfig)
}

#[cfg(test)]
fn read_server_url(path: &Path) -> Result<String, AppConfigError> {
    read_file_config(path).map(|config| config.server_url)
}

fn parse_server_url(json: &str) -> Result<String, AppConfigError> {
    serde_json::from_str::<FileConfig>(json)
        .map(|config| config.server_url)
        .map_err(AppConfigError::InvalidConfig)
}

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

#[derive(Clone)]
pub struct DesktopAppConfig {
    pub backend: LauncherBackendConfig,
    pub paths: ApplicationPaths,
    pub java_installer_url: String,
}

impl DesktopAppConfig {
    #[cfg(test)]
    pub fn for_test(root: &Path, backend: &str) -> Self {
        Self {
            backend: LauncherBackendConfig::new(backend.parse().unwrap()).unwrap(),
            paths: ApplicationPaths {
                root: root.to_owned(),
                game: root.join("game"),
                state: root.join("state"),
                logs: root.join("logs"),
            },
            java_installer_url: String::new(),
        }
    }

    pub fn load() -> Result<Self, AppConfigError> {
        let values = EnvironmentValues::from_process()?;
        let executable = env::current_exe().map_err(AppConfigError::ExecutablePath)?;
        Self::load_from(&executable.with_file_name(CONFIG_FILE), values)
    }

    fn load_from(path: &Path, mut values: EnvironmentValues) -> Result<Self, AppConfigError> {
        let file = read_file_config(path)?;
        if values.server_url.is_none() {
            values.server_url = Some(file.server_url.clone());
        }
        let mut config = Self::resolve(values)?;
        // Old external configs only contain server_url. Use the URL embedded in
        // the current release when this new optional field is absent.
        config.java_installer_url = file.java21_msi_url.unwrap_or(config.java_installer_url);
        Ok(config)
    }

    fn resolve(values: EnvironmentValues) -> Result<Self, AppConfigError> {
        let default_url = parse_server_url(DEFAULT_CONFIG)?;
        let server_url = values.server_url.as_deref().unwrap_or(&default_url).trim();
        if server_url.trim().is_empty() {
            return Err(AppConfigError::EmptyVariable {
                name: SERVER_URL_ENV,
            });
        }
        let server_url = match server_url.trim_end_matches('/') {
            "http://87.58.220.248:3000" => &default_url,
            _ => server_url,
        };
        let mut backend = LauncherBackendConfig::new(server_url.parse().map_err(|_| {
            AppConfigError::InvalidUrl {
                name: SERVER_URL_ENV,
            }
        })?)
        .map_err(|source| AppConfigError::InvalidBackend { source })?;
        // All services and session storage use the same origin, even when the
        // configured address includes /api/client-config or a trailing slash.
        let mut origin = backend.server_url().clone();
        origin.set_path("/");
        origin.set_query(None);
        origin.set_fragment(None);
        backend = LauncherBackendConfig::new(origin)
            .map_err(|source| AppConfigError::InvalidBackend { source })?;
        let paths = ApplicationPaths::resolve(&values)?;

        let defaults: FileConfig =
            serde_json::from_str(DEFAULT_CONFIG).map_err(AppConfigError::InvalidConfig)?;
        Ok(Self {
            backend,
            paths,
            java_installer_url: defaults.java21_msi_url.unwrap_or_default(),
        })
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
    #[error("failed to locate launcher executable: {0}")]
    ExecutablePath(std::io::Error),
    #[error("failed to read configuration {path}: {source}")]
    ReadConfig {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid launcher-config.json (expected server_url string): {0}")]
    InvalidConfig(#[source] serde_json::Error),
    #[error("environment variable {name} is missing")]
    MissingVariable { name: &'static str },
    #[error("environment variable {name} is not valid Unicode")]
    InvalidEnvironmentEncoding { name: &'static str },
    #[error("configuration value {name} is empty")]
    EmptyVariable { name: &'static str },
    #[error("configuration value {name} is not a valid URL")]
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

        assert_eq!(
            config.backend.server_url().as_str(),
            "https://launcher.nevermoreland.ru/"
        );
        assert_eq!(
            config.backend.client_config_url().as_str(),
            "https://launcher.nevermoreland.ru/api/client-config"
        );
    }

    #[test]
    fn legacy_production_override_uses_final_https_origin() {
        let mut environment = values();
        environment.server_url = Some("http://87.58.220.248:3000/".into());
        let config = DesktopAppConfig::resolve(environment).unwrap();
        assert_eq!(
            config.backend.server_url().as_str(),
            "https://launcher.nevermoreland.ru/"
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
    fn accepts_ip_domain_and_full_endpoint_with_one_origin() {
        for base in [
            "https://89.125.214.122",
            "https://launcher.nevermoreland.ru",
        ] {
            for suffix in [
                "",
                "/",
                "/api/client-config",
                "/api/client-config?source=test#fragment",
            ] {
                let mut environment = values();
                environment.server_url = Some(format!(" {base}{suffix} "));
                let config = DesktopAppConfig::resolve(environment).unwrap();
                assert_eq!(config.backend.server_url().as_str(), format!("{base}/"));
                assert_eq!(
                    config.backend.client_config_url().as_str(),
                    format!("{base}/api/client-config")
                );
            }
        }
    }

    #[test]
    fn reads_external_config_and_falls_back_only_when_missing() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join(CONFIG_FILE);
        assert_eq!(
            read_server_url(&path).unwrap(),
            parse_server_url(DEFAULT_CONFIG).unwrap()
        );
        fs::write(
            &path,
            r#"{"server_url":"https://custom.example/api/client-config"}"#,
        )
        .unwrap();
        assert_eq!(
            read_server_url(&path).unwrap(),
            "https://custom.example/api/client-config"
        );
        for invalid in ["broken json", "{}", r#"{"server_url":42}"#] {
            fs::write(&path, invalid).unwrap();
            assert!(matches!(
                read_server_url(&path),
                Err(AppConfigError::InvalidConfig(_))
            ));
        }
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(matches!(
            read_server_url(&path),
            Err(AppConfigError::ReadConfig { .. })
        ));
    }

    #[test]
    fn rejects_empty_invalid_and_non_http_server_urls() {
        for url in [
            "",
            "  ",
            "not a URL",
            "file:///tmp/config",
            "ftp://example.com",
        ] {
            let mut environment = values();
            environment.server_url = Some(url.into());
            assert!(
                DesktopAppConfig::resolve(environment).is_err(),
                "accepted {url:?}"
            );
        }
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
    fn java_host_defaults_support_old_configs_and_explicit_overrides() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("launcher-config.json");
        let expected = "https://launcher.nevermoreland.ru/download/OpenJDK21U-jdk_x64_windows_hotspot_21.0.3_9.msi";
        assert_eq!(
            DesktopAppConfig::load_from(&path, values())
                .unwrap()
                .java_installer_url,
            expected
        );
        fs::write(&path, r#"{"server_url":"https://backend.example"}"#).unwrap();
        assert_eq!(
            DesktopAppConfig::load_from(&path, values())
                .unwrap()
                .java_installer_url,
            expected
        );
        fs::write(&path, r#"{"server_url":"https://backend.example","java21_msi_url":"https://cdn.example/java.msi"}"#).unwrap();
        assert_eq!(
            DesktopAppConfig::load_from(&path, values())
                .unwrap()
                .java_installer_url,
            "https://cdn.example/java.msi"
        );
        fs::write(
            &path,
            r#"{"server_url":"https://backend.example","java21_msi_url":""}"#,
        )
        .unwrap();
        assert_eq!(
            DesktopAppConfig::load_from(&path, values())
                .unwrap()
                .java_installer_url,
            ""
        );
        fs::write(&path, "broken json").unwrap();
        assert!(DesktopAppConfig::load_from(&path, values()).is_err());
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
