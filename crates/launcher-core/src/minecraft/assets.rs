use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AssetIndexFile {
    #[serde(default)]
    pub objects: BTreeMap<String, AssetObject>,
}

impl AssetIndexFile {
    pub fn from_json(json: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(json)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AssetObject {
    pub hash: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssetPathError {
    InvalidSha1 { value: String },
    InvalidBaseUrl,
}

impl fmt::Display for AssetPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSha1 { value } => write!(formatter, "invalid asset SHA-1 {value}"),
            Self::InvalidBaseUrl => {
                formatter.write_str("assets base URL cannot contain path segments")
            }
        }
    }
}

impl Error for AssetPathError {}

pub fn asset_object_path(assets_root: &Path, sha1: &str) -> Result<PathBuf, AssetPathError> {
    let normalized = validated_sha1(sha1)?;
    Ok(assets_root
        .join("objects")
        .join(&normalized[..2])
        .join(normalized))
}

pub fn asset_object_url(
    assets_base_url: &reqwest::Url,
    sha1: &str,
) -> Result<reqwest::Url, AssetPathError> {
    let normalized = validated_sha1(sha1)?;
    let mut url = assets_base_url.clone();
    url.set_query(None);
    url.set_fragment(None);
    let mut segments = url
        .path_segments_mut()
        .map_err(|_| AssetPathError::InvalidBaseUrl)?;
    segments.pop_if_empty();
    segments.push(&normalized[..2]);
    segments.push(&normalized);
    drop(segments);
    Ok(url)
}

pub(crate) fn validated_sha1(value: &str) -> Result<String, AssetPathError> {
    if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(value.to_ascii_lowercase())
    } else {
        Err(AssetPathError::InvalidSha1 {
            value: value.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_asset_index_and_builds_object_path() {
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let json =
            format!(r#"{{"objects":{{"minecraft/sound.ogg":{{"hash":"{hash}","size":42}}}}}}"#);

        let index = AssetIndexFile::from_json(json.as_bytes()).unwrap();
        let object = &index.objects["minecraft/sound.ogg"];
        let path = asset_object_path(Path::new("assets"), &object.hash).unwrap();

        assert_eq!(object.size, 42);
        assert_eq!(path, PathBuf::from("assets/objects/01").join(hash));
    }

    #[test]
    fn rejects_invalid_asset_sha1() {
        let error = asset_object_path(Path::new("assets"), "not-a-sha1").unwrap_err();

        assert!(matches!(error, AssetPathError::InvalidSha1 { .. }));
    }
}
