use std::{error::Error, fmt};

use md5::{Digest, Md5};
use uuid::Uuid;

use crate::auth::MinecraftSession;

pub const MIN_OFFLINE_USERNAME_LENGTH: usize = 3;
pub const MAX_OFFLINE_USERNAME_LENGTH: usize = 16;

const OFFLINE_UUID_PREFIX: &str = "OfflinePlayer:";
const OFFLINE_ACCESS_TOKEN: &str = "0";
const OFFLINE_USER_TYPE: &str = "legacy";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfflineProfile {
    username: String,
    uuid: String,
}

impl OfflineProfile {
    pub fn new(username: impl Into<String>) -> Result<Self, OfflineProfileError> {
        let username = username.into();
        validate_username(&username)?;

        Ok(Self {
            uuid: offline_uuid(&username),
            username,
        })
    }

    pub fn with_uuid(
        username: impl Into<String>,
        uuid: impl AsRef<str>,
    ) -> Result<Self, OfflineProfileError> {
        let username = username.into();
        validate_username(&username)?;
        let uuid = Uuid::parse_str(uuid.as_ref())
            .map_err(|source| OfflineProfileError::InvalidUuid { source })?;

        Ok(Self {
            username,
            uuid: uuid.hyphenated().to_string(),
        })
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn uuid(&self) -> &str {
        &self.uuid
    }
}

#[derive(Debug)]
pub enum OfflineProfileError {
    UsernameTooShort { length: usize, minimum: usize },
    UsernameTooLong { length: usize, maximum: usize },
    InvalidUsernameCharacter { character: char, index: usize },
    InvalidUuid { source: uuid::Error },
}

impl fmt::Display for OfflineProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UsernameTooShort { length, minimum } => write!(
                formatter,
                "offline username has length {length}, minimum is {minimum}"
            ),
            Self::UsernameTooLong { length, maximum } => write!(
                formatter,
                "offline username has length {length}, maximum is {maximum}"
            ),
            Self::InvalidUsernameCharacter { character, index } => write!(
                formatter,
                "offline username contains invalid character {character:?} at index {index}"
            ),
            Self::InvalidUuid { source } => write!(formatter, "invalid offline UUID: {source}"),
        }
    }
}

impl Error for OfflineProfileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidUuid { source } => Some(source),
            Self::UsernameTooShort { .. }
            | Self::UsernameTooLong { .. }
            | Self::InvalidUsernameCharacter { .. } => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct MinecraftLaunchIdentity {
    username: String,
    uuid: String,
    access_token: String,
    user_type: String,
}

impl MinecraftLaunchIdentity {
    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn uuid(&self) -> &str {
        &self.uuid
    }

    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    pub fn user_type(&self) -> &str {
        &self.user_type
    }
}

impl From<&OfflineProfile> for MinecraftLaunchIdentity {
    fn from(profile: &OfflineProfile) -> Self {
        Self {
            username: profile.username.clone(),
            uuid: profile.uuid.clone(),
            access_token: OFFLINE_ACCESS_TOKEN.to_owned(),
            user_type: OFFLINE_USER_TYPE.to_owned(),
        }
    }
}

impl From<&MinecraftSession> for MinecraftLaunchIdentity {
    fn from(session: &MinecraftSession) -> Self {
        Self {
            username: session.username.clone(),
            uuid: session.uuid.clone(),
            access_token: session.access_token().to_owned(),
            user_type: session.user_type().to_owned(),
        }
    }
}

impl fmt::Debug for MinecraftLaunchIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MinecraftLaunchIdentity")
            .field("username", &self.username)
            .field("uuid", &self.uuid)
            .field("access_token", &"<redacted>")
            .field("user_type", &self.user_type)
            .finish()
    }
}

fn validate_username(username: &str) -> Result<(), OfflineProfileError> {
    let length = username.chars().count();
    if length < MIN_OFFLINE_USERNAME_LENGTH {
        return Err(OfflineProfileError::UsernameTooShort {
            length,
            minimum: MIN_OFFLINE_USERNAME_LENGTH,
        });
    }
    if length > MAX_OFFLINE_USERNAME_LENGTH {
        return Err(OfflineProfileError::UsernameTooLong {
            length,
            maximum: MAX_OFFLINE_USERNAME_LENGTH,
        });
    }

    if let Some((index, character)) = username
        .chars()
        .enumerate()
        .find(|(_, character)| !character.is_ascii_alphanumeric() && *character != '_')
    {
        return Err(OfflineProfileError::InvalidUsernameCharacter { character, index });
    }

    Ok(())
}

fn offline_uuid(username: &str) -> String {
    let mut digest: [u8; 16] = Md5::digest(format!("{OFFLINE_UUID_PREFIX}{username}")).into();
    digest[6] = (digest[6] & 0x0f) | 0x30;
    digest[8] = (digest[8] & 0x3f) | 0x80;
    Uuid::from_bytes(digest).hyphenated().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_minecraft_username() {
        let profile = OfflineProfile::new("Player_123").unwrap();

        assert_eq!(profile.username(), "Player_123");
    }

    #[test]
    fn rejects_invalid_username_character() {
        let error = OfflineProfile::new("Bad-Player").unwrap_err();

        assert!(matches!(
            error,
            OfflineProfileError::InvalidUsernameCharacter {
                character: '-',
                index: 3
            }
        ));
    }

    #[test]
    fn rejects_too_short_username() {
        let error = OfflineProfile::new("ab").unwrap_err();

        assert!(matches!(
            error,
            OfflineProfileError::UsernameTooShort {
                length: 2,
                minimum: MIN_OFFLINE_USERNAME_LENGTH
            }
        ));
    }

    #[test]
    fn rejects_too_long_username() {
        let error = OfflineProfile::new("a".repeat(17)).unwrap_err();

        assert!(matches!(
            error,
            OfflineProfileError::UsernameTooLong {
                length: 17,
                maximum: MAX_OFFLINE_USERNAME_LENGTH
            }
        ));
    }

    #[test]
    fn generates_deterministic_standard_offline_uuid() {
        let first = OfflineProfile::new("Notch").unwrap();
        let second = OfflineProfile::new("Notch").unwrap();

        assert_eq!(first.uuid(), second.uuid());
        assert_eq!(first.uuid(), "b50ad385-829d-3141-a216-7e7d7539ba7f");
    }

    #[test]
    fn different_usernames_generate_different_uuids() {
        let first = OfflineProfile::new("PlayerOne").unwrap();
        let second = OfflineProfile::new("PlayerTwo").unwrap();

        assert_ne!(first.uuid(), second.uuid());
    }

    #[test]
    fn preserves_explicit_uuid_in_canonical_form() {
        let profile =
            OfflineProfile::with_uuid("PlayerOne", "0123456789ABCDEF0123456789ABCDEF").unwrap();

        assert_eq!(profile.uuid(), "01234567-89ab-cdef-0123-456789abcdef");
    }

    #[test]
    fn rejects_invalid_explicit_uuid() {
        let error = OfflineProfile::with_uuid("PlayerOne", "not-a-uuid").unwrap_err();

        assert!(matches!(error, OfflineProfileError::InvalidUuid { .. }));
    }

    #[test]
    fn converts_offline_profile_to_legacy_launch_identity() {
        let profile = OfflineProfile::new("PlayerOne").unwrap();
        let identity = MinecraftLaunchIdentity::from(&profile);

        assert_eq!(identity.username(), profile.username());
        assert_eq!(identity.uuid(), profile.uuid());
        assert_eq!(identity.access_token(), "0");
        assert_eq!(identity.user_type(), "legacy");
        let debug = format!("{identity:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("access_token: \"0\""));
    }
}
