use std::{error::Error, fmt};

use serde::Deserialize;

use crate::http::{HttpUrlError, validate_http_url};

pub const MAX_CLIENT_CONFIG_SIZE: usize = 1024 * 1024;
const CLIENT_CONFIG_PATH: &str = "/api/client-config";

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ClientServer {
    pub name: String,
    pub address: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClientConfig {
    pub version: String,
    pub loader: String,
    pub loader_version: String,
    pub java_version: u32,
    pub min_memory: u32,
    pub max_memory: u32,
    pub jvm_args: Vec<String>,
    pub servers: Vec<ClientServer>,
    pub mods_hash: String,
    #[serde(rename = "modsURL")]
    pub mods_url: reqwest::Url,
    #[serde(rename = "serverIP")]
    pub server_ip: String,
}

impl ClientConfig {
    pub fn server_address(&self) -> Option<&str> {
        let server_ip = self.server_ip.trim();
        if !server_ip.is_empty() {
            return Some(server_ip);
        }

        self.servers
            .iter()
            .map(|server| server.address.trim())
            .find(|address| !address.is_empty())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LauncherBackendConfig {
    server_url: reqwest::Url,
    client_config_url: reqwest::Url,
}

impl LauncherBackendConfig {
    pub fn new(server_url: reqwest::Url) -> Result<Self, LauncherBackendConfigError> {
        validate_http_url(&server_url).map_err(LauncherBackendConfigError::ServerUrl)?;
        let client_config_url = server_url
            .join(CLIENT_CONFIG_PATH)
            .map_err(LauncherBackendConfigError::ClientConfigUrl)?;

        Ok(Self {
            server_url,
            client_config_url,
        })
    }

    pub fn server_url(&self) -> &reqwest::Url {
        &self.server_url
    }

    pub fn client_config_url(&self) -> &reqwest::Url {
        &self.client_config_url
    }
}

#[derive(Debug)]
pub enum LauncherBackendConfigError {
    ServerUrl(HttpUrlError),
    ClientConfigUrl(url::ParseError),
}

impl fmt::Display for LauncherBackendConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ServerUrl(source) => write!(formatter, "invalid launcher server URL: {source}"),
            Self::ClientConfigUrl(_) => {
                formatter.write_str("failed to construct the client config URL")
            }
        }
    }
}

impl Error for LauncherBackendConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ServerUrl(source) => Some(source),
            Self::ClientConfigUrl(source) => Some(source),
        }
    }
}

pub async fn fetch_client_config(
    client: &reqwest::Client,
    url: &reqwest::Url,
) -> Result<ClientConfig, ClientConfigFetchError> {
    let mut response = client
        .get(url.clone())
        .timeout(crate::http::METADATA_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|source| ClientConfigFetchError::Request {
            url: url.clone(),
            source,
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(ClientConfigFetchError::HttpStatus {
            url: url.clone(),
            status,
        });
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CLIENT_CONFIG_SIZE as u64)
    {
        return Err(ClientConfigFetchError::TooLarge {
            limit: MAX_CLIENT_CONFIG_SIZE,
        });
    }

    let mut body = Vec::new();
    while let Some(chunk) =
        response
            .chunk()
            .await
            .map_err(|source| ClientConfigFetchError::Request {
                url: url.clone(),
                source,
            })?
    {
        let next_size =
            body.len()
                .checked_add(chunk.len())
                .ok_or(ClientConfigFetchError::TooLarge {
                    limit: MAX_CLIENT_CONFIG_SIZE,
                })?;
        if next_size > MAX_CLIENT_CONFIG_SIZE {
            return Err(ClientConfigFetchError::TooLarge {
                limit: MAX_CLIENT_CONFIG_SIZE,
            });
        }
        body.extend_from_slice(&chunk);
    }

    let config: ClientConfig =
        serde_json::from_slice(&body).map_err(ClientConfigFetchError::InvalidJson)?;
    validate_http_url(&config.mods_url).map_err(ClientConfigFetchError::InvalidModsUrl)?;
    Ok(config)
}

#[derive(Debug)]
pub enum ClientConfigFetchError {
    Request {
        url: reqwest::Url,
        source: reqwest::Error,
    },
    HttpStatus {
        url: reqwest::Url,
        status: reqwest::StatusCode,
    },
    TooLarge {
        limit: usize,
    },
    InvalidJson(serde_json::Error),
    InvalidModsUrl(HttpUrlError),
}

impl fmt::Display for ClientConfigFetchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request { url, .. } => {
                write!(formatter, "failed to fetch client config from {url}")
            }
            Self::HttpStatus { url, status } => {
                write!(
                    formatter,
                    "client config request for {url} returned {status}"
                )
            }
            Self::TooLarge { limit } => {
                write!(
                    formatter,
                    "client config exceeds the {limit} byte size limit"
                )
            }
            Self::InvalidJson(_) => formatter.write_str("client config contains invalid JSON"),
            Self::InvalidModsUrl(source) => {
                write!(
                    formatter,
                    "client config contains an invalid mods URL: {source}"
                )
            }
        }
    }
}

impl Error for ClientConfigFetchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Request { source, .. } => Some(source),
            Self::InvalidJson(source) => Some(source),
            Self::InvalidModsUrl(source) => Some(source),
            Self::HttpStatus { .. } | Self::TooLarge { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use crate::{
        build_http_client,
        http::test_support::{TestResponse, TestServer},
    };

    use super::*;

    #[tokio::test]
    async fn parses_client_config() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let mods_url = server.url("mods.zip")?;
        server.route(
            "/api/client-config",
            TestResponse::ok(config_json(mods_url.as_str(), "server.example:25565")),
        );

        let config =
            fetch_client_config(&build_http_client()?, &server.url("api/client-config")?).await?;

        assert_eq!(config.version, "1.21.1");
        assert_eq!(config.loader, "neoforge");
        assert_eq!(config.loader_version, "21.1.248");
        assert_eq!(config.java_version, 21);
        assert_eq!(config.min_memory, 2048);
        assert_eq!(config.max_memory, 4096);
        assert_eq!(config.jvm_args, ["-Dfile.encoding=UTF-8"]);
        assert_eq!(config.mods_hash, "hash-1");
        assert_eq!(config.mods_url, mods_url);
        assert_eq!(config.server_address(), Some("server.example:25565"));
        Ok(())
    }

    #[tokio::test]
    async fn reports_config_404() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let error = fetch_client_config(&build_http_client()?, &server.url("api/client-config")?)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ClientConfigFetchError::HttpStatus { status, .. }
                if status == reqwest::StatusCode::NOT_FOUND
        ));
        Ok(())
    }

    #[tokio::test]
    async fn reports_invalid_json() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route("/api/client-config", TestResponse::ok("{invalid"));

        let error = fetch_client_config(&build_http_client()?, &server.url("api/client-config")?)
            .await
            .unwrap_err();

        assert!(matches!(error, ClientConfigFetchError::InvalidJson(_)));
        Ok(())
    }

    #[test]
    fn falls_back_to_first_server_address() {
        let mut config: ClientConfig =
            serde_json::from_str(&config_json("https://launcher.example/mods.zip", "")).unwrap();
        config.servers[0].address = "fallback.example:25565".to_owned();

        assert_eq!(config.server_address(), Some("fallback.example:25565"));
    }

    pub(crate) fn config_json(mods_url: &str, server_ip: &str) -> String {
        serde_json::json!({
            "version": "1.21.1",
            "loader": "neoforge",
            "loaderVersion": "21.1.248",
            "javaVersion": 21,
            "minMemory": 2048,
            "maxMemory": 4096,
            "jvmArgs": ["-Dfile.encoding=UTF-8"],
            "servers": [{"name": "MinecraftServer", "address": "fallback:25565"}],
            "modsHash": "hash-1",
            "modsURL": mods_url,
            "launcherServer": "https://ignored.example",
            "serverIP": server_ip
        })
        .to_string()
    }
}
