use std::{error::Error, fmt};

use serde::{Deserialize, Serialize};

use super::{AuthHttpError, XstsToken, send_auth_request};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MinecraftServicesConfig {
    pub login_endpoint: reqwest::Url,
    pub entitlements_endpoint: reqwest::Url,
    pub profile_endpoint: reqwest::Url,
}

impl MinecraftServicesConfig {
    pub fn new() -> Result<Self, url::ParseError> {
        Ok(Self {
            login_endpoint: reqwest::Url::parse(
                "https://api.minecraftservices.com/authentication/login_with_xbox",
            )?,
            entitlements_endpoint: reqwest::Url::parse(
                "https://api.minecraftservices.com/entitlements/mcstore",
            )?,
            profile_endpoint: reqwest::Url::parse(
                "https://api.minecraftservices.com/minecraft/profile",
            )?,
        })
    }

    pub fn with_endpoints(
        login_endpoint: reqwest::Url,
        entitlements_endpoint: reqwest::Url,
        profile_endpoint: reqwest::Url,
    ) -> Self {
        Self {
            login_endpoint,
            entitlements_endpoint,
            profile_endpoint,
        }
    }
}

#[derive(Clone)]
pub struct MinecraftAccessToken {
    access_token: String,
    pub expires_in: u64,
    pub token_type: String,
}

impl MinecraftAccessToken {
    pub fn access_token(&self) -> &str {
        &self.access_token
    }
}

impl fmt::Debug for MinecraftAccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MinecraftAccessToken")
            .field("access_token", &"<redacted>")
            .field("expires_in", &self.expires_in)
            .field("token_type", &self.token_type)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct MinecraftEntitlements {
    #[serde(default)]
    pub items: Vec<MinecraftEntitlement>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct MinecraftEntitlement {
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct MinecraftProfile {
    pub id: String,
    pub name: String,
}

#[derive(Debug)]
pub enum MinecraftLoginError {
    Http(AuthHttpError),
    HttpStatus(reqwest::StatusCode),
    InvalidJson(serde_json::Error),
}

impl fmt::Display for MinecraftLoginError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(source) => write!(formatter, "Minecraft Services login failed: {source}"),
            Self::HttpStatus(status) => {
                write!(formatter, "Minecraft Services login returned {status}")
            }
            Self::InvalidJson(_) => {
                formatter.write_str("Minecraft Services login returned invalid JSON")
            }
        }
    }
}

impl Error for MinecraftLoginError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Http(source) => Some(source),
            Self::InvalidJson(source) => Some(source),
            Self::HttpStatus(_) => None,
        }
    }
}

#[derive(Debug)]
pub enum EntitlementError {
    Http(AuthHttpError),
    HttpStatus(reqwest::StatusCode),
    InvalidJson(serde_json::Error),
    NoJavaEntitlement,
}

impl fmt::Display for EntitlementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(source) => write!(formatter, "Minecraft entitlement check failed: {source}"),
            Self::HttpStatus(status) => {
                write!(
                    formatter,
                    "Minecraft entitlement endpoint returned {status}"
                )
            }
            Self::InvalidJson(_) => {
                formatter.write_str("Minecraft entitlement endpoint returned invalid JSON")
            }
            Self::NoJavaEntitlement => {
                formatter.write_str("Microsoft account does not own Minecraft Java Edition")
            }
        }
    }
}

impl Error for EntitlementError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Http(source) => Some(source),
            Self::InvalidJson(source) => Some(source),
            Self::HttpStatus(_) | Self::NoJavaEntitlement => None,
        }
    }
}

#[derive(Debug)]
pub enum MinecraftProfileError {
    Http(AuthHttpError),
    HttpStatus(reqwest::StatusCode),
    InvalidJson(serde_json::Error),
}

impl fmt::Display for MinecraftProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(source) => write!(formatter, "Minecraft profile request failed: {source}"),
            Self::HttpStatus(status) => {
                write!(formatter, "Minecraft profile endpoint returned {status}")
            }
            Self::InvalidJson(_) => {
                formatter.write_str("Minecraft profile endpoint returned invalid JSON")
            }
        }
    }
}

impl Error for MinecraftProfileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Http(source) => Some(source),
            Self::InvalidJson(source) => Some(source),
            Self::HttpStatus(_) => None,
        }
    }
}

pub async fn authenticate_minecraft(
    client: &reqwest::Client,
    config: &MinecraftServicesConfig,
    xsts_token: &XstsToken,
) -> Result<MinecraftAccessToken, MinecraftLoginError> {
    let request = MinecraftLoginRequest {
        identity_token: format!("XBL3.0 x={};{}", xsts_token.user_hash(), xsts_token.token()),
    };
    let response = send_auth_request(
        client.post(config.login_endpoint.clone()).json(&request),
        &config.login_endpoint,
    )
    .await
    .map_err(MinecraftLoginError::Http)?;
    if !response.status.is_success() {
        return Err(MinecraftLoginError::HttpStatus(response.status));
    }
    let response: MinecraftLoginResponse =
        serde_json::from_slice(&response.body).map_err(MinecraftLoginError::InvalidJson)?;

    Ok(MinecraftAccessToken {
        access_token: response.access_token,
        expires_in: response.expires_in,
        token_type: response.token_type,
    })
}

pub async fn check_minecraft_entitlement(
    client: &reqwest::Client,
    config: &MinecraftServicesConfig,
    token: &MinecraftAccessToken,
) -> Result<MinecraftEntitlements, EntitlementError> {
    let response = send_auth_request(
        client
            .get(config.entitlements_endpoint.clone())
            .bearer_auth(token.access_token()),
        &config.entitlements_endpoint,
    )
    .await
    .map_err(EntitlementError::Http)?;
    if !response.status.is_success() {
        return Err(EntitlementError::HttpStatus(response.status));
    }
    let entitlements: MinecraftEntitlements =
        serde_json::from_slice(&response.body).map_err(EntitlementError::InvalidJson)?;
    if entitlements.items.is_empty() {
        return Err(EntitlementError::NoJavaEntitlement);
    }
    Ok(entitlements)
}

pub async fn fetch_minecraft_profile(
    client: &reqwest::Client,
    config: &MinecraftServicesConfig,
    token: &MinecraftAccessToken,
) -> Result<MinecraftProfile, MinecraftProfileError> {
    let response = send_auth_request(
        client
            .get(config.profile_endpoint.clone())
            .bearer_auth(token.access_token()),
        &config.profile_endpoint,
    )
    .await
    .map_err(MinecraftProfileError::Http)?;
    if !response.status.is_success() {
        return Err(MinecraftProfileError::HttpStatus(response.status));
    }
    serde_json::from_slice(&response.body).map_err(MinecraftProfileError::InvalidJson)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MinecraftLoginRequest {
    identity_token: String,
}

#[derive(Deserialize)]
struct MinecraftLoginResponse {
    access_token: String,
    expires_in: u64,
    #[serde(default = "default_token_type")]
    token_type: String,
}

fn default_token_type() -> String {
    "Bearer".to_owned()
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use serde_json::json;

    use super::*;
    use crate::{
        build_http_client,
        http::test_support::{TestResponse, TestServer},
    };

    #[tokio::test]
    async fn logs_into_minecraft_services() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route(
            "/login",
            TestResponse::ok(
                json!({
                    "access_token": "minecraft-access-secret",
                    "expires_in": 86400,
                    "token_type": "Bearer"
                })
                .to_string(),
            ),
        );

        let token = authenticate_minecraft(
            &build_http_client()?,
            &config(&server),
            &XstsToken::test_token("xsts-secret", "user-hash"),
        )
        .await?;

        assert_eq!(token.access_token(), "minecraft-access-secret");
        assert_eq!(token.expires_in, 86_400);
        assert!(!format!("{token:?}").contains("minecraft-access-secret"));
        Ok(())
    }

    #[tokio::test]
    async fn accepts_nonempty_entitlements() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route(
            "/entitlements",
            TestResponse::ok(
                json!({
                    "items": [{"name": "game_minecraft", "signature": "ignored"}]
                })
                .to_string(),
            ),
        );

        let entitlements = check_minecraft_entitlement(
            &build_http_client()?,
            &config(&server),
            &minecraft_token(),
        )
        .await?;

        assert_eq!(entitlements.items[0].name, "game_minecraft");
        Ok(())
    }

    #[tokio::test]
    async fn missing_entitlement_has_distinct_error() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route(
            "/entitlements",
            TestResponse::ok(json!({"items": []}).to_string()),
        );

        let error = check_minecraft_entitlement(
            &build_http_client()?,
            &config(&server),
            &minecraft_token(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, EntitlementError::NoJavaEntitlement));
        Ok(())
    }

    #[tokio::test]
    async fn parses_minecraft_profile() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route(
            "/profile",
            TestResponse::ok(
                json!({
                    "id": "0123456789abcdef0123456789abcdef",
                    "name": "TestPlayer",
                    "skins": []
                })
                .to_string(),
            ),
        );

        let profile =
            fetch_minecraft_profile(&build_http_client()?, &config(&server), &minecraft_token())
                .await?;

        assert_eq!(profile.name, "TestPlayer");
        assert_eq!(profile.id, "0123456789abcdef0123456789abcdef");
        Ok(())
    }

    fn config(server: &TestServer) -> MinecraftServicesConfig {
        MinecraftServicesConfig::with_endpoints(
            server.url("login").unwrap(),
            server.url("entitlements").unwrap(),
            server.url("profile").unwrap(),
        )
    }

    fn minecraft_token() -> MinecraftAccessToken {
        MinecraftAccessToken {
            access_token: "minecraft-access-secret".to_owned(),
            expires_in: 86_400,
            token_type: "Bearer".to_owned(),
        }
    }
}
