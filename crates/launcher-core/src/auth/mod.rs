mod microsoft;
mod minecraft;
mod session;
mod xbox;

use std::{
    error::Error,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::sync::Notify;

pub use microsoft::{
    DeviceLogin, MicrosoftAuthConfig, MicrosoftAuthError, MicrosoftTokenSet, begin_device_login,
    complete_device_login, refresh_microsoft_token,
};
pub use minecraft::{
    EntitlementError, MinecraftAccessToken, MinecraftEntitlement, MinecraftEntitlements,
    MinecraftLoginError, MinecraftProfile, MinecraftProfileError, MinecraftServicesConfig,
    authenticate_minecraft, check_minecraft_entitlement, fetch_minecraft_profile,
};
pub use session::{MinecraftSession, MinecraftSessionError, create_minecraft_session};
pub use xbox::{
    XboxAuthConfig, XboxUserAuthError, XboxUserToken, XstsAuthError, XstsToken,
    authenticate_xbox_user, authenticate_xsts,
};

pub const MAX_AUTH_RESPONSE_SIZE: usize = 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct AuthCancellation {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl AuthCancellation {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let notified = self.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Debug)]
pub enum AuthHttpError {
    Request {
        endpoint: reqwest::Url,
        source: reqwest::Error,
    },
    ResponseTooLarge {
        endpoint: reqwest::Url,
        limit: usize,
    },
}

impl fmt::Display for AuthHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request { endpoint, .. } => {
                write!(formatter, "authentication request to {endpoint} failed")
            }
            Self::ResponseTooLarge { endpoint, limit } => write!(
                formatter,
                "authentication response from {endpoint} exceeds {limit} bytes"
            ),
        }
    }
}

impl Error for AuthHttpError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Request { source, .. } => Some(source),
            Self::ResponseTooLarge { .. } => None,
        }
    }
}

pub(crate) struct AuthHttpResponse {
    pub status: reqwest::StatusCode,
    pub body: Vec<u8>,
}

pub(crate) async fn send_auth_request(
    request: reqwest::RequestBuilder,
    endpoint: &reqwest::Url,
) -> Result<AuthHttpResponse, AuthHttpError> {
    let mut response = request
        .timeout(crate::http::METADATA_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|source| AuthHttpError::Request {
            endpoint: endpoint.clone(),
            source,
        })?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_AUTH_RESPONSE_SIZE as u64)
    {
        return Err(AuthHttpError::ResponseTooLarge {
            endpoint: endpoint.clone(),
            limit: MAX_AUTH_RESPONSE_SIZE,
        });
    }

    let status = response.status();
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|source| AuthHttpError::Request {
            endpoint: endpoint.clone(),
            source,
        })?
    {
        let next_size =
            body.len()
                .checked_add(chunk.len())
                .ok_or(AuthHttpError::ResponseTooLarge {
                    endpoint: endpoint.clone(),
                    limit: MAX_AUTH_RESPONSE_SIZE,
                })?;
        if next_size > MAX_AUTH_RESPONSE_SIZE {
            return Err(AuthHttpError::ResponseTooLarge {
                endpoint: endpoint.clone(),
                limit: MAX_AUTH_RESPONSE_SIZE,
            });
        }
        body.extend_from_slice(&chunk);
    }

    Ok(AuthHttpResponse { status, body })
}
