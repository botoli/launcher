use std::{error::Error, fmt};

use serde::Deserialize;

use super::{
    acquisition::{VersionMetadataFetchError, fetch_version_metadata_verified},
    metadata::VersionMetadata,
};

pub const MAX_VERSION_CATALOG_SIZE: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct VersionCatalog {
    pub latest: VersionCatalogLatest,
    pub versions: Vec<VersionCatalogEntry>,
}

impl VersionCatalog {
    pub fn latest_release(&self) -> Result<&VersionCatalogEntry, VersionResolveError> {
        resolve_version(self, &self.latest.release)
    }

    pub fn latest_snapshot(&self) -> Result<&VersionCatalogEntry, VersionResolveError> {
        resolve_version(self, &self.latest.snapshot)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct VersionCatalogLatest {
    pub release: String,
    pub snapshot: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct VersionCatalogEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub version_type: String,
    pub url: String,
    pub sha1: String,
}

#[derive(Debug)]
pub enum VersionCatalogError {
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
}

impl fmt::Display for VersionCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request { url, .. } => write!(formatter, "failed to fetch version catalog {url}"),
            Self::HttpStatus { url, status } => {
                write!(
                    formatter,
                    "version catalog request for {url} returned {status}"
                )
            }
            Self::TooLarge { limit } => {
                write!(formatter, "version catalog exceeds the {limit} byte limit")
            }
            Self::InvalidJson(_) => formatter.write_str("version catalog contains invalid JSON"),
        }
    }
}

impl Error for VersionCatalogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Request { source, .. } => Some(source),
            Self::InvalidJson(source) => Some(source),
            Self::HttpStatus { .. } | Self::TooLarge { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VersionResolveError {
    UnknownVersion { version_id: String },
}

impl fmt::Display for VersionResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownVersion { version_id } => {
                write!(
                    formatter,
                    "Minecraft version {version_id} is not in the catalog"
                )
            }
        }
    }
}

impl Error for VersionResolveError {}

#[derive(Debug)]
pub enum FetchVersionByIdError {
    Catalog(VersionCatalogError),
    Resolve(VersionResolveError),
    InvalidMetadataUrl {
        version_id: String,
        value: String,
        source: url::ParseError,
    },
    Metadata(VersionMetadataFetchError),
}

impl fmt::Display for FetchVersionByIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Catalog(source) => write!(formatter, "failed to load version catalog: {source}"),
            Self::Resolve(source) => write!(formatter, "failed to resolve version: {source}"),
            Self::InvalidMetadataUrl { version_id, .. } => {
                write!(
                    formatter,
                    "version {version_id} has an invalid metadata URL"
                )
            }
            Self::Metadata(source) => {
                write!(formatter, "failed to load version metadata: {source}")
            }
        }
    }
}

impl Error for FetchVersionByIdError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Catalog(source) => Some(source),
            Self::Resolve(source) => Some(source),
            Self::InvalidMetadataUrl { source, .. } => Some(source),
            Self::Metadata(source) => Some(source),
        }
    }
}

pub async fn fetch_version_catalog(
    client: &reqwest::Client,
    catalog_url: &reqwest::Url,
) -> Result<VersionCatalog, VersionCatalogError> {
    let mut response = client
        .get(catalog_url.clone())
        .timeout(crate::http::METADATA_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|source| VersionCatalogError::Request {
            url: catalog_url.clone(),
            source,
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(VersionCatalogError::HttpStatus {
            url: catalog_url.clone(),
            status,
        });
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_VERSION_CATALOG_SIZE as u64)
    {
        return Err(VersionCatalogError::TooLarge {
            limit: MAX_VERSION_CATALOG_SIZE,
        });
    }

    let mut body = Vec::new();
    while let Some(chunk) =
        response
            .chunk()
            .await
            .map_err(|source| VersionCatalogError::Request {
                url: catalog_url.clone(),
                source,
            })?
    {
        let next_size =
            body.len()
                .checked_add(chunk.len())
                .ok_or(VersionCatalogError::TooLarge {
                    limit: MAX_VERSION_CATALOG_SIZE,
                })?;
        if next_size > MAX_VERSION_CATALOG_SIZE {
            return Err(VersionCatalogError::TooLarge {
                limit: MAX_VERSION_CATALOG_SIZE,
            });
        }
        body.extend_from_slice(&chunk);
    }

    serde_json::from_slice(&body).map_err(VersionCatalogError::InvalidJson)
}

pub fn resolve_version<'catalog>(
    catalog: &'catalog VersionCatalog,
    version_id: &str,
) -> Result<&'catalog VersionCatalogEntry, VersionResolveError> {
    catalog
        .versions
        .iter()
        .find(|entry| entry.id == version_id)
        .ok_or_else(|| VersionResolveError::UnknownVersion {
            version_id: version_id.to_owned(),
        })
}

pub async fn fetch_version_by_id(
    client: &reqwest::Client,
    catalog_url: &reqwest::Url,
    version_id: &str,
) -> Result<VersionMetadata, FetchVersionByIdError> {
    let catalog = fetch_version_catalog(client, catalog_url)
        .await
        .map_err(FetchVersionByIdError::Catalog)?;
    let entry = resolve_version(&catalog, version_id).map_err(FetchVersionByIdError::Resolve)?;
    let metadata_url = reqwest::Url::parse(&entry.url).map_err(|source| {
        FetchVersionByIdError::InvalidMetadataUrl {
            version_id: entry.id.clone(),
            value: entry.url.clone(),
            source,
        }
    })?;

    fetch_version_metadata_verified(client, &metadata_url, &entry.sha1)
        .await
        .map_err(FetchVersionByIdError::Metadata)
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use serde_json::json;
    use sha1::{Digest, Sha1};

    use super::*;
    use crate::{
        AcquisitionCancellation, MinecraftRuntimeLayout, OperatingSystem, VanillaAcquisitionConfig,
        acquire_vanilla_runtime, build_http_client,
        http::test_support::{TestResponse, TestServer},
    };

    const RELEASE_ID: &str = "1.21.1";
    const SNAPSHOT_ID: &str = "24w33a";

    #[tokio::test]
    async fn fetches_successful_catalog() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let body = catalog_json(&server, "0".repeat(40), "1".repeat(40));
        server.route("/catalog.json", TestResponse::ok(body));

        let catalog =
            fetch_version_catalog(&build_http_client()?, &server.url("catalog.json")?).await?;

        assert_eq!(catalog.latest.release, RELEASE_ID);
        assert_eq!(catalog.versions.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn catalog_404_is_typed() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;

        let error = fetch_version_catalog(&build_http_client()?, &server.url("missing.json")?)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            VersionCatalogError::HttpStatus { status, .. }
                if status == reqwest::StatusCode::NOT_FOUND
        ));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_oversized_catalog() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route(
            "/catalog.json",
            TestResponse::ok(Vec::new()).with_advertised_length(MAX_VERSION_CATALOG_SIZE + 1),
        );

        let error = fetch_version_catalog(&build_http_client()?, &server.url("catalog.json")?)
            .await
            .unwrap_err();

        assert!(matches!(error, VersionCatalogError::TooLarge { .. }));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_invalid_catalog_json() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route("/catalog.json", TestResponse::ok("{invalid"));

        let error = fetch_version_catalog(&build_http_client()?, &server.url("catalog.json")?)
            .await
            .unwrap_err();

        assert!(matches!(error, VersionCatalogError::InvalidJson(_)));
        Ok(())
    }

    #[test]
    fn resolves_release_by_exact_id() {
        let catalog = catalog_fixture();

        let entry = resolve_version(&catalog, RELEASE_ID).unwrap();

        assert_eq!(entry.id, RELEASE_ID);
        assert_eq!(entry.version_type, "release");
    }

    #[test]
    fn resolves_snapshot_by_exact_id() {
        let catalog = catalog_fixture();

        let entry = resolve_version(&catalog, SNAPSHOT_ID).unwrap();

        assert_eq!(entry.id, SNAPSHOT_ID);
        assert_eq!(entry.version_type, "snapshot");
    }

    #[test]
    fn unknown_version_id_is_typed() {
        let error = resolve_version(&catalog_fixture(), "latest").unwrap_err();

        assert_eq!(
            error,
            VersionResolveError::UnknownVersion {
                version_id: "latest".to_owned()
            }
        );
    }

    #[test]
    fn resolves_latest_release() {
        let catalog = catalog_fixture();

        assert_eq!(catalog.latest_release().unwrap().id, RELEASE_ID);
    }

    #[test]
    fn resolves_latest_snapshot() {
        let catalog = catalog_fixture();

        assert_eq!(catalog.latest_snapshot().unwrap().id, SNAPSHOT_ID);
    }

    #[tokio::test]
    async fn fetches_version_by_id_with_verified_metadata() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let metadata = metadata_json(&server, b"client");
        let metadata_sha1 = sha1_hex(metadata.as_bytes());
        server.route("/versions/release.json", TestResponse::ok(metadata));
        server.route(
            "/catalog.json",
            TestResponse::ok(catalog_json(&server, metadata_sha1, "1".repeat(40))),
        );

        let fetched = fetch_version_by_id(
            &build_http_client()?,
            &server.url("catalog.json")?,
            RELEASE_ID,
        )
        .await?;

        assert_eq!(fetched.id, RELEASE_ID);
        assert_eq!(server.request_count("/versions/release.json"), 1);
        Ok(())
    }

    #[tokio::test]
    async fn rejects_version_metadata_with_wrong_sha1() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route(
            "/versions/release.json",
            TestResponse::ok(metadata_json(&server, b"client")),
        );
        server.route(
            "/catalog.json",
            TestResponse::ok(catalog_json(&server, "0".repeat(40), "1".repeat(40))),
        );

        let error = fetch_version_by_id(
            &build_http_client()?,
            &server.url("catalog.json")?,
            RELEASE_ID,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            FetchVersionByIdError::Metadata(VersionMetadataFetchError::Sha1Mismatch { .. })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn catalog_to_metadata_to_runtime_acquisition() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let client_jar = b"synthetic client jar";
        let metadata = metadata_json(&server, client_jar);
        let metadata_sha1 = sha1_hex(metadata.as_bytes());
        server.route("/versions/release.json", TestResponse::ok(metadata));
        server.route("/client.jar", TestResponse::ok(client_jar.to_vec()));
        server.route(
            "/catalog.json",
            TestResponse::ok(catalog_json(&server, metadata_sha1, "1".repeat(40))),
        );
        let client = build_http_client()?;
        let metadata =
            fetch_version_by_id(&client, &server.url("catalog.json")?, RELEASE_ID).await?;
        let installation = tempfile::tempdir()?;
        let layout = MinecraftRuntimeLayout::new(installation.path());
        let config = VanillaAcquisitionConfig::new(server.url("assets/objects/")?);

        let summary = acquire_vanilla_runtime(
            &client,
            &metadata,
            &layout,
            OperatingSystem::Windows,
            &config,
            &AcquisitionCancellation::default(),
            |_| {},
        )
        .await?;

        assert_eq!(summary.downloaded_files, 1);
        assert_eq!(std::fs::read(layout.client_jar(RELEASE_ID))?, client_jar);
        Ok(())
    }

    fn catalog_fixture() -> VersionCatalog {
        VersionCatalog {
            latest: VersionCatalogLatest {
                release: RELEASE_ID.to_owned(),
                snapshot: SNAPSHOT_ID.to_owned(),
            },
            versions: vec![
                VersionCatalogEntry {
                    id: RELEASE_ID.to_owned(),
                    version_type: "release".to_owned(),
                    url: "https://example.invalid/release.json".to_owned(),
                    sha1: "0".repeat(40),
                },
                VersionCatalogEntry {
                    id: SNAPSHOT_ID.to_owned(),
                    version_type: "snapshot".to_owned(),
                    url: "https://example.invalid/snapshot.json".to_owned(),
                    sha1: "1".repeat(40),
                },
            ],
        }
    }

    fn catalog_json(server: &TestServer, release_sha1: String, snapshot_sha1: String) -> String {
        json!({
            "latest": {
                "release": RELEASE_ID,
                "snapshot": SNAPSHOT_ID
            },
            "versions": [
                {
                    "id": RELEASE_ID,
                    "type": "release",
                    "url": server.url("versions/release.json").unwrap().as_str(),
                    "sha1": release_sha1,
                    "ignored": "field"
                },
                {
                    "id": SNAPSHOT_ID,
                    "type": "snapshot",
                    "url": server.url("versions/snapshot.json").unwrap().as_str(),
                    "sha1": snapshot_sha1
                }
            ],
            "ignored": true
        })
        .to_string()
    }

    fn metadata_json(server: &TestServer, client_jar: &[u8]) -> String {
        json!({
            "id": RELEASE_ID,
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "assets": "test-assets",
            "downloads": {
                "client": {
                    "url": server.url("client.jar").unwrap().as_str(),
                    "sha1": sha1_hex(client_jar),
                    "size": client_jar.len()
                }
            },
            "libraries": []
        })
        .to_string()
    }

    fn sha1_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha1::digest(bytes))
    }
}
