use std::{error::Error, fmt, time::Duration};

use serde::Deserialize;
use tokio::time::Instant;

use super::{AuthCancellation, AuthHttpError, send_auth_request};

const DEFAULT_SCOPE: &str = "XboxLive.signin offline_access";
const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;
const SLOW_DOWN_INCREMENT_SECONDS: u64 = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MicrosoftAuthConfig {
    pub client_id: String,
    pub scope: String,
    pub device_authorization_endpoint: reqwest::Url,
    pub token_endpoint: reqwest::Url,
}

impl MicrosoftAuthConfig {
    pub fn new(client_id: impl Into<String>) -> Result<Self, url::ParseError> {
        Ok(Self {
            client_id: client_id.into(),
            scope: DEFAULT_SCOPE.to_owned(),
            device_authorization_endpoint: reqwest::Url::parse(
                "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode",
            )?,
            token_endpoint: reqwest::Url::parse(
                "https://login.microsoftonline.com/consumers/oauth2/v2.0/token",
            )?,
        })
    }

    pub fn with_endpoints(
        client_id: impl Into<String>,
        device_authorization_endpoint: reqwest::Url,
        token_endpoint: reqwest::Url,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            scope: DEFAULT_SCOPE.to_owned(),
            device_authorization_endpoint,
            token_endpoint,
        }
    }
}

#[derive(Clone)]
pub struct DeviceLogin {
    device_code: String,
    pub verification_uri: String,
    pub user_code: String,
    pub message: Option<String>,
    pub expires_in: u64,
    pub interval: u64,
    issued_at: Instant,
}

impl fmt::Debug for DeviceLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceLogin")
            .field("device_code", &"<redacted>")
            .field("verification_uri", &self.verification_uri)
            .field("user_code", &self.user_code)
            .field("message", &self.message)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish()
    }
}

#[derive(Clone)]
pub struct MicrosoftTokenSet {
    access_token: String,
    refresh_token: Option<String>,
    pub expires_in: u64,
    pub token_type: String,
}

impl MicrosoftTokenSet {
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    pub fn refresh_token(&self) -> Option<&str> {
        self.refresh_token.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn test_token(access_token: &str, refresh_token: Option<&str>) -> Self {
        Self {
            access_token: access_token.to_owned(),
            refresh_token: refresh_token.map(str::to_owned),
            expires_in: 3_600,
            token_type: "Bearer".to_owned(),
        }
    }
}

impl fmt::Debug for MicrosoftTokenSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MicrosoftTokenSet")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_in", &self.expires_in)
            .field("token_type", &self.token_type)
            .finish()
    }
}

#[derive(Debug)]
pub enum MicrosoftAuthError {
    Http(AuthHttpError),
    HttpStatus {
        endpoint: reqwest::Url,
        status: reqwest::StatusCode,
    },
    InvalidJson(serde_json::Error),
    OAuth {
        code: String,
    },
    DeviceCodeExpired,
    AccessDenied,
    Cancelled,
}

impl fmt::Display for MicrosoftAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(source) => write!(formatter, "Microsoft authentication failed: {source}"),
            Self::HttpStatus { endpoint, status } => {
                write!(formatter, "Microsoft endpoint {endpoint} returned {status}")
            }
            Self::InvalidJson(_) => {
                formatter.write_str("Microsoft authentication returned invalid JSON")
            }
            Self::OAuth { code } => write!(formatter, "Microsoft OAuth error: {code}"),
            Self::DeviceCodeExpired => formatter.write_str("Microsoft device code expired"),
            Self::AccessDenied => formatter.write_str("Microsoft device login was denied"),
            Self::Cancelled => formatter.write_str("Microsoft device login was cancelled"),
        }
    }
}

impl Error for MicrosoftAuthError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Http(source) => Some(source),
            Self::InvalidJson(source) => Some(source),
            Self::HttpStatus { .. }
            | Self::OAuth { .. }
            | Self::DeviceCodeExpired
            | Self::AccessDenied
            | Self::Cancelled => None,
        }
    }
}

pub async fn begin_device_login(
    client: &reqwest::Client,
    config: &MicrosoftAuthConfig,
) -> Result<DeviceLogin, MicrosoftAuthError> {
    let response = send_auth_request(
        client
            .post(config.device_authorization_endpoint.clone())
            .form(&[
                ("client_id", config.client_id.as_str()),
                ("scope", config.scope.as_str()),
            ]),
        &config.device_authorization_endpoint,
    )
    .await
    .map_err(MicrosoftAuthError::Http)?;
    if !response.status.is_success() {
        return Err(MicrosoftAuthError::HttpStatus {
            endpoint: config.device_authorization_endpoint.clone(),
            status: response.status,
        });
    }
    let response: DeviceCodeResponse =
        serde_json::from_slice(&response.body).map_err(MicrosoftAuthError::InvalidJson)?;

    Ok(DeviceLogin {
        device_code: response.device_code,
        verification_uri: response.verification_uri,
        user_code: response.user_code,
        message: response.message,
        expires_in: response.expires_in,
        interval: response.interval,
        issued_at: Instant::now(),
    })
}

pub async fn complete_device_login(
    client: &reqwest::Client,
    config: &MicrosoftAuthConfig,
    login: &DeviceLogin,
    cancellation: &AuthCancellation,
) -> Result<MicrosoftTokenSet, MicrosoftAuthError> {
    let deadline = login.issued_at + Duration::from_secs(login.expires_in);
    let mut interval = Duration::from_secs(login.interval);

    loop {
        wait_for_next_poll(deadline, interval, cancellation).await?;
        if cancellation.is_cancelled() {
            return Err(MicrosoftAuthError::Cancelled);
        }

        let request = client.post(config.token_endpoint.clone()).form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", config.client_id.as_str()),
            ("device_code", login.device_code.as_str()),
        ]);
        let response = tokio::select! {
            () = cancellation.cancelled() => return Err(MicrosoftAuthError::Cancelled),
            () = tokio::time::sleep_until(deadline) => {
                return Err(MicrosoftAuthError::DeviceCodeExpired);
            }
            response = send_auth_request(request, &config.token_endpoint) => {
                response.map_err(MicrosoftAuthError::Http)?
            }
        };

        if response.status.is_success() {
            let token: TokenResponse =
                serde_json::from_slice(&response.body).map_err(MicrosoftAuthError::InvalidJson)?;
            return Ok(token.into_token_set(None));
        }

        let oauth: OAuthErrorResponse = match serde_json::from_slice(&response.body) {
            Ok(error) => error,
            Err(_) => {
                return Err(MicrosoftAuthError::HttpStatus {
                    endpoint: config.token_endpoint.clone(),
                    status: response.status,
                });
            }
        };
        match oauth.error.as_str() {
            "authorization_pending" => {}
            "slow_down" => interval += Duration::from_secs(SLOW_DOWN_INCREMENT_SECONDS),
            "expired_token" => return Err(MicrosoftAuthError::DeviceCodeExpired),
            "authorization_declined" | "access_denied" => {
                return Err(MicrosoftAuthError::AccessDenied);
            }
            _ => return Err(MicrosoftAuthError::OAuth { code: oauth.error }),
        }
    }
}

pub async fn refresh_microsoft_token(
    client: &reqwest::Client,
    config: &MicrosoftAuthConfig,
    refresh_token: &str,
) -> Result<MicrosoftTokenSet, MicrosoftAuthError> {
    let response = send_auth_request(
        client.post(config.token_endpoint.clone()).form(&[
            ("grant_type", "refresh_token"),
            ("client_id", config.client_id.as_str()),
            ("refresh_token", refresh_token),
            ("scope", config.scope.as_str()),
        ]),
        &config.token_endpoint,
    )
    .await
    .map_err(MicrosoftAuthError::Http)?;
    if !response.status.is_success() {
        return Err(parse_oauth_or_status(
            &config.token_endpoint,
            response.status,
            &response.body,
        ));
    }
    let token: TokenResponse =
        serde_json::from_slice(&response.body).map_err(MicrosoftAuthError::InvalidJson)?;
    Ok(token.into_token_set(Some(refresh_token)))
}

async fn wait_for_next_poll(
    deadline: Instant,
    interval: Duration,
    cancellation: &AuthCancellation,
) -> Result<(), MicrosoftAuthError> {
    let now = Instant::now();
    if now >= deadline {
        return Err(MicrosoftAuthError::DeviceCodeExpired);
    }
    let wake_at = std::cmp::min(now + interval, deadline);
    tokio::select! {
        () = cancellation.cancelled() => Err(MicrosoftAuthError::Cancelled),
        () = tokio::time::sleep_until(wake_at) => {
            if Instant::now() >= deadline {
                Err(MicrosoftAuthError::DeviceCodeExpired)
            } else {
                Ok(())
            }
        }
    }
}

fn parse_oauth_or_status(
    endpoint: &reqwest::Url,
    status: reqwest::StatusCode,
    body: &[u8],
) -> MicrosoftAuthError {
    match serde_json::from_slice::<OAuthErrorResponse>(body) {
        Ok(error) => MicrosoftAuthError::OAuth { code: error.error },
        Err(_) => MicrosoftAuthError::HttpStatus {
            endpoint: endpoint.clone(),
            status,
        },
    }
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    #[serde(default = "default_poll_interval")]
    interval: u64,
    #[serde(default)]
    message: Option<String>,
}

fn default_poll_interval() -> u64 {
    DEFAULT_POLL_INTERVAL_SECONDS
}

#[derive(Deserialize)]
struct TokenResponse {
    token_type: String,
    expires_in: u64,
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
}

impl TokenResponse {
    fn into_token_set(self, fallback_refresh_token: Option<&str>) -> MicrosoftTokenSet {
        MicrosoftTokenSet {
            access_token: self.access_token,
            refresh_token: self
                .refresh_token
                .or_else(|| fallback_refresh_token.map(str::to_owned)),
            expires_in: self.expires_in,
            token_type: self.token_type,
        }
    }
}

#[derive(Deserialize)]
struct OAuthErrorResponse {
    error: String,
}

#[cfg(test)]
mod tests {
    use std::{error::Error, time::Duration};

    use serde_json::json;

    use super::*;
    use crate::{
        build_http_client,
        http::test_support::{TestResponse, TestServer},
    };

    #[tokio::test]
    async fn parses_device_code_response() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route("/device", TestResponse::ok(device_response(900, 5)));

        let login = begin_device_login(&build_http_client()?, &config(&server)).await?;

        assert_eq!(login.verification_uri, "https://example.test/activate");
        assert_eq!(login.user_code, "ABCD-EFGH");
        assert_eq!(login.message.as_deref(), Some("Open the verification page"));
        assert_eq!(login.expires_in, 900);
        assert_eq!(login.interval, 5);
        Ok(())
    }

    #[tokio::test]
    async fn authorization_pending_causes_another_poll() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route("/device", TestResponse::ok(device_response(900, 0)));
        server.route(
            "/token",
            TestResponse::status(400)
                .with_body(json!({"error": "authorization_pending"}).to_string()),
        );
        let client = build_http_client()?;
        let config = config(&server);
        let login = begin_device_login(&client, &config).await?;
        let cancellation = AuthCancellation::default();
        let task = spawn_completion(&client, &config, &login, &cancellation);

        wait_for_requests(&server, 2).await?;
        cancellation.cancel();
        let error = task.await?.unwrap_err();

        assert!(matches!(error, MicrosoftAuthError::Cancelled));
        assert!(server.request_count("/token") >= 2);
        Ok(())
    }

    #[tokio::test]
    async fn slow_down_increases_polling_interval() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route("/device", TestResponse::ok(device_response(900, 0)));
        server.route(
            "/token",
            TestResponse::status(400).with_body(json!({"error": "slow_down"}).to_string()),
        );
        let client = build_http_client()?;
        let config = config(&server);
        let login = begin_device_login(&client, &config).await?;
        let cancellation = AuthCancellation::default();
        let task = spawn_completion(&client, &config, &login, &cancellation);

        wait_for_requests(&server, 1).await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(server.request_count("/token"), 1);
        cancellation.cancel();
        let error = task.await?.unwrap_err();

        assert!(matches!(error, MicrosoftAuthError::Cancelled));
        Ok(())
    }

    #[tokio::test]
    async fn completes_device_login_with_token_set() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route("/device", TestResponse::ok(device_response(900, 0)));
        server.route("/token", TestResponse::ok(token_response()));
        let client = build_http_client()?;
        let config = config(&server);
        let login = begin_device_login(&client, &config).await?;

        let token =
            complete_device_login(&client, &config, &login, &AuthCancellation::default()).await?;

        assert_eq!(token.access_token(), "microsoft-access-secret");
        assert_eq!(token.refresh_token(), Some("microsoft-refresh-secret"));
        assert_eq!(token.expires_in, 3_600);
        assert_eq!(token.token_type, "Bearer");
        Ok(())
    }

    #[tokio::test]
    async fn device_code_expires_without_polling() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route("/device", TestResponse::ok(device_response(0, 0)));
        let client = build_http_client()?;
        let config = config(&server);
        let login = begin_device_login(&client, &config).await?;

        let error = complete_device_login(&client, &config, &login, &AuthCancellation::default())
            .await
            .unwrap_err();

        assert!(matches!(error, MicrosoftAuthError::DeviceCodeExpired));
        assert_eq!(server.request_count("/token"), 0);
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_stops_polling_before_another_request() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route("/device", TestResponse::ok(device_response(900, 60)));
        server.route(
            "/token",
            TestResponse::status(400)
                .with_body(json!({"error": "authorization_pending"}).to_string()),
        );
        let client = build_http_client()?;
        let config = config(&server);
        let login = begin_device_login(&client, &config).await?;
        let cancellation = AuthCancellation::default();
        let task_client = client.clone();
        let task_config = config.clone();
        let task_login = login.clone();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            complete_device_login(&task_client, &task_config, &task_login, &task_cancellation).await
        });
        tokio::task::yield_now().await;

        cancellation.cancel();
        let error = tokio::time::timeout(Duration::from_secs(1), task)
            .await??
            .unwrap_err();

        assert!(matches!(error, MicrosoftAuthError::Cancelled));
        assert_eq!(server.request_count("/token"), 0);
        Ok(())
    }

    #[tokio::test]
    async fn refreshes_microsoft_token() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route(
            "/token",
            TestResponse::ok(
                json!({
                    "token_type": "Bearer",
                    "expires_in": 7200,
                    "access_token": "refreshed-access-secret"
                })
                .to_string(),
            ),
        );

        let token = refresh_microsoft_token(
            &build_http_client()?,
            &config(&server),
            "existing-refresh-secret",
        )
        .await?;

        assert_eq!(token.access_token(), "refreshed-access-secret");
        assert_eq!(token.refresh_token(), Some("existing-refresh-secret"));
        assert_eq!(token.expires_in, 7_200);
        Ok(())
    }

    fn config(server: &TestServer) -> MicrosoftAuthConfig {
        MicrosoftAuthConfig::with_endpoints(
            "test-client-id",
            server.url("device").unwrap(),
            server.url("token").unwrap(),
        )
    }

    fn device_response(expires_in: u64, interval: u64) -> String {
        json!({
            "device_code": "device-code-secret",
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://example.test/activate",
            "expires_in": expires_in,
            "interval": interval,
            "message": "Open the verification page"
        })
        .to_string()
    }

    fn token_response() -> String {
        json!({
            "token_type": "Bearer",
            "expires_in": 3600,
            "access_token": "microsoft-access-secret",
            "refresh_token": "microsoft-refresh-secret"
        })
        .to_string()
    }

    fn spawn_completion(
        client: &reqwest::Client,
        config: &MicrosoftAuthConfig,
        login: &DeviceLogin,
        cancellation: &AuthCancellation,
    ) -> tokio::task::JoinHandle<Result<MicrosoftTokenSet, MicrosoftAuthError>> {
        let client = client.clone();
        let config = config.clone();
        let login = login.clone();
        let cancellation = cancellation.clone();
        tokio::spawn(
            async move { complete_device_login(&client, &config, &login, &cancellation).await },
        )
    }

    async fn wait_for_requests(
        server: &TestServer,
        expected: usize,
    ) -> Result<(), tokio::time::error::Elapsed> {
        tokio::time::timeout(Duration::from_secs(1), async {
            while server.request_count("/token") < expected {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
    }
}
