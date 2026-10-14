use std::{
    collections::HashMap,
    error::Error,
    fmt, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use sha1::{Digest, Sha1};
use tokio::task::{JoinError, JoinSet};

use super::{
    assets::{AssetIndexFile, AssetPathError, asset_object_path, asset_object_url, validated_sha1},
    download::{
        AcquisitionCancellation, DownloadDisposition, RuntimeDownloadError, ensure_verified_file,
    },
    metadata::{LibraryArtifact, VersionMetadata},
    rules::{OperatingSystem, RuleContext, rules_allow},
    runtime::MinecraftRuntimeLayout,
};

pub const MAX_VERSION_METADATA_SIZE: usize = 8 * 1024 * 1024;
pub const MAX_ASSET_INDEX_SIZE: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_CONCURRENCY: usize = 6;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VanillaAcquisitionConfig {
    pub assets_base_url: reqwest::Url,
    pub max_concurrency: usize,
}

impl VanillaAcquisitionConfig {
    pub fn new(assets_base_url: reqwest::Url) -> Self {
        Self {
            assets_base_url,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AcquisitionProgress {
    pub completed_bytes: u64,
    pub total_bytes: u64,
}

impl AcquisitionProgress {
    pub fn fraction(self) -> f32 {
        if self.total_bytes == 0 {
            1.0
        } else {
            (self.completed_bytes as f64 / self.total_bytes as f64).clamp(0.0, 1.0) as f32
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AcquisitionSummary {
    pub downloaded_files: usize,
    pub reused_files: usize,
    pub total_bytes: u64,
}

#[derive(Debug)]
pub enum VersionMetadataFetchError {
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
    InvalidSha1 {
        value: String,
    },
    Sha1Mismatch {
        expected: String,
        actual: String,
    },
    InvalidJson(serde_json::Error),
}

impl fmt::Display for VersionMetadataFetchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request { url, .. } => {
                write!(formatter, "failed to fetch version metadata from {url}")
            }
            Self::HttpStatus { url, status } => {
                write!(
                    formatter,
                    "version metadata request for {url} returned {status}"
                )
            }
            Self::TooLarge { limit } => {
                write!(formatter, "version metadata exceeds the {limit} byte limit")
            }
            Self::InvalidSha1 { value } => {
                write!(formatter, "version metadata has invalid SHA-1 {value}")
            }
            Self::Sha1Mismatch { expected, actual } => write!(
                formatter,
                "version metadata SHA-1 mismatch: expected {expected}, got {actual}"
            ),
            Self::InvalidJson(_) => formatter.write_str("version metadata contains invalid JSON"),
        }
    }
}

impl Error for VersionMetadataFetchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Request { source, .. } => Some(source),
            Self::InvalidJson(source) => Some(source),
            Self::HttpStatus { .. }
            | Self::TooLarge { .. }
            | Self::InvalidSha1 { .. }
            | Self::Sha1Mismatch { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum AcquisitionError {
    InvalidConcurrency,
    MissingDownloadMetadata {
        component: String,
        field: &'static str,
    },
    InvalidUrl {
        component: String,
        value: String,
        source: url::ParseError,
    },
    UnsafeLibraryPath {
        library_name: String,
        path: String,
    },
    InvalidAssetIndexId {
        id: String,
    },
    InvalidAsset(AssetPathError),
    ConflictingDestination {
        path: PathBuf,
    },
    Download(RuntimeDownloadError),
    ReadAssetIndex {
        path: PathBuf,
        source: io::Error,
    },
    AssetIndexTooLarge {
        limit: usize,
    },
    InvalidAssetIndex(serde_json::Error),
    CreateAssetsRoot {
        path: PathBuf,
        source: io::Error,
    },
    DownloadTask {
        source: JoinError,
    },
    Cancelled,
}

impl fmt::Display for AcquisitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConcurrency => {
                formatter.write_str("acquisition concurrency must be greater than zero")
            }
            Self::MissingDownloadMetadata { component, field } => {
                write!(formatter, "{component} is missing download field {field}")
            }
            Self::InvalidUrl { component, .. } => {
                write!(formatter, "{component} contains an invalid download URL")
            }
            Self::UnsafeLibraryPath { library_name, path } => {
                write!(
                    formatter,
                    "library {library_name} has unsafe download path {path}"
                )
            }
            Self::InvalidAssetIndexId { id } => write!(formatter, "unsafe asset index id {id}"),
            Self::InvalidAsset(source) => write!(formatter, "invalid asset object: {source}"),
            Self::ConflictingDestination { path } => write!(
                formatter,
                "multiple downloads disagree about destination {}",
                path.display()
            ),
            Self::Download(source) => write!(formatter, "runtime file download failed: {source}"),
            Self::ReadAssetIndex { path, source } => write!(
                formatter,
                "failed to read asset index {}: {source}",
                path.display()
            ),
            Self::AssetIndexTooLarge { limit } => {
                write!(
                    formatter,
                    "asset index exceeds the {limit} byte parse limit"
                )
            }
            Self::InvalidAssetIndex(_) => formatter.write_str("asset index contains invalid JSON"),
            Self::CreateAssetsRoot { path, source } => write!(
                formatter,
                "failed to create assets root {}: {source}",
                path.display()
            ),
            Self::DownloadTask { source } => write!(formatter, "download task failed: {source}"),
            Self::Cancelled => formatter.write_str("vanilla runtime acquisition was cancelled"),
        }
    }
}

impl Error for AcquisitionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidUrl { source, .. } => Some(source),
            Self::InvalidAsset(source) => Some(source),
            Self::Download(source) => Some(source),
            Self::ReadAssetIndex { source, .. } | Self::CreateAssetsRoot { source, .. } => {
                Some(source)
            }
            Self::InvalidAssetIndex(source) => Some(source),
            Self::DownloadTask { source } => Some(source),
            Self::InvalidConcurrency
            | Self::MissingDownloadMetadata { .. }
            | Self::UnsafeLibraryPath { .. }
            | Self::InvalidAssetIndexId { .. }
            | Self::ConflictingDestination { .. }
            | Self::AssetIndexTooLarge { .. }
            | Self::Cancelled => None,
        }
    }
}

pub async fn fetch_version_metadata(
    client: &reqwest::Client,
    url: &reqwest::Url,
) -> Result<VersionMetadata, VersionMetadataFetchError> {
    let body = fetch_version_metadata_bytes(client, url).await?;
    parse_version_metadata_bytes(&body)
}

pub async fn fetch_version_metadata_verified(
    client: &reqwest::Client,
    url: &reqwest::Url,
    expected_sha1: &str,
) -> Result<VersionMetadata, VersionMetadataFetchError> {
    let expected_sha1 =
        validated_sha1(expected_sha1).map_err(|_| VersionMetadataFetchError::InvalidSha1 {
            value: expected_sha1.to_owned(),
        })?;
    let body = fetch_version_metadata_bytes(client, url).await?;
    let actual_sha1 = format!("{:x}", Sha1::digest(&body));
    if actual_sha1 != expected_sha1 {
        return Err(VersionMetadataFetchError::Sha1Mismatch {
            expected: expected_sha1,
            actual: actual_sha1,
        });
    }
    parse_version_metadata_bytes(&body)
}

async fn fetch_version_metadata_bytes(
    client: &reqwest::Client,
    url: &reqwest::Url,
) -> Result<Vec<u8>, VersionMetadataFetchError> {
    let mut response = client
        .get(url.clone())
        .timeout(crate::http::METADATA_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|source| VersionMetadataFetchError::Request {
            url: url.clone(),
            source,
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(VersionMetadataFetchError::HttpStatus {
            url: url.clone(),
            status,
        });
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_VERSION_METADATA_SIZE as u64)
    {
        return Err(VersionMetadataFetchError::TooLarge {
            limit: MAX_VERSION_METADATA_SIZE,
        });
    }

    let mut body = Vec::new();
    while let Some(chunk) =
        response
            .chunk()
            .await
            .map_err(|source| VersionMetadataFetchError::Request {
                url: url.clone(),
                source,
            })?
    {
        let next_size =
            body.len()
                .checked_add(chunk.len())
                .ok_or(VersionMetadataFetchError::TooLarge {
                    limit: MAX_VERSION_METADATA_SIZE,
                })?;
        if next_size > MAX_VERSION_METADATA_SIZE {
            return Err(VersionMetadataFetchError::TooLarge {
                limit: MAX_VERSION_METADATA_SIZE,
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn parse_version_metadata_bytes(body: &[u8]) -> Result<VersionMetadata, VersionMetadataFetchError> {
    VersionMetadata::from_json(std::str::from_utf8(body).map_err(|_| {
        VersionMetadataFetchError::InvalidJson(serde_json::Error::io(io::Error::new(
            io::ErrorKind::InvalidData,
            "version metadata is not UTF-8",
        )))
    })?)
    .map_err(VersionMetadataFetchError::InvalidJson)
}

pub async fn acquire_vanilla_runtime<F>(
    client: &reqwest::Client,
    metadata: &VersionMetadata,
    layout: &MinecraftRuntimeLayout,
    operating_system: OperatingSystem,
    config: &VanillaAcquisitionConfig,
    cancellation: &AcquisitionCancellation,
    on_progress: F,
) -> Result<AcquisitionSummary, AcquisitionError>
where
    F: Fn(AcquisitionProgress) + Send + Sync + 'static,
{
    if config.max_concurrency == 0 {
        return Err(AcquisitionError::InvalidConcurrency);
    }
    if cancellation.is_cancelled() {
        return Err(AcquisitionError::Cancelled);
    }

    let assets_root = layout.assets_dir();
    tokio::fs::create_dir_all(&assets_root)
        .await
        .map_err(|source| AcquisitionError::CreateAssetsRoot {
            path: assets_root.clone(),
            source,
        })?;
    let (runtime_items, asset_index_path) =
        runtime_download_items(metadata, layout, operating_system)?;
    let reporter = Arc::new(ProgressReporter::new(Arc::new(on_progress)));
    reporter.add_total(sum_sizes(&runtime_items));
    let mut summary = run_download_batch(
        client,
        runtime_items,
        config.max_concurrency,
        cancellation,
        Arc::clone(&reporter),
    )
    .await?;

    if let Some(asset_index_path) = asset_index_path {
        let asset_index = read_asset_index(&asset_index_path).await?;
        let asset_items = asset_download_items(&asset_index, &assets_root, config)?;
        reporter.add_total(sum_sizes(&asset_items));
        let asset_summary = run_download_batch(
            client,
            asset_items,
            config.max_concurrency,
            cancellation,
            Arc::clone(&reporter),
        )
        .await?;
        summary.downloaded_files += asset_summary.downloaded_files;
        summary.reused_files += asset_summary.reused_files;
    }

    let progress = reporter.snapshot();
    summary.total_bytes = progress.total_bytes;
    reporter.emit();
    Ok(summary)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DownloadItem {
    url: reqwest::Url,
    destination: PathBuf,
    expected_size: u64,
    expected_sha1: String,
}

fn runtime_download_items(
    metadata: &VersionMetadata,
    layout: &MinecraftRuntimeLayout,
    operating_system: OperatingSystem,
) -> Result<(Vec<DownloadItem>, Option<PathBuf>), AcquisitionError> {
    let mut items = Vec::new();
    let client = metadata
        .downloads
        .client
        .as_ref()
        .ok_or_else(|| missing("client", "downloads.client"))?;
    items.push(download_info_item(
        "client",
        &client.url,
        client.size,
        &client.sha1,
        layout.client_jar(&metadata.id),
    )?);

    let rule_context = RuleContext {
        operating_system,
        features: Default::default(),
    };
    let libraries_root = layout.libraries_dir();
    for library in &metadata.libraries {
        if !rules_allow(&library.rules, &rule_context) {
            continue;
        }
        if let Some(artifact) = library
            .downloads
            .as_ref()
            .and_then(|downloads| downloads.artifact.as_ref())
        {
            items.push(library_item(&library.name, artifact, &libraries_root)?);
        }
        if let Some(classifier) = library.native_classifier(operating_system) {
            let (_, artifact) = library.native_artifact(operating_system).ok_or_else(|| {
                missing(
                    &format!("library {} classifier {classifier}", library.name),
                    "artifact",
                )
            })?;
            items.push(library_item(&library.name, artifact, &libraries_root)?);
        }
    }

    let asset_index_path = if let Some(asset_index) = &metadata.asset_index {
        if !is_safe_component(&asset_index.id) {
            return Err(AcquisitionError::InvalidAssetIndexId {
                id: asset_index.id.clone(),
            });
        }
        let path = layout
            .assets_dir()
            .join("indexes")
            .join(format!("{}.json", asset_index.id));
        items.push(download_info_item(
            "asset index",
            &asset_index.url,
            asset_index.size,
            &asset_index.sha1,
            path.clone(),
        )?);
        Some(path)
    } else {
        None
    };
    deduplicate_items(items).map(|items| (items, asset_index_path))
}

fn library_item(
    library_name: &str,
    artifact: &LibraryArtifact,
    libraries_root: &Path,
) -> Result<DownloadItem, AcquisitionError> {
    let relative_path =
        safe_relative_path(&artifact.path).ok_or_else(|| AcquisitionError::UnsafeLibraryPath {
            library_name: library_name.to_owned(),
            path: artifact.path.clone(),
        })?;
    download_info_item(
        &format!("library {library_name}"),
        &artifact.url,
        artifact.size,
        &artifact.sha1,
        libraries_root.join(relative_path),
    )
}

fn download_info_item(
    component: &str,
    url: &Option<String>,
    size: Option<u64>,
    sha1: &Option<String>,
    destination: PathBuf,
) -> Result<DownloadItem, AcquisitionError> {
    let url_value = url.as_ref().ok_or_else(|| missing(component, "url"))?;
    let url = reqwest::Url::parse(url_value).map_err(|source| AcquisitionError::InvalidUrl {
        component: component.to_owned(),
        value: url_value.clone(),
        source,
    })?;
    let expected_size = size.ok_or_else(|| missing(component, "size"))?;
    let expected_sha1 = sha1.as_ref().ok_or_else(|| missing(component, "sha1"))?;
    let expected_sha1 = validated_sha1(expected_sha1).map_err(AcquisitionError::InvalidAsset)?;
    Ok(DownloadItem {
        url,
        destination,
        expected_size,
        expected_sha1,
    })
}

fn asset_download_items(
    asset_index: &AssetIndexFile,
    assets_root: &Path,
    config: &VanillaAcquisitionConfig,
) -> Result<Vec<DownloadItem>, AcquisitionError> {
    let mut items = Vec::with_capacity(asset_index.objects.len());
    for asset in asset_index.objects.values() {
        let destination =
            asset_object_path(assets_root, &asset.hash).map_err(AcquisitionError::InvalidAsset)?;
        let url = asset_object_url(&config.assets_base_url, &asset.hash)
            .map_err(AcquisitionError::InvalidAsset)?;
        items.push(DownloadItem {
            url,
            destination,
            expected_size: asset.size,
            expected_sha1: validated_sha1(&asset.hash).map_err(AcquisitionError::InvalidAsset)?,
        });
    }
    deduplicate_items(items)
}

fn deduplicate_items(items: Vec<DownloadItem>) -> Result<Vec<DownloadItem>, AcquisitionError> {
    let mut by_destination = HashMap::<PathBuf, DownloadItem>::new();
    for item in items {
        match by_destination.get(&item.destination) {
            Some(existing)
                if existing.expected_size != item.expected_size
                    || existing.expected_sha1 != item.expected_sha1
                    || existing.url != item.url =>
            {
                return Err(AcquisitionError::ConflictingDestination {
                    path: item.destination,
                });
            }
            Some(_) => {}
            None => {
                by_destination.insert(item.destination.clone(), item);
            }
        }
    }
    Ok(by_destination.into_values().collect())
}

async fn read_asset_index(path: &Path) -> Result<AssetIndexFile, AcquisitionError> {
    let metadata =
        tokio::fs::metadata(path)
            .await
            .map_err(|source| AcquisitionError::ReadAssetIndex {
                path: path.to_owned(),
                source,
            })?;
    if metadata.len() > MAX_ASSET_INDEX_SIZE as u64 {
        return Err(AcquisitionError::AssetIndexTooLarge {
            limit: MAX_ASSET_INDEX_SIZE,
        });
    }
    let contents =
        tokio::fs::read(path)
            .await
            .map_err(|source| AcquisitionError::ReadAssetIndex {
                path: path.to_owned(),
                source,
            })?;
    AssetIndexFile::from_json(&contents).map_err(AcquisitionError::InvalidAssetIndex)
}

async fn run_download_batch(
    client: &reqwest::Client,
    items: Vec<DownloadItem>,
    max_concurrency: usize,
    cancellation: &AcquisitionCancellation,
    reporter: Arc<ProgressReporter>,
) -> Result<AcquisitionSummary, AcquisitionError> {
    let mut items = items.into_iter();
    let mut tasks = JoinSet::new();
    let mut summary = AcquisitionSummary::default();

    loop {
        while tasks.len() < max_concurrency && !cancellation.is_cancelled() {
            let Some(item) = items.next() else {
                break;
            };
            let client = client.clone();
            let cancellation = cancellation.clone();
            let task_reporter = Arc::clone(&reporter);
            tasks.spawn(async move {
                let chunk_reporter = Arc::clone(&task_reporter);
                let disposition = ensure_verified_file(
                    &client,
                    &item.url,
                    &item.destination,
                    item.expected_size,
                    &item.expected_sha1,
                    &cancellation,
                    &move |bytes| chunk_reporter.complete(bytes),
                )
                .await?;
                Ok::<_, RuntimeDownloadError>((disposition, item.expected_size))
            });
        }

        if tasks.is_empty() {
            if cancellation.is_cancelled() {
                return Err(AcquisitionError::Cancelled);
            }
            break;
        }

        let result = tasks
            .join_next()
            .await
            .expect("download task set is not empty")
            .map_err(|source| AcquisitionError::DownloadTask { source })?;
        match result {
            Ok((DownloadDisposition::AlreadyValid, size)) => {
                reporter.complete(size);
                summary.reused_files += 1;
            }
            Ok((DownloadDisposition::Downloaded, _)) => summary.downloaded_files += 1,
            Err(RuntimeDownloadError::Cancelled) => {
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                return Err(AcquisitionError::Cancelled);
            }
            Err(source) => {
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                return Err(AcquisitionError::Download(source));
            }
        }
    }
    Ok(summary)
}

struct ProgressReporter {
    completed: AtomicU64,
    total: AtomicU64,
    callback: Arc<dyn Fn(AcquisitionProgress) + Send + Sync>,
}

impl ProgressReporter {
    fn new(callback: Arc<dyn Fn(AcquisitionProgress) + Send + Sync>) -> Self {
        Self {
            completed: AtomicU64::new(0),
            total: AtomicU64::new(0),
            callback,
        }
    }

    fn add_total(&self, bytes: u64) {
        self.total.fetch_add(bytes, Ordering::AcqRel);
        self.emit();
    }

    fn complete(&self, bytes: u64) {
        self.completed.fetch_add(bytes, Ordering::AcqRel);
        self.emit();
    }

    fn snapshot(&self) -> AcquisitionProgress {
        AcquisitionProgress {
            completed_bytes: self.completed.load(Ordering::Acquire),
            total_bytes: self.total.load(Ordering::Acquire),
        }
    }

    fn emit(&self) {
        (self.callback)(self.snapshot());
    }
}

fn sum_sizes(items: &[DownloadItem]) -> u64 {
    items.iter().fold(0_u64, |total, item| {
        total.saturating_add(item.expected_size)
    })
}

fn missing(component: &str, field: &'static str) -> AcquisitionError {
    AcquisitionError::MissingDownloadMetadata {
        component: component.to_owned(),
        field,
    }
}

fn safe_relative_path(path: &str) -> Option<PathBuf> {
    if path.is_empty()
        || path.starts_with('/')
        || path.starts_with('\\')
        || has_windows_drive_prefix(path)
        || path.contains('\0')
    {
        return None;
    }
    let mut relative = PathBuf::new();
    for component in path.split(['/', '\\']) {
        match component {
            "" | "." => {}
            ".." => return None,
            component if component.contains(':') => return None,
            component => relative.push(component),
        }
    }
    (!relative.as_os_str().is_empty()).then_some(relative)
}

fn has_windows_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn is_safe_component(value: &str) -> bool {
    !value.is_empty() && value != "." && value != ".." && !value.contains(['/', '\\', ':', '\0'])
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use serde_json::json;
    use sha1::{Digest, Sha1};

    use crate::{
        build_http_client,
        http::test_support::{TestResponse, TestServer},
        validate_vanilla_runtime,
    };

    use super::*;

    #[tokio::test]
    async fn fetches_version_metadata() {
        let server = TestServer::start().await.unwrap();
        let body = minimal_metadata_json(server.url("client.jar").unwrap().as_str(), b"client")
            .to_string();
        server.route("/version.json", TestResponse::ok(body));

        let metadata = fetch_version_metadata(
            &build_http_client().unwrap(),
            &server.url("version.json").unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(metadata.id, "test-version");
        assert_eq!(metadata.downloads.client.unwrap().size, Some(6));
    }

    #[tokio::test]
    async fn version_metadata_404_is_typed() {
        let server = TestServer::start().await.unwrap();

        let error = fetch_version_metadata(
            &build_http_client().unwrap(),
            &server.url("missing.json").unwrap(),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            VersionMetadataFetchError::HttpStatus { status, .. }
                if status == reqwest::StatusCode::NOT_FOUND
        ));
    }

    #[tokio::test]
    async fn rejects_oversized_version_metadata() {
        let server = TestServer::start().await.unwrap();
        server.route(
            "/large.json",
            TestResponse::ok(b"{}".to_vec()).with_advertised_length(MAX_VERSION_METADATA_SIZE + 1),
        );

        let error = fetch_version_metadata(
            &build_http_client().unwrap(),
            &server.url("large.json").unwrap(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, VersionMetadataFetchError::TooLarge { .. }));
    }

    #[tokio::test]
    async fn valid_existing_client_is_reused_without_http_request() {
        let server = TestServer::start().await.unwrap();
        let contents = b"existing client";
        server.route("/client.jar", TestResponse::status(500));
        let metadata = metadata_from_value(minimal_metadata_json(
            server.url("client.jar").unwrap().as_str(),
            contents,
        ));
        let temp = tempfile::tempdir().unwrap();
        let layout = MinecraftRuntimeLayout::new(temp.path());
        write_file(&layout.client_jar(&metadata.id), contents);

        let summary = acquire(
            &metadata,
            &layout,
            VanillaAcquisitionConfig::new(server.url("assets/objects/").unwrap()),
            AcquisitionCancellation::default(),
            |_| {},
        )
        .await
        .unwrap();

        assert_eq!(summary.reused_files, 1);
        assert_eq!(summary.downloaded_files, 0);
        assert_eq!(server.request_count("/client.jar"), 0);
    }

    #[tokio::test]
    async fn corrupt_client_is_replaced() {
        let server = TestServer::start().await.unwrap();
        let contents = b"correct client";
        server.route("/client.jar", TestResponse::ok(contents.to_vec()));
        let metadata = metadata_from_value(minimal_metadata_json(
            server.url("client.jar").unwrap().as_str(),
            contents,
        ));
        let temp = tempfile::tempdir().unwrap();
        let layout = MinecraftRuntimeLayout::new(temp.path());
        write_file(&layout.client_jar(&metadata.id), b"corrupt client");

        let summary = acquire(
            &metadata,
            &layout,
            VanillaAcquisitionConfig::new(server.url("assets/objects/").unwrap()),
            AcquisitionCancellation::default(),
            |_| {},
        )
        .await
        .unwrap();

        assert_eq!(summary.downloaded_files, 1);
        assert_eq!(fs::read(layout.client_jar(&metadata.id)).unwrap(), contents);
        assert_eq!(server.request_count("/client.jar"), 1);
    }

    #[tokio::test]
    async fn unsafe_library_path_is_rejected_before_downloads() {
        let server = TestServer::start().await.unwrap();
        let metadata = metadata_from_value(json!({
            "id": "unsafe",
            "mainClass": "Main",
            "assets": "",
            "downloads": { "client": download_json(server.url("client.jar").unwrap().as_str(), b"client") },
            "libraries": [{
                "name": "test:unsafe:1",
                "downloads": { "artifact": artifact_json("../escape.jar", server.url("library.jar").unwrap().as_str(), b"library") }
            }]
        }));
        let temp = tempfile::tempdir().unwrap();

        let error = acquire(
            &metadata,
            &MinecraftRuntimeLayout::new(temp.path()),
            VanillaAcquisitionConfig::new(server.url("assets/objects/").unwrap()),
            AcquisitionCancellation::default(),
            |_| {},
        )
        .await
        .unwrap_err();

        assert!(matches!(error, AcquisitionError::UnsafeLibraryPath { .. }));
        assert_eq!(server.request_count("/client.jar"), 0);
    }

    #[tokio::test]
    async fn invalid_asset_sha1_is_rejected_after_index_parse() {
        let server = TestServer::start().await.unwrap();
        let index = json!({
            "objects": { "bad/object": { "hash": "not-a-sha1", "size": 4 } }
        })
        .to_string()
        .into_bytes();
        server.route("/client.jar", TestResponse::ok(b"client".to_vec()));
        server.route("/index.json", TestResponse::ok(index.clone()));
        let metadata = metadata_from_value(json!({
            "id": "bad-assets",
            "mainClass": "Main",
            "assets": "bad",
            "assetIndex": {
                "id": "bad",
                "url": server.url("index.json").unwrap().as_str(),
                "sha1": sha1_hex(&index),
                "size": index.len()
            },
            "downloads": { "client": download_json(server.url("client.jar").unwrap().as_str(), b"client") },
            "libraries": []
        }));
        let temp = tempfile::tempdir().unwrap();

        let error = acquire(
            &metadata,
            &MinecraftRuntimeLayout::new(temp.path()),
            VanillaAcquisitionConfig::new(server.url("assets/objects/").unwrap()),
            AcquisitionCancellation::default(),
            |_| {},
        )
        .await
        .unwrap_err();

        assert!(matches!(error, AcquisitionError::InvalidAsset(_)));
    }

    #[tokio::test]
    async fn bounded_concurrency_never_exceeds_configuration() {
        let server = TestServer::start().await.unwrap();
        let fixture = FullFixture::new(&server, true);
        let temp = tempfile::tempdir().unwrap();
        let mut config = VanillaAcquisitionConfig::new(server.url("assets/objects/").unwrap());
        config.max_concurrency = 2;

        acquire(
            &fixture.metadata,
            &MinecraftRuntimeLayout::new(temp.path()),
            config,
            AcquisitionCancellation::default(),
            |_| {},
        )
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            while server.active_requests() != 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("test server requests should finish");
        assert_eq!(server.max_active_requests(), 2);
    }

    #[tokio::test]
    async fn acquisition_cancellation_stops_streams_and_cleans_temps() {
        let server = TestServer::start().await.unwrap();
        let contents = vec![9_u8; 256 * 1024];
        server.route(
            "/client.jar",
            TestResponse::slow(contents.clone(), 1024, Duration::from_millis(10)),
        );
        let metadata = metadata_from_value(minimal_metadata_json(
            server.url("client.jar").unwrap().as_str(),
            &contents,
        ));
        let temp = tempfile::tempdir().unwrap();
        let layout = MinecraftRuntimeLayout::new(temp.path());
        let cancellation = AcquisitionCancellation::default();
        let task_cancellation = cancellation.clone();
        let client = build_http_client().unwrap();
        let config = VanillaAcquisitionConfig::new(server.url("assets/objects/").unwrap());
        let task_layout = layout.clone();
        let task = tokio::spawn(async move {
            acquire_vanilla_runtime(
                &client,
                &metadata,
                &task_layout,
                OperatingSystem::Windows,
                &config,
                &task_cancellation,
                |_| {},
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;

        cancellation.cancel();
        let error = task.await.unwrap().unwrap_err();

        assert!(matches!(error, AcquisitionError::Cancelled));
        assert!(!layout.client_jar("test-version").exists());
        assert!(walk_files(layout.root()).is_empty());
    }

    #[tokio::test]
    async fn full_acquisition_downloads_runtime_and_reaches_total_progress() {
        let server = TestServer::start().await.unwrap();
        let fixture = FullFixture::new(&server, false);
        let temp = tempfile::tempdir().unwrap();
        let layout = MinecraftRuntimeLayout::new(temp.path());
        let progress = Arc::new(Mutex::new(Vec::new()));
        let callback_progress = Arc::clone(&progress);

        let summary = acquire(
            &fixture.metadata,
            &layout,
            VanillaAcquisitionConfig::new(server.url("assets/objects/").unwrap()),
            AcquisitionCancellation::default(),
            move |update| callback_progress.lock().unwrap().push(update),
        )
        .await
        .unwrap();

        assert_eq!(summary.downloaded_files, 7);
        assert_eq!(
            fs::read(layout.client_jar("full-test")).unwrap(),
            fixture.client
        );
        assert_eq!(
            fs::read(layout.libraries_dir().join("org/test/lib/1/lib.jar")).unwrap(),
            fixture.library
        );
        assert_eq!(
            fs::read(layout.libraries_dir().join(&fixture.native_classifier_path)).unwrap(),
            fixture.native_classifier
        );
        assert_eq!(
            fs::read(layout.assets_dir().join("indexes/test-assets.json")).unwrap(),
            fixture.asset_index
        );
        for object in &fixture.asset_objects {
            assert_eq!(
                fs::read(asset_object_path(&layout.assets_dir(), &object.hash).unwrap()).unwrap(),
                object.contents
            );
        }
        let final_progress = progress.lock().unwrap().last().copied().unwrap();
        assert_eq!(final_progress.completed_bytes, final_progress.total_bytes);
        assert_eq!(final_progress.total_bytes, summary.total_bytes);
        assert!(
            validate_vanilla_runtime(&fixture.metadata, &layout, OperatingSystem::Windows)
                .unwrap()
                .is_valid()
        );
    }

    async fn acquire<F>(
        metadata: &VersionMetadata,
        layout: &MinecraftRuntimeLayout,
        config: VanillaAcquisitionConfig,
        cancellation: AcquisitionCancellation,
        on_progress: F,
    ) -> Result<AcquisitionSummary, AcquisitionError>
    where
        F: Fn(AcquisitionProgress) + Send + Sync + 'static,
    {
        acquire_vanilla_runtime(
            &build_http_client().unwrap(),
            metadata,
            layout,
            OperatingSystem::Windows,
            &config,
            &cancellation,
            on_progress,
        )
        .await
    }

    struct AssetFixture {
        hash: String,
        contents: Vec<u8>,
    }

    struct FullFixture {
        metadata: VersionMetadata,
        client: Vec<u8>,
        library: Vec<u8>,
        native_classifier: Vec<u8>,
        native_classifier_path: String,
        asset_index: Vec<u8>,
        asset_objects: Vec<AssetFixture>,
    }

    impl FullFixture {
        fn new(server: &TestServer, slow: bool) -> Self {
            let client = b"synthetic client".to_vec();
            let library = b"synthetic library".to_vec();
            let native_library = b"native classpath library".to_vec();
            let native_classifier = b"native classifier jar".to_vec();
            let classifier = if cfg!(target_pointer_width = "64") {
                "natives-windows-64"
            } else {
                "natives-windows-32"
            };
            let native_classifier_path = format!("org/test/native/1/native-1-{classifier}.jar");
            let asset_objects = vec![
                AssetFixture::new(b"first asset object"),
                AssetFixture::new(b"second asset object"),
            ];
            let asset_index = json!({
                "objects": {
                    "minecraft/first.bin": { "hash": &asset_objects[0].hash, "size": asset_objects[0].contents.len() },
                    "minecraft/second.bin": { "hash": &asset_objects[1].hash, "size": asset_objects[1].contents.len() }
                }
            })
            .to_string()
            .into_bytes();
            route(server, "/runtime/client.jar", &client, slow);
            route(server, "/runtime/library.jar", &library, slow);
            route(server, "/runtime/native.jar", &native_library, slow);
            route(
                server,
                "/runtime/native-classifier.jar",
                &native_classifier,
                slow,
            );
            route(server, "/runtime/index.json", &asset_index, slow);
            for object in &asset_objects {
                route(
                    server,
                    &format!("/assets/objects/{}/{}", &object.hash[..2], object.hash),
                    &object.contents,
                    slow,
                );
            }
            let mut classifiers = serde_json::Map::new();
            classifiers.insert(
                classifier.to_owned(),
                artifact_json(
                    &native_classifier_path,
                    server
                        .url("runtime/native-classifier.jar")
                        .unwrap()
                        .as_str(),
                    &native_classifier,
                ),
            );
            let metadata = metadata_from_value(json!({
                "id": "full-test",
                "mainClass": "net.minecraft.client.main.Main",
                "assets": "test-assets",
                "assetIndex": {
                    "id": "test-assets",
                    "url": server.url("runtime/index.json").unwrap().as_str(),
                    "sha1": sha1_hex(&asset_index),
                    "size": asset_index.len()
                },
                "downloads": {
                    "client": download_json(server.url("runtime/client.jar").unwrap().as_str(), &client)
                },
                "libraries": [
                    {
                        "name": "org.test:lib:1",
                        "downloads": {
                            "artifact": artifact_json(
                                "org/test/lib/1/lib.jar",
                                server.url("runtime/library.jar").unwrap().as_str(),
                                &library
                            )
                        }
                    },
                    {
                        "name": "org.test:native:1",
                        "natives": { "windows": "natives-windows-${arch}" },
                        "downloads": {
                            "artifact": artifact_json(
                                "org/test/native/1/native.jar",
                                server.url("runtime/native.jar").unwrap().as_str(),
                                &native_library
                            ),
                            "classifiers": classifiers
                        }
                    }
                ],
                "arguments": { "jvm": [], "game": [] }
            }));
            Self {
                metadata,
                client,
                library,
                native_classifier,
                native_classifier_path,
                asset_index,
                asset_objects,
            }
        }
    }

    impl AssetFixture {
        fn new(contents: &[u8]) -> Self {
            Self {
                hash: sha1_hex(contents),
                contents: contents.to_vec(),
            }
        }
    }

    fn minimal_metadata_json(client_url: &str, contents: &[u8]) -> serde_json::Value {
        json!({
            "id": "test-version",
            "mainClass": "Main",
            "assets": "",
            "downloads": { "client": download_json(client_url, contents) },
            "libraries": []
        })
    }

    fn download_json(url: &str, contents: &[u8]) -> serde_json::Value {
        json!({ "url": url, "sha1": sha1_hex(contents), "size": contents.len() })
    }

    fn artifact_json(path: &str, url: &str, contents: &[u8]) -> serde_json::Value {
        json!({
            "path": path,
            "url": url,
            "sha1": sha1_hex(contents),
            "size": contents.len()
        })
    }

    fn metadata_from_value(value: serde_json::Value) -> VersionMetadata {
        VersionMetadata::from_json(&value.to_string()).unwrap()
    }

    fn route(server: &TestServer, path: &str, contents: &[u8], slow: bool) {
        let response = if slow {
            TestResponse::slow(contents.to_vec(), 64, Duration::from_millis(15))
        } else {
            TestResponse::ok(contents.to_vec())
        };
        server.route(path, response);
    }

    fn sha1_hex(contents: &[u8]) -> String {
        format!("{:x}", Sha1::digest(contents))
    }

    fn write_file(path: &Path, contents: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn walk_files(root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let mut pending = vec![root.to_owned()];
        while let Some(path) = pending.pop() {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    if entry.path().is_dir() {
                        pending.push(entry.path());
                    } else {
                        files.push(entry.path());
                    }
                }
            }
        }
        files
    }
}
