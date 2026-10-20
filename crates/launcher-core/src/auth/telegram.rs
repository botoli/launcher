use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

#[derive(Clone, Deserialize)]
pub struct TelegramChallenge {
    pub id: String,
    pub poll_secret: String,
    pub code: String,
    pub expires_at: u64,
    pub url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TelegramUser {
    pub login: String,
    pub nickname: String,
    #[serde(rename = "registrationStatus")]
    pub registration_status: String,
}

// Deliberately no Debug: credentials must never enter launcher diagnostics.
#[derive(Clone, Deserialize, Serialize)]
pub struct TelegramSession {
    pub token: String,
    pub expires_at: u64,
    pub user: TelegramUser,
}

#[derive(Debug, thiserror::Error)]
pub enum TelegramAuthError {
    #[error("Сессия истекла или доступ заблокирован. Войдите снова через Telegram.")]
    Unauthorized,
    #[error("Сервис входа недоступен. Проверьте подключение и повторите попытку.")]
    Network,
    #[error("Сервис входа вернул некорректный ответ.")]
    InvalidResponse,
    #[error(
        "По этому адресу отсутствует Telegram API (HTTP 404). Проверьте адрес сервера или обновите backend."
    )]
    ApiUnavailable,
    #[error("Telegram-бот ещё не подключён к серверу.")]
    NotConfigured,
}

#[derive(Clone)]
pub struct TelegramAuthClient {
    client: reqwest::Client,
    base: reqwest::Url,
}

impl TelegramAuthClient {
    pub fn new(client: reqwest::Client, base: reqwest::Url) -> Self {
        Self { client, base }
    }

    async fn request<T: serde::de::DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        bearer: Option<&str>,
        poll: Option<&str>,
    ) -> Result<T, TelegramAuthError> {
        let url = self
            .base
            .join(path)
            .map_err(|_| TelegramAuthError::InvalidResponse)?;
        let mut request = self
            .client
            .request(method, url)
            .timeout(Duration::from_secs(15));
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        if let Some(secret) = poll {
            request = request.header("X-Poll-Secret", secret);
        }
        let mut response = request
            .send()
            .await
            .map_err(|_| TelegramAuthError::Network)?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            // Do not log URLs, response bodies or credentials from auth requests.
            tracing::warn!(
                http_status = status,
                "Telegram authentication request failed"
            );
        }
        match status {
            401 | 403 => return Err(TelegramAuthError::Unauthorized),
            404 if path == "/api/auth/telegram/challenges" => {
                return Err(TelegramAuthError::ApiUnavailable);
            }
            503 => return Err(TelegramAuthError::NotConfigured),
            200..=299 => {}
            _ => return Err(TelegramAuthError::Network),
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| TelegramAuthError::Network)?
        {
            if bytes.len() + chunk.len() > 65536 {
                return Err(TelegramAuthError::InvalidResponse);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| TelegramAuthError::InvalidResponse)
    }

    pub async fn begin(&self) -> Result<TelegramChallenge, TelegramAuthError> {
        let c: TelegramChallenge = self
            .request(
                reqwest::Method::POST,
                "/api/auth/telegram/challenges",
                None,
                None,
            )
            .await?;
        // Never open an arbitrary URL supplied by a compromised/misconfigured API.
        let url: reqwest::Url = c
            .url
            .parse()
            .map_err(|_| TelegramAuthError::InvalidResponse)?;
        if c.id.len() != 43
            || !c
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            || url.scheme() != "https"
            || url.host_str() != Some("t.me")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(TelegramAuthError::InvalidResponse);
        }
        Ok(c)
    }
    pub async fn status(&self, c: &TelegramChallenge) -> Result<String, TelegramAuthError> {
        #[derive(Deserialize)]
        struct Status {
            status: String,
        }
        let response: Status = self
            .request(
                reqwest::Method::GET,
                &format!("/api/auth/telegram/challenges/{}", c.id),
                None,
                Some(&c.poll_secret),
            )
            .await?;
        Ok(response.status)
    }
    pub async fn exchange(
        &self,
        c: &TelegramChallenge,
    ) -> Result<TelegramSession, TelegramAuthError> {
        self.request(
            reqwest::Method::POST,
            &format!("/api/auth/telegram/challenges/{}/exchange", c.id),
            None,
            Some(&c.poll_secret),
        )
        .await
    }
    pub async fn cancel(&self, c: &TelegramChallenge) {
        let _ = self
            .request::<serde_json::Value>(
                reqwest::Method::DELETE,
                &format!("/api/auth/telegram/challenges/{}", c.id),
                None,
                Some(&c.poll_secret),
            )
            .await;
    }
    pub async fn me(&self, token: &str) -> Result<TelegramUser, TelegramAuthError> {
        self.request(reqwest::Method::GET, "/api/auth/me", Some(token), None)
            .await
    }
    pub async fn logout(&self, token: &str) -> Result<(), TelegramAuthError> {
        self.request::<serde_json::Value>(
            reqwest::Method::POST,
            "/api/auth/logout",
            Some(token),
            None,
        )
        .await
        .map(|_| ())
    }
}

#[derive(Clone)]
pub struct TelegramAuthGate {
    pub client: TelegramAuthClient,
    session: Arc<RwLock<Option<TelegramSession>>>,
}

impl std::fmt::Debug for TelegramAuthGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TelegramAuthGate(<protected>)")
    }
}
impl PartialEq for TelegramAuthGate {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.session, &other.session) && self.client.base == other.client.base
    }
}
impl Eq for TelegramAuthGate {}

impl TelegramAuthGate {
    pub fn new(client: TelegramAuthClient) -> Self {
        Self {
            client,
            session: Arc::new(RwLock::new(None)),
        }
    }
    pub fn set_session(&self, session: Option<TelegramSession>) {
        *self.session.write().expect("session lock") = session;
    }
    pub fn session(&self) -> Option<TelegramSession> {
        self.session.read().expect("session lock").clone()
    }
    pub async fn verify(&self) -> Result<TelegramUser, TelegramAuthError> {
        let s = self.session().ok_or(TelegramAuthError::Unauthorized)?;
        let user = self.client.me(&s.token).await?;
        if user.registration_status == "banned"
            || user.login != s.user.login
            || user.nickname != s.user.nickname
            || crate::OfflineProfile::new(&user.nickname).is_err()
        {
            return Err(TelegramAuthError::Unauthorized);
        }
        Ok(user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn mock(body: &str, status: u16) -> (reqwest::Url, tokio::task::JoinHandle<String>) {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", server.local_addr().unwrap())
            .parse()
            .unwrap();
        let body = body.to_owned();
        let task = tokio::spawn(async move {
            let (mut stream, _) = server.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut bytes = [0u8; 4096];
                let n = stream.read(&mut bytes).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&bytes[..n]);
                if request.windows(4).any(|s| s == b"\r\n\r\n") {
                    break;
                }
            }
            stream.write_all(format!("HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        (url, task)
    }
    fn session() -> TelegramSession {
        TelegramSession {
            token: "test-only-token".into(),
            expires_at: 9999999999999,
            user: TelegramUser {
                login: "tg_1".into(),
                nickname: "Player".into(),
                registration_status: "pending".into(),
            },
        }
    }
    #[tokio::test]
    async fn missing_telegram_api_is_not_reported_as_a_connection_problem() {
        let (url, request) = mock(r#"{"error":"API endpoint not found"}"#, 404).await;
        let client = TelegramAuthClient::new(reqwest::Client::new(), url);
        let error = client.begin().await.err().expect("missing API must fail");

        assert!(error.to_string().contains("HTTP 404"));
        assert!(!error.to_string().contains("Проверьте подключение"));
        assert!(
            request
                .await
                .unwrap()
                .starts_with("POST /api/auth/telegram/challenges HTTP/1.1")
        );
    }
    #[tokio::test]
    async fn creates_challenge_and_distinguishes_a_disconnected_bot() {
        let (url, request) = mock(
            r#"{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","poll_secret":"test-only-secret","code":"123456","expires_at":9999999999999,"url":"https://t.me/test_bot?start=test"}"#,
            200,
        )
        .await;
        let client = TelegramAuthClient::new(reqwest::Client::new(), url);
        let challenge = client.begin().await.unwrap();
        assert_eq!(challenge.code, "123456");
        assert_eq!(challenge.url, "https://t.me/test_bot?start=test");
        request.await.unwrap();

        let (url, request) = mock("{}", 503).await;
        let client = TelegramAuthClient::new(reqwest::Client::new(), url);
        assert!(matches!(
            client.begin().await,
            Err(TelegramAuthError::NotConfigured)
        ));
        request.await.unwrap();
    }
    #[tokio::test]
    async fn verifies_pending_and_keeps_credentials_out_of_urls_and_debug() {
        let (url, request) = mock(
            r#"{"login":"tg_1","nickname":"Player","registrationStatus":"pending"}"#,
            200,
        )
        .await;
        let gate = TelegramAuthGate::new(TelegramAuthClient::new(reqwest::Client::new(), url));
        assert!(matches!(
            gate.verify().await,
            Err(TelegramAuthError::Unauthorized)
        ));
        gate.set_session(Some(session()));
        assert_eq!(gate.verify().await.unwrap().nickname, "Player");
        let request = request.await.unwrap();
        assert!(request.starts_with("GET /api/auth/me HTTP/1.1"));
        assert!(
            request
                .to_lowercase()
                .contains("authorization: bearer test-only-token")
        );
        assert!(!format!("{gate:?}").contains("test-only-token"));
        gate.set_session(None);
        assert!(matches!(
            gate.verify().await,
            Err(TelegramAuthError::Unauthorized)
        ));
    }
    #[tokio::test]
    async fn rejects_ban_changed_identity_revocation_and_oversized_response() {
        for (body, status) in [
            (
                r#"{"login":"tg_1","nickname":"Player","registrationStatus":"banned"}"#.to_owned(),
                200,
            ),
            (
                r#"{"login":"tg_2","nickname":"Player","registrationStatus":"pending"}"#.to_owned(),
                200,
            ),
            (
                r#"{"login":"tg_1","nickname":"OtherPlayer","registrationStatus":"confirmed"}"#
                    .to_owned(),
                200,
            ),
            ("{}".to_owned(), 401),
            ("x".repeat(65537), 200),
        ] {
            let (url, request) = mock(&body, status).await;
            let gate = TelegramAuthGate::new(TelegramAuthClient::new(reqwest::Client::new(), url));
            gate.set_session(Some(session()));
            assert!(gate.verify().await.is_err());
            request.await.unwrap();
        }
    }
    #[tokio::test]
    async fn core_requires_session_before_any_backend_downloads() {
        let (base, request) = mock(
            r#"{"login":"tg_1","nickname":"Player","registrationStatus":"pending"}"#,
            200,
        )
        .await;
        let client = reqwest::Client::new();
        let gate = TelegramAuthGate::new(TelegramAuthClient::new(client.clone(), base.clone()));
        let temp = tempfile::tempdir().unwrap();
        let vanilla = crate::VanillaLaunchConfig {
            telegram_auth: Some(gate.clone()),
            heap_memory: None,
            jvm_args: vec![],
            version_id: "test".into(),
            java_major_override: None,
            server_address: None,
            catalog_url: base.clone(),
            assets_base_url: base.clone(),
            runtime_root: temp.path().into(),
            game_directory: temp.path().into(),
            launcher_name: "test".into(),
            launcher_version: "test".into(),
        };
        let game = crate::LauncherGameConfig::new(client.clone(), vanilla);
        let (backend_base, config_request) = mock("{}", 200).await;
        let core = crate::LauncherCore::new_backend_with_game(
            &tokio::runtime::Handle::current(),
            temp.path(),
            client,
            crate::LauncherBackendConfig::new(backend_base).unwrap(),
            game,
        );
        core.set_offline_profile(crate::OfflineProfile::new("Player").unwrap());
        let mut states = core.subscribe_state();
        core.send_command(crate::LauncherCommand::Play)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while states.borrow().status() != crate::LauncherStatus::AuthenticationRequired {
                states.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);

        // Re-authentication must let the same actor proceed to client config.
        gate.set_session(Some(session()));
        core.send_command(crate::LauncherCommand::Play)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                states.changed().await.unwrap();
                if states.borrow().status() == crate::LauncherStatus::Error {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            states.borrow().failure(),
            Some(&crate::LauncherFailure::ClientConfig)
        );
        assert!(
            config_request
                .await
                .unwrap()
                .starts_with("GET /api/client-config HTTP/1.1")
        );
        assert!(
            request
                .await
                .unwrap()
                .starts_with("GET /api/auth/me HTTP/1.1")
        );
    }
}
