use std::{error::Error, fmt};

use serde::{Deserialize, Serialize};

use super::{AuthHttpError, MicrosoftTokenSet, send_auth_request};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XboxAuthConfig {
    pub user_auth_endpoint: reqwest::Url,
    pub xsts_endpoint: reqwest::Url,
    pub relying_party: String,
}

impl XboxAuthConfig {
    pub fn new() -> Result<Self, url::ParseError> {
        Ok(Self {
            user_auth_endpoint: reqwest::Url::parse(
                "https://user.auth.xboxlive.com/user/authenticate",
            )?,
            xsts_endpoint: reqwest::Url::parse("https://xsts.auth.xboxlive.com/xsts/authorize")?,
            relying_party: "rp://api.minecraftservices.com/".to_owned(),
        })
    }

    pub fn with_endpoints(user_auth_endpoint: reqwest::Url, xsts_endpoint: reqwest::Url) -> Self {
        Self {
            user_auth_endpoint,
            xsts_endpoint,
            relying_party: "rp://api.minecraftservices.com/".to_owned(),
        }
    }
}

#[derive(Clone)]
pub struct XboxUserToken {
    token: String,
    user_hash: String,
}

impl XboxUserToken {
    pub fn user_hash(&self) -> &str {
        &self.user_hash
    }
}

impl fmt::Debug for XboxUserToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("XboxUserToken")
            .field("token", &"<redacted>")
            .field("user_hash", &self.user_hash)
            .finish()
    }
}

#[derive(Clone)]
pub struct XstsToken {
    token: String,
    user_hash: String,
}

impl XstsToken {
    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn user_hash(&self) -> &str {
        &self.user_hash
    }

    #[cfg(test)]
    pub(crate) fn test_token(token: &str, user_hash: &str) -> Self {
        Self {
            token: token.to_owned(),
            user_hash: user_hash.to_owned(),
        }
    }
}

impl fmt::Debug for XstsToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("XstsToken")
            .field("token", &"<redacted>")
            .field("user_hash", &self.user_hash)
            .finish()
    }
}

#[derive(Debug)]
pub enum XboxUserAuthError {
    Http(AuthHttpError),
    HttpStatus(reqwest::StatusCode),
    InvalidJson(serde_json::Error),
    MissingUserHash,
}

impl fmt::Display for XboxUserAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(source) => write!(formatter, "Xbox user authentication failed: {source}"),
            Self::HttpStatus(status) => {
                write!(formatter, "Xbox user authentication returned {status}")
            }
            Self::InvalidJson(_) => {
                formatter.write_str("Xbox user authentication returned invalid JSON")
            }
            Self::MissingUserHash => {
                formatter.write_str("Xbox user authentication response has no user hash")
            }
        }
    }
}

impl Error for XboxUserAuthError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Http(source) => Some(source),
            Self::InvalidJson(source) => Some(source),
            Self::HttpStatus(_) | Self::MissingUserHash => None,
        }
    }
}

#[derive(Debug)]
pub enum XstsAuthError {
    Http(AuthHttpError),
    Rejected {
        status: reqwest::StatusCode,
        xerr: Option<u64>,
    },
    InvalidJson(serde_json::Error),
    MissingUserHash,
}

impl fmt::Display for XstsAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(source) => write!(formatter, "XSTS authentication failed: {source}"),
            Self::Rejected { status, xerr } => {
                write!(formatter, "XSTS authentication was rejected with {status}")?;
                if let Some(xerr) = xerr {
                    write!(formatter, " (XErr {xerr})")?;
                }
                Ok(())
            }
            Self::InvalidJson(_) => formatter.write_str("XSTS returned invalid JSON"),
            Self::MissingUserHash => formatter.write_str("XSTS response has no user hash"),
        }
    }
}

impl Error for XstsAuthError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Http(source) => Some(source),
            Self::InvalidJson(source) => Some(source),
            Self::Rejected { .. } | Self::MissingUserHash => None,
        }
    }
}

pub async fn authenticate_xbox_user(
    client: &reqwest::Client,
    config: &XboxAuthConfig,
    microsoft_token: &MicrosoftTokenSet,
) -> Result<XboxUserToken, XboxUserAuthError> {
    let request = XboxUserAuthRequest {
        properties: XboxUserAuthProperties {
            auth_method: "RPS",
            site_name: "user.auth.xboxlive.com",
            rps_ticket: format!("d={}", microsoft_token.access_token()),
        },
        relying_party: "http://auth.xboxlive.com",
        token_type: "JWT",
    };
    let response = send_auth_request(
        client
            .post(config.user_auth_endpoint.clone())
            .json(&request),
        &config.user_auth_endpoint,
    )
    .await
    .map_err(XboxUserAuthError::Http)?;
    if !response.status.is_success() {
        return Err(XboxUserAuthError::HttpStatus(response.status));
    }
    let response: XboxTokenResponse =
        serde_json::from_slice(&response.body).map_err(XboxUserAuthError::InvalidJson)?;
    let user_hash = response
        .user_hash()
        .ok_or(XboxUserAuthError::MissingUserHash)?;

    Ok(XboxUserToken {
        token: response.token,
        user_hash,
    })
}

pub async fn authenticate_xsts(
    client: &reqwest::Client,
    config: &XboxAuthConfig,
    user_token: &XboxUserToken,
) -> Result<XstsToken, XstsAuthError> {
    let request = XstsAuthRequest {
        properties: XstsProperties {
            sandbox_id: "RETAIL",
            user_tokens: std::slice::from_ref(&user_token.token),
        },
        relying_party: &config.relying_party,
        token_type: "JWT",
    };
    let response = send_auth_request(
        client.post(config.xsts_endpoint.clone()).json(&request),
        &config.xsts_endpoint,
    )
    .await
    .map_err(XstsAuthError::Http)?;
    if !response.status.is_success() {
        let xerr = serde_json::from_slice::<XstsFailureResponse>(&response.body)
            .ok()
            .and_then(|failure| failure.xerr);
        return Err(XstsAuthError::Rejected {
            status: response.status,
            xerr,
        });
    }
    let response: XboxTokenResponse =
        serde_json::from_slice(&response.body).map_err(XstsAuthError::InvalidJson)?;
    let user_hash = response.user_hash().ok_or(XstsAuthError::MissingUserHash)?;

    Ok(XstsToken {
        token: response.token,
        user_hash,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct XboxUserAuthRequest<'a> {
    properties: XboxUserAuthProperties<'a>,
    relying_party: &'a str,
    token_type: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct XboxUserAuthProperties<'a> {
    auth_method: &'a str,
    site_name: &'a str,
    rps_ticket: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct XstsAuthRequest<'a> {
    properties: XstsProperties<'a>,
    relying_party: &'a str,
    token_type: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct XstsProperties<'a> {
    sandbox_id: &'a str,
    user_tokens: &'a [String],
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct XboxTokenResponse {
    token: String,
    display_claims: DisplayClaims,
}

impl XboxTokenResponse {
    fn user_hash(&self) -> Option<String> {
        self.display_claims
            .xui
            .first()
            .map(|claim| claim.user_hash().to_owned())
    }
}

#[derive(Deserialize)]
struct DisplayClaims {
    xui: Vec<XuiClaim>,
}

#[derive(Deserialize)]
struct XuiClaim {
    uhs: String,
}

impl XuiClaim {
    fn user_hash(&self) -> &str {
        &self.uhs
    }
}

#[derive(Deserialize)]
struct XstsFailureResponse {
    #[serde(rename = "XErr")]
    xerr: Option<u64>,
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
    async fn authenticates_xbox_user() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route("/user", TestResponse::ok(xbox_token_response("user-token")));
        let config = config(&server);

        let token = authenticate_xbox_user(
            &build_http_client()?,
            &config,
            &MicrosoftTokenSet::test_token("microsoft-secret", None),
        )
        .await?;

        assert_eq!(token.user_hash(), "user-hash");
        assert_eq!(server.request_count("/user"), 1);
        assert!(!format!("{token:?}").contains("user-token"));
        Ok(())
    }

    #[tokio::test]
    async fn authenticates_xsts() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route("/xsts", TestResponse::ok(xbox_token_response("xsts-token")));
        let config = config(&server);
        let user_token = XboxUserToken {
            token: "user-token".to_owned(),
            user_hash: "user-hash".to_owned(),
        };

        let token = authenticate_xsts(&build_http_client()?, &config, &user_token).await?;

        assert_eq!(token.token(), "xsts-token");
        assert_eq!(token.user_hash(), "user-hash");
        Ok(())
    }

    #[tokio::test]
    async fn xsts_failure_is_typed_without_response_body() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route(
            "/xsts",
            TestResponse::status(401)
                .with_body(json!({"XErr": 2148916233_u64, "secret": "do-not-log"}).to_string()),
        );
        let config = config(&server);
        let user_token = XboxUserToken {
            token: "user-token".to_owned(),
            user_hash: "user-hash".to_owned(),
        };

        let error = authenticate_xsts(&build_http_client()?, &config, &user_token)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            XstsAuthError::Rejected {
                status: reqwest::StatusCode::UNAUTHORIZED,
                xerr: Some(2_148_916_233)
            }
        ));
        assert!(!format!("{error:?}").contains("do-not-log"));
        Ok(())
    }

    fn config(server: &TestServer) -> XboxAuthConfig {
        XboxAuthConfig::with_endpoints(server.url("user").unwrap(), server.url("xsts").unwrap())
    }

    fn xbox_token_response(token: &str) -> String {
        json!({
            "Token": token,
            "DisplayClaims": {
                "xui": [{"uhs": "user-hash"}]
            }
        })
        .to_string()
    }
}
