use std::{
    error::Error,
    fmt,
    fs::File,
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
};

use tokio::io::AsyncReadExt;

use crate::{
    manifest::ManifestFile,
    verification::{VerificationError, canonicalize_root, resolve_manifest_path},
};

const LOCAL_CHUNK_SIZE: usize = 64 * 1024;

pub type SourceFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait FileChunkStream: Send {
    fn next_chunk(&mut self) -> SourceFuture<'_, Result<Option<Vec<u8>>, FileSourceError>>;
}

pub trait FileSource: Send + Sync {
    fn fetch<'a>(
        &'a self,
        file: &'a ManifestFile,
    ) -> SourceFuture<'a, Result<Box<dyn FileChunkStream>, FileSourceError>>;
}

#[derive(Clone, Debug)]
pub struct LocalFileSource {
    repository_root: PathBuf,
}

impl LocalFileSource {
    pub fn new(repository_root: impl Into<PathBuf>) -> Self {
        Self {
            repository_root: repository_root.into(),
        }
    }

    pub fn repository_root(&self) -> &Path {
        &self.repository_root
    }
}

impl FileSource for LocalFileSource {
    fn fetch<'a>(
        &'a self,
        file: &'a ManifestFile,
    ) -> SourceFuture<'a, Result<Box<dyn FileChunkStream>, FileSourceError>> {
        let repository_root = self.repository_root.clone();
        let manifest_path = file.path.clone();

        Box::pin(async move {
            let source_path = manifest_path.clone();
            let opened = tokio::task::spawn_blocking(move || {
                let canonical_root = canonicalize_root(&repository_root)?;
                let path = resolve_manifest_path(&canonical_root, &manifest_path)?;
                let file = File::open(&path).map_err(|source| FileSourceError::OpenFile {
                    path: path.clone(),
                    source,
                })?;
                Ok::<_, FileSourceError>((file, path))
            })
            .await
            .map_err(FileSourceError::LocalTask)??;

            Ok(Box::new(LocalFileStream {
                file: tokio::fs::File::from_std(opened.0),
                path: opened.1,
                manifest_path: source_path,
            }) as Box<dyn FileChunkStream>)
        })
    }
}

struct LocalFileStream {
    file: tokio::fs::File,
    path: PathBuf,
    manifest_path: String,
}

impl FileChunkStream for LocalFileStream {
    fn next_chunk(&mut self) -> SourceFuture<'_, Result<Option<Vec<u8>>, FileSourceError>> {
        Box::pin(async move {
            let mut chunk = vec![0_u8; LOCAL_CHUNK_SIZE];
            let read_count =
                self.file
                    .read(&mut chunk)
                    .await
                    .map_err(|source| FileSourceError::ReadFile {
                        path: self.path.clone(),
                        manifest_path: self.manifest_path.clone(),
                        source,
                    })?;
            if read_count == 0 {
                return Ok(None);
            }
            chunk.truncate(read_count);
            Ok(Some(chunk))
        })
    }
}

#[derive(Debug)]
pub enum FileSourceError {
    UnsafePath(VerificationError),
    OpenFile {
        path: PathBuf,
        source: io::Error,
    },
    ReadFile {
        path: PathBuf,
        manifest_path: String,
        source: io::Error,
    },
    LocalTask(tokio::task::JoinError),
    HttpRequest {
        url: reqwest::Url,
        source: reqwest::Error,
    },
    HttpStatus {
        url: reqwest::Url,
        status: reqwest::StatusCode,
    },
}

impl fmt::Display for FileSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafePath(error) => write!(formatter, "unsafe source path: {error}"),
            Self::OpenFile { path, .. } => {
                write!(formatter, "failed to open source file {}", path.display())
            }
            Self::ReadFile {
                path,
                manifest_path,
                ..
            } => write!(
                formatter,
                "failed to read source file {} for {manifest_path:?}",
                path.display()
            ),
            Self::LocalTask(_) => formatter.write_str("local file source task failed"),
            Self::HttpRequest { url, .. } => {
                write!(formatter, "HTTP request failed for {url}")
            }
            Self::HttpStatus { url, status } => {
                write!(formatter, "HTTP request for {url} returned {status}")
            }
        }
    }
}

impl Error for FileSourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::UnsafePath(error) => Some(error),
            Self::OpenFile { source, .. } | Self::ReadFile { source, .. } => Some(source),
            Self::LocalTask(error) => Some(error),
            Self::HttpRequest { source, .. } => Some(source),
            Self::HttpStatus { .. } => None,
        }
    }
}

impl From<VerificationError> for FileSourceError {
    fn from(error: VerificationError) -> Self {
        Self::UnsafePath(error)
    }
}
