use std::{error::Error, fmt, time::Duration};

use crate::{
    ManifestFile,
    manifest::PackManifest,
    source::{FileChunkStream, FileSource, FileSourceError, SourceFuture},
    verification::validated_manifest_path_components,
};

pub const MAX_MANIFEST_SIZE: usize = 4 * 1024 * 1024;
pub(crate) const METADATA_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REDIRECTS: usize = 5;
const USER_AGENT: &str = "EquestrianDreamLauncher/0.1";

pub fn build_http_client() -> Result<reqwest::Client, HttpClientBuildError> {
    build_http_client_with_read_timeout(HTTP_READ_TIMEOUT)
}

pub(crate) fn build_http_client_with_read_timeout(
    timeout: Duration,
) -> Result<reqwest::Client, HttpClientBuildError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
        // A progressing archive may take minutes to finish. Limit inactivity,
        // not its full duration; metadata requests set their own total deadline.
        .read_timeout(timeout)
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(HttpClientBuildError)
}

#[derive(Debug)]
pub struct HttpClientBuildError(reqwest::Error);

impl fmt::Display for HttpClientBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("failed to build launcher HTTP client")
    }
}

impl Error for HttpClientBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

pub async fn fetch_manifest(
    client: &reqwest::Client,
    url: &reqwest::Url,
) -> Result<PackManifest, ManifestFetchError> {
    let mut response = client
        .get(url.clone())
        .timeout(METADATA_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|source| ManifestFetchError::Request {
            url: url.clone(),
            source,
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(ManifestFetchError::HttpStatus {
            url: url.clone(),
            status,
        });
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MANIFEST_SIZE as u64)
    {
        return Err(ManifestFetchError::TooLarge {
            limit: MAX_MANIFEST_SIZE,
        });
    }

    let mut body = Vec::new();
    while let Some(chunk) =
        response
            .chunk()
            .await
            .map_err(|source| ManifestFetchError::Request {
                url: url.clone(),
                source,
            })?
    {
        let next_size =
            body.len()
                .checked_add(chunk.len())
                .ok_or(ManifestFetchError::TooLarge {
                    limit: MAX_MANIFEST_SIZE,
                })?;
        if next_size > MAX_MANIFEST_SIZE {
            return Err(ManifestFetchError::TooLarge {
                limit: MAX_MANIFEST_SIZE,
            });
        }
        body.extend_from_slice(&chunk);
    }

    serde_json::from_slice(&body).map_err(ManifestFetchError::InvalidJson)
}

#[derive(Debug)]
pub enum ManifestFetchError {
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

impl fmt::Display for ManifestFetchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request { url, .. } => write!(formatter, "failed to fetch manifest from {url}"),
            Self::HttpStatus { url, status } => {
                write!(formatter, "manifest request for {url} returned {status}")
            }
            Self::TooLarge { limit } => {
                write!(formatter, "manifest exceeds the {limit} byte size limit")
            }
            Self::InvalidJson(_) => formatter.write_str("remote manifest contains invalid JSON"),
        }
    }
}

impl Error for ManifestFetchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Request { source, .. } => Some(source),
            Self::InvalidJson(source) => Some(source),
            Self::HttpStatus { .. } | Self::TooLarge { .. } => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HttpFileSource {
    client: reqwest::Client,
    base_url: reqwest::Url,
}

impl HttpFileSource {
    pub fn new(
        client: reqwest::Client,
        base_url: reqwest::Url,
    ) -> Result<Self, HttpFileSourceConfigError> {
        Ok(Self {
            client,
            base_url: normalize_files_base_url(base_url)?,
        })
    }

    pub fn base_url(&self) -> &reqwest::Url {
        &self.base_url
    }

    pub fn file_url(&self, file: &ManifestFile) -> Result<reqwest::Url, FileSourceError> {
        let components = validated_manifest_path_components(&file.path)?;
        let mut url = self.base_url.clone();
        let mut path = url.path_segments_mut().map_err(|_| {
            FileSourceError::UnsafePath(
                crate::verification::VerificationError::InvalidManifestPath {
                    path: file.path.clone(),
                    reason: crate::verification::InvalidManifestPathReason::Absolute,
                },
            )
        })?;
        path.pop_if_empty();
        for component in components {
            path.push(&component);
        }
        drop(path);
        Ok(url)
    }
}

impl FileSource for HttpFileSource {
    fn fetch<'a>(
        &'a self,
        file: &'a ManifestFile,
    ) -> SourceFuture<'a, Result<Box<dyn FileChunkStream>, FileSourceError>> {
        let file_url = self.file_url(file);
        Box::pin(async move {
            let url = file_url?;
            let response = self
                .client
                .get(url.clone())
                .send()
                .await
                .map_err(|source| FileSourceError::HttpRequest {
                    url: url.clone(),
                    source,
                })?;
            let status = response.status();
            if !status.is_success() {
                return Err(FileSourceError::HttpStatus { url, status });
            }

            Ok(Box::new(HttpFileStream { response, url }) as Box<dyn FileChunkStream>)
        })
    }
}

struct HttpFileStream {
    response: reqwest::Response,
    url: reqwest::Url,
}

impl FileChunkStream for HttpFileStream {
    fn next_chunk(&mut self) -> SourceFuture<'_, Result<Option<Vec<u8>>, FileSourceError>> {
        Box::pin(async move {
            self.response
                .chunk()
                .await
                .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
                .map_err(|source| FileSourceError::HttpRequest {
                    url: self.url.clone(),
                    source,
                })
        })
    }
}

#[derive(Clone, Debug)]
pub struct RemotePackConfig {
    manifest_url: reqwest::Url,
    files_base_url: reqwest::Url,
}

impl RemotePackConfig {
    pub fn new(
        manifest_url: reqwest::Url,
        files_base_url: reqwest::Url,
    ) -> Result<Self, RemotePackConfigError> {
        validate_http_url(&manifest_url).map_err(RemotePackConfigError::ManifestUrl)?;
        let files_base_url = normalize_files_base_url(files_base_url)
            .map_err(RemotePackConfigError::FilesBaseUrl)?;
        Ok(Self {
            manifest_url,
            files_base_url,
        })
    }

    pub fn manifest_url(&self) -> &reqwest::Url {
        &self.manifest_url
    }

    pub fn files_base_url(&self) -> &reqwest::Url {
        &self.files_base_url
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpUrlError {
    UnsupportedScheme,
    MissingHost,
    CredentialsNotAllowed,
    QueryOrFragmentNotAllowed,
    CannotBeBase,
}

impl fmt::Display for HttpUrlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::UnsupportedScheme => "URL scheme must be http or https",
            Self::MissingHost => "URL must contain a host",
            Self::CredentialsNotAllowed => "URL credentials are not allowed",
            Self::QueryOrFragmentNotAllowed => "base URL query and fragment are not allowed",
            Self::CannotBeBase => "URL cannot be used as a hierarchical base",
        };
        formatter.write_str(message)
    }
}

impl Error for HttpUrlError {}

#[derive(Debug)]
pub struct HttpFileSourceConfigError(HttpUrlError);

impl fmt::Display for HttpFileSourceConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid HTTP files base URL: {}", self.0)
    }
}

impl Error for HttpFileSourceConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

#[derive(Debug)]
pub enum RemotePackConfigError {
    ManifestUrl(HttpUrlError),
    FilesBaseUrl(HttpFileSourceConfigError),
}

impl fmt::Display for RemotePackConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ManifestUrl(error) => write!(formatter, "invalid manifest URL: {error}"),
            Self::FilesBaseUrl(error) => error.fmt(formatter),
        }
    }
}

impl Error for RemotePackConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ManifestUrl(error) => Some(error),
            Self::FilesBaseUrl(error) => Some(error),
        }
    }
}

fn normalize_files_base_url(
    mut url: reqwest::Url,
) -> Result<reqwest::Url, HttpFileSourceConfigError> {
    validate_http_url(&url).map_err(HttpFileSourceConfigError)?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(HttpFileSourceConfigError(
            HttpUrlError::QueryOrFragmentNotAllowed,
        ));
    }
    if !url.path().ends_with('/') {
        url.path_segments_mut()
            .map_err(|_| HttpFileSourceConfigError(HttpUrlError::CannotBeBase))?
            .push("");
    }
    Ok(url)
}

pub(crate) fn validate_http_url(url: &reqwest::Url) -> Result<(), HttpUrlError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(HttpUrlError::UnsupportedScheme);
    }
    if url.host_str().is_none() {
        return Err(HttpUrlError::MissingHost);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(HttpUrlError::CredentialsNotAllowed);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::{
        collections::HashMap,
        io,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        task::JoinHandle,
    };

    #[derive(Clone)]
    pub(crate) struct TestResponse {
        status: u16,
        body: Vec<u8>,
        chunk_size: usize,
        chunk_delay: Duration,
        advertised_length: Option<usize>,
        chunked: bool,
    }

    impl TestResponse {
        pub(crate) fn ok(body: impl Into<Vec<u8>>) -> Self {
            Self {
                status: 200,
                body: body.into(),
                chunk_size: usize::MAX,
                chunk_delay: Duration::ZERO,
                advertised_length: None,
                chunked: false,
            }
        }

        pub(crate) fn status(status: u16) -> Self {
            Self {
                status,
                body: Vec::new(),
                chunk_size: usize::MAX,
                chunk_delay: Duration::ZERO,
                advertised_length: None,
                chunked: false,
            }
        }

        pub(crate) fn slow(
            body: impl Into<Vec<u8>>,
            chunk_size: usize,
            chunk_delay: Duration,
        ) -> Self {
            Self {
                status: 200,
                body: body.into(),
                chunk_size,
                chunk_delay,
                advertised_length: None,
                chunked: false,
            }
        }

        pub(crate) fn with_advertised_length(mut self, length: usize) -> Self {
            self.advertised_length = Some(length);
            self
        }

        pub(crate) fn chunked(mut self) -> Self {
            self.chunked = true;
            self
        }

        pub(crate) fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
            self.body = body.into();
            self
        }
    }

    pub(crate) struct TestServer {
        base_url: reqwest::Url,
        routes: Arc<Mutex<HashMap<String, TestResponse>>>,
        request_counts: Arc<Mutex<HashMap<String, usize>>>,
        active_requests: Arc<AtomicUsize>,
        max_active_requests: Arc<AtomicUsize>,
        task: JoinHandle<()>,
    }

    impl TestServer {
        pub(crate) async fn start() -> Result<Self, Box<dyn std::error::Error>> {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let address = listener.local_addr()?;
            let routes = Arc::new(Mutex::new(HashMap::new()));
            let server_routes = Arc::clone(&routes);
            let request_counts = Arc::new(Mutex::new(HashMap::new()));
            let server_request_counts = Arc::clone(&request_counts);
            let active_requests = Arc::new(AtomicUsize::new(0));
            let server_active_requests = Arc::clone(&active_requests);
            let max_active_requests = Arc::new(AtomicUsize::new(0));
            let server_max_active_requests = Arc::clone(&max_active_requests);
            let task = tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let connection_routes = Arc::clone(&server_routes);
                    let connection_request_counts = Arc::clone(&server_request_counts);
                    let connection_active_requests = Arc::clone(&server_active_requests);
                    let connection_max_active_requests = Arc::clone(&server_max_active_requests);
                    let connection_task = tokio::spawn(async move {
                        let _result = handle_connection(
                            stream,
                            connection_routes,
                            connection_request_counts,
                            connection_active_requests,
                            connection_max_active_requests,
                        )
                        .await;
                    });
                    drop(connection_task);
                }
            });

            Ok(Self {
                base_url: reqwest::Url::parse(&format!("http://{address}/"))?,
                routes,
                request_counts,
                active_requests,
                max_active_requests,
                task,
            })
        }

        pub(crate) fn route(&self, path: &str, response: TestResponse) {
            self.routes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(path.to_owned(), response);
        }

        pub(crate) fn url(&self, path: &str) -> Result<reqwest::Url, url::ParseError> {
            self.base_url.join(path)
        }

        pub(crate) fn host_str(&self) -> Option<&str> {
            self.base_url.host_str()
        }

        pub(crate) fn request_count(&self, path: &str) -> usize {
            self.request_counts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(path)
                .copied()
                .unwrap_or(0)
        }

        pub(crate) fn max_active_requests(&self) -> usize {
            self.max_active_requests.load(Ordering::Acquire)
        }

        pub(crate) fn active_requests(&self) -> usize {
            self.active_requests.load(Ordering::Acquire)
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn handle_connection(
        mut stream: TcpStream,
        routes: Arc<Mutex<HashMap<String, TestResponse>>>,
        request_counts: Arc<Mutex<HashMap<String, usize>>>,
        active_requests: Arc<AtomicUsize>,
        max_active_requests: Arc<AtomicUsize>,
    ) -> io::Result<()> {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read_count = stream.read(&mut buffer).await?;
            if read_count == 0 || request.len() > 16 * 1024 {
                return Ok(());
            }
            request.extend_from_slice(&buffer[..read_count]);
        }
        let request = String::from_utf8_lossy(&request);
        let path = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/");
        *request_counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(path.to_owned())
            .or_insert(0) += 1;
        let active = active_requests.fetch_add(1, Ordering::AcqRel) + 1;
        max_active_requests.fetch_max(active, Ordering::AcqRel);
        let mut active_request_guard = Some(ActiveRequestGuard(active_requests));
        let response = routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(path)
            .cloned()
            .unwrap_or_else(|| TestResponse::status(404));
        let reason = match response.status {
            200 => "OK",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "Test Status",
        };
        let content_length = response.advertised_length.unwrap_or(response.body.len());
        let headers = if response.chunked {
            format!(
                "HTTP/1.1 {} {}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                response.status, reason
            )
        } else {
            format!(
                "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.status, reason, content_length
            )
        };
        stream.write_all(headers.as_bytes()).await?;

        let chunk_size = response.chunk_size.max(1);
        let chunk_count = response.body.len().div_ceil(chunk_size);
        for (index, chunk) in response.body.chunks(chunk_size).enumerate() {
            if !response.chunk_delay.is_zero() {
                tokio::time::sleep(response.chunk_delay).await;
            }
            if index + 1 == chunk_count {
                drop(active_request_guard.take());
            }
            if response.chunked {
                stream
                    .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                    .await?;
            }
            stream.write_all(chunk).await?;
            if response.chunked {
                stream.write_all(b"\r\n").await?;
            }
            stream.flush().await?;
        }
        if response.chunked {
            stream.write_all(b"0\r\n\r\n").await?;
        }
        Ok(())
    }

    struct ActiveRequestGuard(Arc<AtomicUsize>);

    impl Drop for ActiveRequestGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, fs, time::Duration};

    use sha2::{Digest, Sha256};

    use super::{
        test_support::{TestResponse, TestServer},
        *,
    };
    use crate::{
        InstallCancellation, InstallError, ManifestFile, PackManifest, install_files,
        verify_installation,
    };

    #[tokio::test]
    async fn fetches_valid_manifest() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let manifest = manifest_with(Vec::new());
        server.route(
            "/manifest.json",
            TestResponse::ok(manifest.to_json_pretty()?),
        );

        let fetched = fetch_manifest(&build_http_client()?, &server.url("manifest.json")?).await?;

        assert_eq!(fetched, manifest);
        Ok(())
    }

    #[tokio::test]
    async fn rejects_invalid_manifest_json() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route("/manifest.json", TestResponse::ok("{invalid"));

        let error = fetch_manifest(&build_http_client()?, &server.url("manifest.json")?)
            .await
            .unwrap_err();

        assert!(matches!(error, ManifestFetchError::InvalidJson(_)));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_manifest_error_statuses() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let client = build_http_client()?;
        for status in [404, 500] {
            let path = format!("/manifest-{status}.json");
            server.route(&path, TestResponse::status(status));
            let error = fetch_manifest(&client, &server.url(path.trim_start_matches('/'))?)
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                ManifestFetchError::HttpStatus { status: actual, .. }
                    if actual.as_u16() == status
            ));
        }
        Ok(())
    }

    #[tokio::test]
    async fn rejects_manifest_over_size_limit() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route(
            "/manifest.json",
            TestResponse::ok(Vec::new()).with_advertised_length(MAX_MANIFEST_SIZE + 1),
        );

        let error = fetch_manifest(&build_http_client()?, &server.url("manifest.json")?)
            .await
            .unwrap_err();

        assert!(matches!(error, ManifestFetchError::TooLarge { .. }));
        Ok(())
    }

    #[tokio::test]
    async fn streams_file_into_installer_and_reports_complete_progress()
    -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let installation = tempfile::tempdir()?;
        let bytes = vec![5_u8; 192 * 1024 + 7];
        let file = fixture_file("mods/example.jar", &bytes);
        server.route(
            "/files/mods/example.jar",
            TestResponse::slow(bytes, 16 * 1024, Duration::from_millis(1)),
        );
        let manifest = manifest_with(vec![file]);
        let before = verify_installation(installation.path(), &manifest)?;
        let source = HttpFileSource::new(build_http_client()?, server.url("files/")?)?;
        let mut progress = Vec::new();

        install_files(
            installation.path(),
            &manifest,
            &before,
            &source,
            &InstallCancellation::default(),
            |update| progress.push(update),
        )
        .await?;

        assert!(verify_installation(installation.path(), &manifest)?.is_valid());
        assert_eq!(progress.last().map(|value| value.fraction()), Some(1.0));
        assert!(progress.len() > 1);
        Ok(())
    }

    #[tokio::test]
    async fn file_404_is_typed_source_error() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let installation = tempfile::tempdir()?;
        let file = fixture_file("missing.jar", b"expected");
        let manifest = manifest_with(vec![file]);
        let before = verify_installation(installation.path(), &manifest)?;
        let source = HttpFileSource::new(build_http_client()?, server.url("files/")?)?;

        let error = install_files(
            installation.path(),
            &manifest,
            &before,
            &source,
            &InstallCancellation::default(),
            |_| {},
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            InstallError::Source(FileSourceError::HttpStatus { status, .. })
                if status == reqwest::StatusCode::NOT_FOUND
        ));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_http_file_with_wrong_hash_or_size() -> Result<(), Box<dyn Error>> {
        for wrong_size in [false, true] {
            let server = TestServer::start().await?;
            let installation = tempfile::tempdir()?;
            let mut file = fixture_file("client.jar", b"expected");
            if wrong_size {
                file.size += 1;
                server.route("/files/client.jar", TestResponse::ok(b"expected".to_vec()));
            } else {
                server.route("/files/client.jar", TestResponse::ok(b"wrong".to_vec()));
            }
            let manifest = manifest_with(vec![file]);
            let before = verify_installation(installation.path(), &manifest)?;
            let source = HttpFileSource::new(build_http_client()?, server.url("files/")?)?;

            let error = install_files(
                installation.path(),
                &manifest,
                &before,
                &source,
                &InstallCancellation::default(),
                |_| {},
            )
            .await
            .unwrap_err();

            assert!(matches!(error, InstallError::ContentMismatch { .. }));
        }
        Ok(())
    }

    #[tokio::test]
    async fn installs_multiple_http_files() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let installation = tempfile::tempdir()?;
        let files = [
            ("a.bin", b"first".as_slice()),
            ("nested/b.bin", b"second".as_slice()),
        ];
        let mut entries = Vec::new();
        for (path, bytes) in files {
            entries.push(fixture_file(path, bytes));
            server.route(&format!("/files/{path}"), TestResponse::ok(bytes.to_vec()));
        }
        let manifest = manifest_with(entries);
        let before = verify_installation(installation.path(), &manifest)?;
        let source = HttpFileSource::new(build_http_client()?, server.url("files/")?)?;

        let summary = install_files(
            installation.path(),
            &manifest,
            &before,
            &source,
            &InstallCancellation::default(),
            |_| {},
        )
        .await?;

        assert_eq!(summary.installed_files().len(), 2);
        assert!(verify_installation(installation.path(), &manifest)?.is_valid());
        Ok(())
    }

    #[tokio::test]
    async fn cancel_drops_http_stream_preserves_target_and_cleans_temp()
    -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let installation = tempfile::tempdir()?;
        let bytes = vec![8_u8; 256 * 1024];
        let file = fixture_file("client.jar", &bytes);
        fs::write(installation.path().join("client.jar"), b"old target")?;
        server.route(
            "/files/client.jar",
            TestResponse::slow(bytes, 8 * 1024, Duration::from_millis(20)),
        );
        let manifest = manifest_with(vec![file]);
        let before = verify_installation(installation.path(), &manifest)?;
        let source = HttpFileSource::new(build_http_client()?, server.url("files/")?)?;
        let cancellation = InstallCancellation::default();
        let task_cancellation = cancellation.clone();
        let cancel_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            task_cancellation.cancel();
        });

        let result = install_files(
            installation.path(),
            &manifest,
            &before,
            &source,
            &cancellation,
            |_| {},
        )
        .await;
        cancel_task.await?;

        assert!(matches!(result, Err(InstallError::Cancelled)));
        assert_eq!(
            fs::read(installation.path().join("client.jar"))?,
            b"old target"
        );
        assert_eq!(fs::read_dir(installation.path())?.count(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn file_path_cannot_change_host_or_scheme_and_traversal_is_rejected()
    -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let source = HttpFileSource::new(build_http_client()?, server.url("files/")?)?;
        let url_like = fixture_file("https:/evil.example/payload", b"");
        let mapped = source.file_url(&url_like)?;

        assert_eq!(mapped.scheme(), "http");
        assert_eq!(mapped.host_str(), server.host_str());
        assert!(mapped.path().starts_with("/files/"));

        for path in ["../outside", "//evil.example/payload"] {
            let file = fixture_file(path, b"");
            assert!(matches!(
                source.file_url(&file),
                Err(FileSourceError::UnsafePath(_))
            ));
        }
        Ok(())
    }

    fn manifest_with(files: Vec<ManifestFile>) -> PackManifest {
        PackManifest {
            version: "remote-test".to_owned(),
            minecraft_version: "1.21.1".to_owned(),
            files,
        }
    }

    pub(crate) fn fixture_file(path: &str, contents: &[u8]) -> ManifestFile {
        let mut hasher = Sha256::new();
        hasher.update(contents);
        ManifestFile {
            path: path.to_owned(),
            sha256: format!("{:x}", hasher.finalize()),
            size: contents.len() as u64,
        }
    }
}
