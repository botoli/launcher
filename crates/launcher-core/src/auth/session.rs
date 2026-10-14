use std::{error::Error, fmt};

use super::{
    EntitlementError, MicrosoftTokenSet, MinecraftLoginError, MinecraftProfileError,
    MinecraftServicesConfig, XboxAuthConfig, XboxUserAuthError, XstsAuthError,
    authenticate_minecraft, authenticate_xbox_user, authenticate_xsts, check_minecraft_entitlement,
    fetch_minecraft_profile,
};

#[derive(Clone)]
pub struct MinecraftSession {
    pub username: String,
    pub uuid: String,
    access_token: String,
}

impl MinecraftSession {
    pub fn new(
        username: impl Into<String>,
        uuid: impl Into<String>,
        access_token: impl Into<String>,
    ) -> Self {
        Self {
            username: username.into(),
            uuid: uuid.into(),
            access_token: access_token.into(),
        }
    }

    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    pub fn user_type(&self) -> &'static str {
        "msa"
    }
}

impl fmt::Debug for MinecraftSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MinecraftSession")
            .field("username", &self.username)
            .field("uuid", &self.uuid)
            .field("access_token", &"<redacted>")
            .finish()
    }
}

#[derive(Debug)]
pub enum MinecraftSessionError {
    XboxUser(XboxUserAuthError),
    Xsts(XstsAuthError),
    MinecraftLogin(MinecraftLoginError),
    Entitlement(EntitlementError),
    Profile(MinecraftProfileError),
}

impl fmt::Display for MinecraftSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::XboxUser(source) => {
                write!(formatter, "Xbox user authentication failed: {source}")
            }
            Self::Xsts(source) => write!(formatter, "XSTS authentication failed: {source}"),
            Self::MinecraftLogin(source) => {
                write!(formatter, "Minecraft Services login failed: {source}")
            }
            Self::Entitlement(source) => write!(formatter, "ownership check failed: {source}"),
            Self::Profile(source) => write!(formatter, "profile request failed: {source}"),
        }
    }
}

impl Error for MinecraftSessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::XboxUser(source) => Some(source),
            Self::Xsts(source) => Some(source),
            Self::MinecraftLogin(source) => Some(source),
            Self::Entitlement(source) => Some(source),
            Self::Profile(source) => Some(source),
        }
    }
}

pub async fn create_minecraft_session(
    client: &reqwest::Client,
    xbox_config: &XboxAuthConfig,
    minecraft_config: &MinecraftServicesConfig,
    microsoft_token: &MicrosoftTokenSet,
) -> Result<MinecraftSession, MinecraftSessionError> {
    let xbox_token = authenticate_xbox_user(client, xbox_config, microsoft_token)
        .await
        .map_err(MinecraftSessionError::XboxUser)?;
    let xsts_token = authenticate_xsts(client, xbox_config, &xbox_token)
        .await
        .map_err(MinecraftSessionError::Xsts)?;
    let minecraft_token = authenticate_minecraft(client, minecraft_config, &xsts_token)
        .await
        .map_err(MinecraftSessionError::MinecraftLogin)?;
    check_minecraft_entitlement(client, minecraft_config, &minecraft_token)
        .await
        .map_err(MinecraftSessionError::Entitlement)?;
    let profile = fetch_minecraft_profile(client, minecraft_config, &minecraft_token)
        .await
        .map_err(MinecraftSessionError::Profile)?;

    Ok(MinecraftSession::new(
        profile.name,
        profile.id,
        minecraft_token.access_token(),
    ))
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use serde_json::json;

    use super::*;
    use crate::{
        AuthCancellation, MicrosoftAuthConfig, XstsToken, begin_device_login, build_http_client,
        complete_device_login,
        http::test_support::{TestResponse, TestServer},
    };

    #[tokio::test]
    async fn full_device_code_to_minecraft_session_flow() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        route_full_flow(&server);
        let client = build_http_client()?;
        let microsoft_config = MicrosoftAuthConfig::with_endpoints(
            "test-client-id",
            server.url("device")?,
            server.url("token")?,
        );
        let xbox_config =
            XboxAuthConfig::with_endpoints(server.url("xbox-user")?, server.url("xsts")?);
        let minecraft_config = MinecraftServicesConfig::with_endpoints(
            server.url("minecraft-login")?,
            server.url("entitlements")?,
            server.url("profile")?,
        );

        let login = begin_device_login(&client, &microsoft_config).await?;
        let microsoft_token = complete_device_login(
            &client,
            &microsoft_config,
            &login,
            &AuthCancellation::default(),
        )
        .await?;
        let session =
            create_minecraft_session(&client, &xbox_config, &minecraft_config, &microsoft_token)
                .await?;

        assert_eq!(session.username, "TestPlayer");
        assert_eq!(session.uuid, "0123456789abcdef0123456789abcdef");
        assert_eq!(session.access_token(), "minecraft-access-secret");
        assert_eq!(session.user_type(), "msa");
        for path in [
            "/device",
            "/token",
            "/xbox-user",
            "/xsts",
            "/minecraft-login",
            "/entitlements",
            "/profile",
        ] {
            assert_eq!(server.request_count(path), 1, "request count for {path}");
        }
        Ok(())
    }

    #[test]
    fn secret_tokens_are_redacted_from_debug_output() {
        let microsoft = MicrosoftTokenSet::test_token(
            "microsoft-access-secret",
            Some("microsoft-refresh-secret"),
        );
        let xsts = XstsToken::test_token("xsts-secret", "user-hash");
        let session = MinecraftSession {
            username: "TestPlayer".to_owned(),
            uuid: "test-uuid".to_owned(),
            access_token: "minecraft-access-secret".to_owned(),
        };
        let output = format!("{microsoft:?} {xsts:?} {session:?}");

        for secret in [
            "microsoft-access-secret",
            "microsoft-refresh-secret",
            "xsts-secret",
            "minecraft-access-secret",
        ] {
            assert!(!output.contains(secret));
        }
        assert!(output.contains("<redacted>"));
    }

    fn route_full_flow(server: &TestServer) {
        server.route(
            "/device",
            TestResponse::ok(
                json!({
                    "device_code": "device-code-secret",
                    "user_code": "ABCD-EFGH",
                    "verification_uri": "https://example.test/activate",
                    "expires_in": 900,
                    "interval": 0
                })
                .to_string(),
            ),
        );
        server.route(
            "/token",
            TestResponse::ok(
                json!({
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "access_token": "microsoft-access-secret",
                    "refresh_token": "microsoft-refresh-secret"
                })
                .to_string(),
            ),
        );
        server.route(
            "/xbox-user",
            TestResponse::ok(xbox_response("xbox-user-secret")),
        );
        server.route("/xsts", TestResponse::ok(xbox_response("xsts-secret")));
        server.route(
            "/minecraft-login",
            TestResponse::ok(
                json!({
                    "access_token": "minecraft-access-secret",
                    "expires_in": 86400,
                    "token_type": "Bearer"
                })
                .to_string(),
            ),
        );
        server.route(
            "/entitlements",
            TestResponse::ok(
                json!({
                    "items": [{"name": "game_minecraft", "signature": "ignored"}]
                })
                .to_string(),
            ),
        );
        server.route(
            "/profile",
            TestResponse::ok(
                json!({
                    "id": "0123456789abcdef0123456789abcdef",
                    "name": "TestPlayer"
                })
                .to_string(),
            ),
        );
    }

    fn xbox_response(token: &str) -> String {
        json!({
            "Token": token,
            "DisplayClaims": {
                "xui": [{"uhs": "user-hash"}]
            }
        })
        .to_string()
    }
}
