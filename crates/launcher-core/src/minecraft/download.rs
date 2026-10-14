use std::{
    error::Error,
    fmt, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use sha1::{Digest, Sha1};
use tempfile::{NamedTempFile, TempPath};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Notify,
};

use super::assets::{AssetPathError, validated_sha1};

const VERIFY_BUFFER_SIZE: usize = 64 * 1024;

#[derive(Clone, Debug, Default)]
pub struct AcquisitionCancellation {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl AcquisitionCancellation {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerifiedDownload {
    pub bytes_written: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DownloadDisposition {
    AlreadyValid,
    Downloaded,
}

#[derive(Debug)]
pub enum RuntimeDownloadError {
    InvalidSha1(AssetPathError),
    Request {
        url: reqwest::Url,
        source: reqwest::Error,
    },
    HttpStatus {
        url: reqwest::Url,
        status: reqwest::StatusCode,
    },
    CreateDirectory {
        path: PathBuf,
        source: io::Error,
    },
    CreateTemporaryFile {
        path: PathBuf,
        source: io::Error,
    },
    ReadExisting {
        path: PathBuf,
        source: io::Error,
    },
    WriteTemporaryFile {
        path: PathBuf,
        source: io::Error,
    },
    SyncTemporaryFile {
        path: PathBuf,
        source: io::Error,
    },
    ContentMismatch {
        expected_sha1: String,
        actual_sha1: String,
        expected_size: u64,
        actual_size: u64,
    },
    Persist {
        path: PathBuf,
        source: io::Error,
    },
    FilesystemTask {
        source: tokio::task::JoinError,
    },
    Cancelled,
}

impl fmt::Display for RuntimeDownloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSha1(source) => write!(formatter, "invalid expected SHA-1: {source}"),
            Self::Request { url, .. } => write!(formatter, "failed to download {url}"),
            Self::HttpStatus { url, status } => {
                write!(formatter, "download {url} returned {status}")
            }
            Self::CreateDirectory { path, source } => write!(
                formatter,
                "failed to create download directory {}: {source}",
                path.display()
            ),
            Self::CreateTemporaryFile { path, source } => write!(
                formatter,
                "failed to create temporary file near {}: {source}",
                path.display()
            ),
            Self::ReadExisting { path, source } => write!(
                formatter,
                "failed to verify existing file {}: {source}",
                path.display()
            ),
            Self::WriteTemporaryFile { path, source } => write!(
                formatter,
                "failed to write temporary download {}: {source}",
                path.display()
            ),
            Self::SyncTemporaryFile { path, source } => write!(
                formatter,
                "failed to sync temporary download {}: {source}",
                path.display()
            ),
            Self::ContentMismatch {
                expected_size,
                actual_size,
                ..
            } => write!(
                formatter,
                "downloaded content mismatch: expected {expected_size} bytes, got {actual_size}"
            ),
            Self::Persist { path, source } => write!(
                formatter,
                "failed to atomically replace {}: {source}",
                path.display()
            ),
            Self::FilesystemTask { source } => {
                write!(formatter, "download filesystem task failed: {source}")
            }
            Self::Cancelled => formatter.write_str("runtime download was cancelled"),
        }
    }
}

impl Error for RuntimeDownloadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidSha1(source) => Some(source),
            Self::Request { source, .. } => Some(source),
            Self::CreateDirectory { source, .. }
            | Self::CreateTemporaryFile { source, .. }
            | Self::ReadExisting { source, .. }
            | Self::WriteTemporaryFile { source, .. }
            | Self::SyncTemporaryFile { source, .. }
            | Self::Persist { source, .. } => Some(source),
            Self::FilesystemTask { source } => Some(source),
            Self::HttpStatus { .. } | Self::ContentMismatch { .. } | Self::Cancelled => None,
        }
    }
}

pub async fn download_verified_file(
    client: &reqwest::Client,
    url: &reqwest::Url,
    destination: &Path,
    expected_size: u64,
    expected_sha1: &str,
) -> Result<VerifiedDownload, RuntimeDownloadError> {
    let cancellation = AcquisitionCancellation::default();
    download_file_with_control(
        client,
        url,
        destination,
        expected_size,
        expected_sha1,
        &cancellation,
        &|_| {},
    )
    .await
}

pub(crate) async fn ensure_verified_file(
    client: &reqwest::Client,
    url: &reqwest::Url,
    destination: &Path,
    expected_size: u64,
    expected_sha1: &str,
    cancellation: &AcquisitionCancellation,
    on_chunk: &(dyn Fn(u64) + Send + Sync),
) -> Result<DownloadDisposition, RuntimeDownloadError> {
    let expected_sha1 = validated_sha1(expected_sha1).map_err(RuntimeDownloadError::InvalidSha1)?;
    if verify_existing_file(destination, expected_size, &expected_sha1, cancellation).await? {
        return Ok(DownloadDisposition::AlreadyValid);
    }
    download_file_with_control(
        client,
        url,
        destination,
        expected_size,
        &expected_sha1,
        cancellation,
        on_chunk,
    )
    .await?;
    Ok(DownloadDisposition::Downloaded)
}

async fn download_file_with_control(
    client: &reqwest::Client,
    url: &reqwest::Url,
    destination: &Path,
    expected_size: u64,
    expected_sha1: &str,
    cancellation: &AcquisitionCancellation,
    on_chunk: &(dyn Fn(u64) + Send + Sync),
) -> Result<VerifiedDownload, RuntimeDownloadError> {
    let expected_sha1 = validated_sha1(expected_sha1).map_err(RuntimeDownloadError::InvalidSha1)?;
    ensure_not_cancelled(cancellation)?;
    let response = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(RuntimeDownloadError::Cancelled),
        result = client.get(url.clone()).send() => result.map_err(|source| RuntimeDownloadError::Request {
            url: url.clone(),
            source,
        })?,
    };
    let status = response.status();
    if !status.is_success() {
        return Err(RuntimeDownloadError::HttpStatus {
            url: url.clone(),
            status,
        });
    }

    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    tokio::fs::create_dir_all(parent).await.map_err(|source| {
        RuntimeDownloadError::CreateDirectory {
            path: parent.to_owned(),
            source,
        }
    })?;
    let (mut temporary, temporary_path, temporary_guard) = create_temporary_file(parent).await?;
    let mut response = response;
    let mut hasher = Sha1::new();
    let mut actual_size = 0_u64;

    loop {
        let chunk = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(RuntimeDownloadError::Cancelled),
            result = response.chunk() => result.map_err(|source| RuntimeDownloadError::Request {
                url: url.clone(),
                source,
            })?,
        };
        let Some(chunk) = chunk else {
            break;
        };
        actual_size = actual_size.saturating_add(chunk.len() as u64);
        hasher.update(&chunk);
        temporary.write_all(&chunk).await.map_err(|source| {
            RuntimeDownloadError::WriteTemporaryFile {
                path: temporary_path.clone(),
                source,
            }
        })?;
        on_chunk(chunk.len() as u64);
    }

    let actual_sha1 = format!("{:x}", hasher.finalize());
    if actual_size != expected_size || actual_sha1 != expected_sha1 {
        return Err(RuntimeDownloadError::ContentMismatch {
            expected_sha1,
            actual_sha1,
            expected_size,
            actual_size,
        });
    }
    temporary
        .flush()
        .await
        .map_err(|source| RuntimeDownloadError::WriteTemporaryFile {
            path: temporary_path.clone(),
            source,
        })?;
    temporary
        .sync_all()
        .await
        .map_err(|source| RuntimeDownloadError::SyncTemporaryFile {
            path: temporary_path,
            source,
        })?;
    ensure_not_cancelled(cancellation)?;
    drop(temporary);
    persist_temporary_file(temporary_guard, destination.to_owned()).await?;
    Ok(VerifiedDownload {
        bytes_written: actual_size,
    })
}

async fn verify_existing_file(
    path: &Path,
    expected_size: u64,
    expected_sha1: &str,
    cancellation: &AcquisitionCancellation,
) -> Result<bool, RuntimeDownloadError> {
    ensure_not_cancelled(cancellation)?;
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(RuntimeDownloadError::ReadExisting {
                path: path.to_owned(),
                source,
            });
        }
    };
    if !metadata.is_file() || metadata.len() != expected_size {
        return Ok(false);
    }

    let mut file =
        tokio::fs::File::open(path)
            .await
            .map_err(|source| RuntimeDownloadError::ReadExisting {
                path: path.to_owned(),
                source,
            })?;
    let mut buffer = vec![0_u8; VERIFY_BUFFER_SIZE];
    let mut hasher = Sha1::new();
    loop {
        let count = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(RuntimeDownloadError::Cancelled),
            result = file.read(&mut buffer) => result.map_err(|source| {
                RuntimeDownloadError::ReadExisting {
                    path: path.to_owned(),
                    source,
                }
            })?,
        };
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()) == expected_sha1)
}

async fn create_temporary_file(
    parent: &Path,
) -> Result<(tokio::fs::File, PathBuf, TempPath), RuntimeDownloadError> {
    let parent = parent.to_owned();
    tokio::task::spawn_blocking(move || {
        let temporary = NamedTempFile::new_in(&parent).map_err(|source| {
            RuntimeDownloadError::CreateTemporaryFile {
                path: parent,
                source,
            }
        })?;
        let temporary_path = temporary.path().to_owned();
        let (file, guard) = temporary.into_parts();
        Ok((tokio::fs::File::from_std(file), temporary_path, guard))
    })
    .await
    .map_err(|source| RuntimeDownloadError::FilesystemTask { source })?
}

async fn persist_temporary_file(
    temporary: TempPath,
    destination: PathBuf,
) -> Result<(), RuntimeDownloadError> {
    tokio::task::spawn_blocking(move || {
        temporary
            .persist(&destination)
            .map_err(|error| RuntimeDownloadError::Persist {
                path: destination,
                source: error.error,
            })
    })
    .await
    .map_err(|source| RuntimeDownloadError::FilesystemTask { source })?
}

fn ensure_not_cancelled(
    cancellation: &AcquisitionCancellation,
) -> Result<(), RuntimeDownloadError> {
    if cancellation.is_cancelled() {
        Err(RuntimeDownloadError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use crate::http::test_support::{TestResponse, TestServer};

    use super::*;

    #[tokio::test]
    async fn downloads_verified_file_and_replaces_corrupt_target() {
        let server = TestServer::start().await.unwrap();
        let contents = b"verified client jar".to_vec();
        server.route("/client.jar", TestResponse::ok(contents.clone()));
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("client.jar");
        fs::write(&destination, vec![0_u8; contents.len()]).unwrap();

        let result = download_verified_file(
            &crate::build_http_client().unwrap(),
            &server.url("client.jar").unwrap(),
            &destination,
            contents.len() as u64,
            &sha1_hex(&contents),
        )
        .await
        .unwrap();

        assert_eq!(result.bytes_written, contents.len() as u64);
        assert_eq!(fs::read(destination).unwrap(), contents);
    }

    #[tokio::test]
    async fn hash_mismatch_preserves_old_target_and_cleans_temp() {
        assert_mismatch_preserves_target(4, &sha1_hex(b"different")).await;
    }

    #[tokio::test]
    async fn size_mismatch_preserves_old_target_and_cleans_temp() {
        assert_mismatch_preserves_target(99, &sha1_hex(b"body")).await;
    }

    #[tokio::test]
    async fn cancellation_stops_stream_and_cleans_temp() {
        let server = TestServer::start().await.unwrap();
        let contents = vec![7_u8; 256 * 1024];
        server.route(
            "/slow.jar",
            TestResponse::slow(contents.clone(), 1024, Duration::from_millis(10)),
        );
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("slow.jar");
        let cancellation = AcquisitionCancellation::default();
        let task_cancellation = cancellation.clone();
        let client = crate::build_http_client().unwrap();
        let url = server.url("slow.jar").unwrap();
        let expected_sha1 = sha1_hex(&contents);
        let task_destination = destination.clone();
        let task = tokio::spawn(async move {
            download_file_with_control(
                &client,
                &url,
                &task_destination,
                contents.len() as u64,
                &expected_sha1,
                &task_cancellation,
                &|_| {},
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;

        cancellation.cancel();
        let error = task.await.unwrap().unwrap_err();

        assert!(matches!(error, RuntimeDownloadError::Cancelled));
        assert!(!destination.exists());
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    async fn assert_mismatch_preserves_target(expected_size: u64, expected_sha1: &str) {
        let server = TestServer::start().await.unwrap();
        server.route("/file.jar", TestResponse::ok(b"body".to_vec()));
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("file.jar");
        fs::write(&destination, b"old target").unwrap();

        let error = download_verified_file(
            &crate::build_http_client().unwrap(),
            &server.url("file.jar").unwrap(),
            &destination,
            expected_size,
            expected_sha1,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            RuntimeDownloadError::ContentMismatch { .. }
        ));
        assert_eq!(fs::read(&destination).unwrap(), b"old target");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    fn sha1_hex(contents: &[u8]) -> String {
        format!("{:x}", Sha1::digest(contents))
    }
}
