use std::{
    collections::HashSet,
    error::Error,
    fmt, io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use sha2::{Digest, Sha256};
use tempfile::{NamedTempFile, TempPath};
use tokio::{io::AsyncWriteExt, sync::Notify};

use crate::{
    manifest::{ManifestFile, PackManifest},
    source::{FileSource, FileSourceError},
    verification::{
        VerificationError, VerificationResult, canonicalize_root, resolve_manifest_path,
    },
};

#[cfg(test)]
const COPY_BUFFER_SIZE: usize = 64 * 1024;

#[derive(Clone, Debug, Default)]
pub struct InstallCancellation {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl InstallCancellation {
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
pub struct InstallProgress {
    installed_bytes: u64,
    total_bytes: u64,
}

impl InstallProgress {
    pub const fn installed_bytes(self) -> u64 {
        self.installed_bytes
    }

    pub const fn total_bytes(self) -> u64 {
        self.total_bytes
    }

    pub fn fraction(self) -> f32 {
        if self.total_bytes == 0 {
            1.0
        } else {
            (self.installed_bytes as f64 / self.total_bytes as f64).clamp(0.0, 1.0) as f32
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstallSummary {
    installed_files: Vec<String>,
    installed_bytes: u64,
}

impl InstallSummary {
    pub fn installed_files(&self) -> &[String] {
        &self.installed_files
    }

    pub const fn installed_bytes(&self) -> u64 {
        self.installed_bytes
    }
}

#[derive(Debug)]
pub enum InstallError {
    UnsafeTarget(VerificationError),
    MissingManifestEntry {
        path: String,
    },
    TotalSizeOverflow,
    CreateParentDirectory {
        path: PathBuf,
        source: io::Error,
    },
    CreateTemporaryFile {
        directory: PathBuf,
        source: io::Error,
    },
    Source(FileSourceError),
    ReadSource {
        path: String,
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
        path: String,
        expected_sha256: String,
        actual_sha256: String,
        expected_size: u64,
        actual_size: u64,
    },
    PersistFile {
        path: PathBuf,
        source: io::Error,
    },
    FilesystemTask(tokio::task::JoinError),
    Cancelled,
}

impl fmt::Display for InstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafeTarget(error) => write!(formatter, "unsafe installation path: {error}"),
            Self::MissingManifestEntry { path } => {
                write!(
                    formatter,
                    "verification references an unknown manifest file {path:?}"
                )
            }
            Self::TotalSizeOverflow => {
                formatter.write_str("total installation size overflowed u64")
            }
            Self::CreateParentDirectory { path, .. } => write!(
                formatter,
                "failed to create installation directory {}",
                path.display()
            ),
            Self::CreateTemporaryFile { directory, .. } => write!(
                formatter,
                "failed to create a temporary file in {}",
                directory.display()
            ),
            Self::Source(error) => write!(formatter, "file source failed: {error}"),
            Self::ReadSource { path, .. } => {
                write!(formatter, "failed to read source data for {path:?}")
            }
            Self::WriteTemporaryFile { path, .. } => {
                write!(
                    formatter,
                    "failed to write temporary file {}",
                    path.display()
                )
            }
            Self::SyncTemporaryFile { path, .. } => {
                write!(
                    formatter,
                    "failed to sync temporary file {}",
                    path.display()
                )
            }
            Self::ContentMismatch {
                path,
                expected_sha256,
                actual_sha256,
                expected_size,
                actual_size,
            } => write!(
                formatter,
                "source data for {path:?} does not match manifest: expected {expected_size} bytes/{expected_sha256}, got {actual_size} bytes/{actual_sha256}"
            ),
            Self::PersistFile { path, .. } => {
                write!(formatter, "failed to atomically replace {}", path.display())
            }
            Self::FilesystemTask(_) => formatter.write_str("installation filesystem task failed"),
            Self::Cancelled => formatter.write_str("installation was cancelled"),
        }
    }
}

impl Error for InstallError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::UnsafeTarget(error) => Some(error),
            Self::CreateParentDirectory { source, .. }
            | Self::CreateTemporaryFile { source, .. }
            | Self::ReadSource { source, .. }
            | Self::WriteTemporaryFile { source, .. }
            | Self::SyncTemporaryFile { source, .. }
            | Self::PersistFile { source, .. } => Some(source),
            Self::Source(error) => Some(error),
            Self::FilesystemTask(error) => Some(error),
            Self::MissingManifestEntry { .. }
            | Self::TotalSizeOverflow
            | Self::ContentMismatch { .. }
            | Self::Cancelled => None,
        }
    }
}

pub async fn install_files(
    installation_root: impl AsRef<std::path::Path>,
    manifest: &PackManifest,
    verification: &VerificationResult,
    source: &dyn FileSource,
    cancellation: &InstallCancellation,
    mut on_progress: impl FnMut(InstallProgress),
) -> Result<InstallSummary, InstallError> {
    let installation_root = installation_root.as_ref().to_owned();
    let canonical_root = tokio::task::spawn_blocking(move || {
        canonicalize_root(&installation_root).map_err(InstallError::UnsafeTarget)
    })
    .await
    .map_err(InstallError::FilesystemTask)??;
    let files = files_to_install(manifest, verification)?;
    let total_bytes = files.iter().try_fold(0_u64, |total, file| {
        total
            .checked_add(file.size)
            .ok_or(InstallError::TotalSizeOverflow)
    })?;
    let mut installed_bytes = 0_u64;
    let mut installed_files = Vec::with_capacity(files.len());

    for manifest_file in files {
        ensure_not_cancelled(cancellation)?;
        let target_path = resolve_target_path(&canonical_root, &manifest_file.path).await?;
        let parent = target_path.parent().ok_or_else(|| {
            InstallError::UnsafeTarget(VerificationError::InvalidManifestPath {
                path: manifest_file.path.clone(),
                reason: crate::verification::InvalidManifestPathReason::Empty,
            })
        })?;
        tokio::fs::create_dir_all(parent).await.map_err(|source| {
            InstallError::CreateParentDirectory {
                path: parent.to_owned(),
                source,
            }
        })?;
        let target_path = resolve_target_path(&canonical_root, &manifest_file.path).await?;
        let parent = target_path.parent().ok_or_else(|| {
            InstallError::UnsafeTarget(VerificationError::InvalidManifestPath {
                path: manifest_file.path.clone(),
                reason: crate::verification::InvalidManifestPathReason::Empty,
            })
        })?;

        let (mut temporary, temporary_path, temporary_guard) =
            create_temporary_file(parent.to_owned()).await?;
        let mut source_stream = tokio::select! {
            () = cancellation.cancelled() => return Err(InstallError::Cancelled),
            result = source.fetch(manifest_file) => result.map_err(InstallError::Source)?,
        };
        let mut file_hasher = Sha256::new();
        let mut file_bytes = 0_u64;

        loop {
            ensure_not_cancelled(cancellation)?;
            let chunk = tokio::select! {
                () = cancellation.cancelled() => return Err(InstallError::Cancelled),
                result = source_stream.next_chunk() => result.map_err(InstallError::Source)?,
            };
            let Some(chunk) = chunk else {
                break;
            };
            ensure_not_cancelled(cancellation)?;

            temporary.write_all(&chunk).await.map_err(|source| {
                InstallError::WriteTemporaryFile {
                    path: temporary_path.clone(),
                    source,
                }
            })?;
            file_hasher.update(&chunk);
            let chunk_size =
                u64::try_from(chunk.len()).map_err(|_| InstallError::TotalSizeOverflow)?;
            file_bytes = file_bytes
                .checked_add(chunk_size)
                .ok_or(InstallError::TotalSizeOverflow)?;
            installed_bytes = installed_bytes
                .checked_add(chunk_size)
                .ok_or(InstallError::TotalSizeOverflow)?;
            on_progress(InstallProgress {
                installed_bytes,
                total_bytes,
            });
        }

        let actual_sha256 = format!("{:x}", file_hasher.finalize());
        if file_bytes != manifest_file.size
            || !actual_sha256.eq_ignore_ascii_case(&manifest_file.sha256)
        {
            return Err(InstallError::ContentMismatch {
                path: manifest_file.path.clone(),
                expected_sha256: manifest_file.sha256.clone(),
                actual_sha256,
                expected_size: manifest_file.size,
                actual_size: file_bytes,
            });
        }

        temporary
            .sync_all()
            .await
            .map_err(|source| InstallError::SyncTemporaryFile {
                path: temporary_path.clone(),
                source,
            })?;
        ensure_not_cancelled(cancellation)?;
        drop(temporary);
        persist_temporary_file(temporary_guard, target_path).await?;

        installed_files.push(manifest_file.path.clone());
        on_progress(InstallProgress {
            installed_bytes,
            total_bytes,
        });
    }

    if installed_files.is_empty() {
        on_progress(InstallProgress {
            installed_bytes: 0,
            total_bytes: 0,
        });
    }

    Ok(InstallSummary {
        installed_files,
        installed_bytes,
    })
}

async fn resolve_target_path(
    canonical_root: &std::path::Path,
    manifest_path: &str,
) -> Result<PathBuf, InstallError> {
    let canonical_root = canonical_root.to_owned();
    let manifest_path = manifest_path.to_owned();
    tokio::task::spawn_blocking(move || {
        resolve_manifest_path(&canonical_root, &manifest_path).map_err(InstallError::UnsafeTarget)
    })
    .await
    .map_err(InstallError::FilesystemTask)?
}

async fn create_temporary_file(
    parent: PathBuf,
) -> Result<(tokio::fs::File, PathBuf, TempPath), InstallError> {
    tokio::task::spawn_blocking(move || {
        let temporary =
            NamedTempFile::new_in(&parent).map_err(|source| InstallError::CreateTemporaryFile {
                directory: parent,
                source,
            })?;
        let temporary_path = temporary.path().to_owned();
        let (file, guard) = temporary.into_parts();
        Ok((tokio::fs::File::from_std(file), temporary_path, guard))
    })
    .await
    .map_err(InstallError::FilesystemTask)?
}

async fn persist_temporary_file(temporary: TempPath, target: PathBuf) -> Result<(), InstallError> {
    tokio::task::spawn_blocking(move || {
        temporary
            .persist(&target)
            .map_err(|error| InstallError::PersistFile {
                path: target,
                source: error.error,
            })
    })
    .await
    .map_err(InstallError::FilesystemTask)?
}

fn files_to_install<'a>(
    manifest: &'a PackManifest,
    verification: &VerificationResult,
) -> Result<Vec<&'a ManifestFile>, InstallError> {
    let mut required_paths: HashSet<&str> = verification
        .missing_files()
        .iter()
        .map(String::as_str)
        .chain(verification.corrupt_files().iter().map(|file| file.path()))
        .collect();
    let mut files = Vec::with_capacity(required_paths.len());

    for manifest_file in &manifest.files {
        if required_paths.remove(manifest_file.path.as_str()) {
            files.push(manifest_file);
        }
    }

    if let Some(path) = required_paths.into_iter().next() {
        return Err(InstallError::MissingManifestEntry {
            path: path.to_owned(),
        });
    }

    Ok(files)
}

fn ensure_not_cancelled(cancellation: &InstallCancellation) -> Result<(), InstallError> {
    if cancellation.is_cancelled() {
        Err(InstallError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;
    use crate::{LocalFileSource, verify_installation};

    #[test]
    fn installs_missing_file() -> Result<(), Box<dyn Error>> {
        let installation = tempfile::tempdir()?;
        let repository = tempfile::tempdir()?;
        let manifest_file = fixture_file("mods/example.jar", b"new mod");
        write_fixture(repository.path(), &manifest_file.path, b"new mod")?;
        let manifest = manifest_with(vec![manifest_file]);
        let verification = verify_installation(installation.path(), &manifest)?;

        let summary = install_local(
            installation.path(),
            repository.path(),
            &manifest,
            &verification,
            &InstallCancellation::default(),
            |_| {},
        )?;

        assert_eq!(summary.installed_files(), &["mods/example.jar"]);
        assert_eq!(
            fs::read(installation.path().join("mods/example.jar"))?,
            b"new mod"
        );
        Ok(())
    }

    #[test]
    fn replaces_corrupt_file() -> Result<(), Box<dyn Error>> {
        let installation = tempfile::tempdir()?;
        let repository = tempfile::tempdir()?;
        let manifest_file = fixture_file("client.jar", b"correct client");
        write_fixture(repository.path(), &manifest_file.path, b"correct client")?;
        write_fixture(
            installation.path(),
            &manifest_file.path,
            b"old corrupt client",
        )?;
        let manifest = manifest_with(vec![manifest_file]);
        let verification = verify_installation(installation.path(), &manifest)?;

        install_local(
            installation.path(),
            repository.path(),
            &manifest,
            &verification,
            &InstallCancellation::default(),
            |_| {},
        )?;

        assert_eq!(
            fs::read(installation.path().join("client.jar"))?,
            b"correct client"
        );
        Ok(())
    }

    #[test]
    fn verified_file_is_not_opened_or_rewritten() -> Result<(), Box<dyn Error>> {
        let installation = tempfile::tempdir()?;
        let repository = tempfile::tempdir()?;
        let manifest_file = fixture_file("client.jar", b"already correct");
        write_fixture(installation.path(), &manifest_file.path, b"already correct")?;
        let manifest = manifest_with(vec![manifest_file]);
        let verification = verify_installation(installation.path(), &manifest)?;

        let summary = install_local(
            installation.path(),
            repository.path(),
            &manifest,
            &verification,
            &InstallCancellation::default(),
            |_| {},
        )?;

        assert!(summary.installed_files().is_empty());
        assert_eq!(
            fs::read(installation.path().join("client.jar"))?,
            b"already correct"
        );
        Ok(())
    }

    #[test]
    fn creates_nested_target_directories() -> Result<(), Box<dyn Error>> {
        let installation = tempfile::tempdir()?;
        let repository = tempfile::tempdir()?;
        let manifest_file = fixture_file("versions/1.21.1/client.jar", b"nested");
        write_fixture(repository.path(), &manifest_file.path, b"nested")?;
        let manifest = manifest_with(vec![manifest_file]);
        let verification = verify_installation(installation.path(), &manifest)?;

        install_local(
            installation.path(),
            repository.path(),
            &manifest,
            &verification,
            &InstallCancellation::default(),
            |_| {},
        )?;

        assert!(
            installation
                .path()
                .join("versions/1.21.1/client.jar")
                .is_file()
        );
        Ok(())
    }

    #[test]
    fn installs_multiple_missing_and_corrupt_files() -> Result<(), Box<dyn Error>> {
        let installation = tempfile::tempdir()?;
        let repository = tempfile::tempdir()?;
        let first = fixture_file("a.bin", b"first");
        let second = fixture_file("nested/b.bin", b"second");
        let third = fixture_file("c.bin", b"third");
        for (file, contents) in [
            (&first, b"first".as_slice()),
            (&second, b"second".as_slice()),
            (&third, b"third".as_slice()),
        ] {
            write_fixture(repository.path(), &file.path, contents)?;
        }
        write_fixture(installation.path(), &third.path, b"corrupt")?;
        let manifest = manifest_with(vec![first, second, third]);
        let verification = verify_installation(installation.path(), &manifest)?;

        let summary = install_local(
            installation.path(),
            repository.path(),
            &manifest,
            &verification,
            &InstallCancellation::default(),
            |_| {},
        )?;

        assert_eq!(summary.installed_files().len(), 3);
        assert!(verify_installation(installation.path(), &manifest)?.is_valid());
        Ok(())
    }

    #[test]
    fn byte_progress_reaches_one() -> Result<(), Box<dyn Error>> {
        let installation = tempfile::tempdir()?;
        let repository = tempfile::tempdir()?;
        let manifest_file = fixture_file("progress.bin", &[7_u8; COPY_BUFFER_SIZE * 2 + 17]);
        write_fixture(
            repository.path(),
            &manifest_file.path,
            &[7_u8; COPY_BUFFER_SIZE * 2 + 17],
        )?;
        let manifest = manifest_with(vec![manifest_file]);
        let verification = verify_installation(installation.path(), &manifest)?;
        let mut progress = Vec::new();

        install_local(
            installation.path(),
            repository.path(),
            &manifest,
            &verification,
            &InstallCancellation::default(),
            |update| progress.push(update),
        )?;

        assert!(progress.len() >= 3);
        assert_eq!(progress.last().map(|update| update.fraction()), Some(1.0));
        assert!(progress.windows(2).all(|pair| {
            pair[0].installed_bytes() <= pair[1].installed_bytes()
                && pair[0].total_bytes() == pair[1].total_bytes()
        }));
        Ok(())
    }

    #[test]
    fn rejects_source_with_wrong_hash() -> Result<(), Box<dyn Error>> {
        let installation = tempfile::tempdir()?;
        let repository = tempfile::tempdir()?;
        let manifest_file = fixture_file("client.jar", b"expected");
        write_fixture(repository.path(), &manifest_file.path, b"different")?;
        let manifest = manifest_with(vec![manifest_file]);
        let verification = verify_installation(installation.path(), &manifest)?;

        let error = install_local(
            installation.path(),
            repository.path(),
            &manifest,
            &verification,
            &InstallCancellation::default(),
            |_| {},
        )
        .unwrap_err();

        assert!(matches!(error, InstallError::ContentMismatch { .. }));
        Ok(())
    }

    #[test]
    fn rejects_source_with_wrong_size() -> Result<(), Box<dyn Error>> {
        let installation = tempfile::tempdir()?;
        let repository = tempfile::tempdir()?;
        let mut manifest_file = fixture_file("client.jar", b"source bytes");
        manifest_file.size += 1;
        write_fixture(repository.path(), &manifest_file.path, b"source bytes")?;
        let manifest = manifest_with(vec![manifest_file]);
        let verification = verify_installation(installation.path(), &manifest)?;

        let error = install_local(
            installation.path(),
            repository.path(),
            &manifest,
            &verification,
            &InstallCancellation::default(),
            |_| {},
        )
        .unwrap_err();

        assert!(matches!(
            error,
            InstallError::ContentMismatch {
                expected_size,
                actual_size,
                ..
            } if expected_size != actual_size
        ));
        Ok(())
    }

    #[test]
    fn source_error_does_not_damage_existing_target() -> Result<(), Box<dyn Error>> {
        let installation = tempfile::tempdir()?;
        let repository = tempfile::tempdir()?;
        let manifest_file = fixture_file("client.jar", b"expected replacement");
        write_fixture(repository.path(), &manifest_file.path, b"bad source")?;
        write_fixture(installation.path(), &manifest_file.path, b"old target")?;
        let manifest = manifest_with(vec![manifest_file]);
        let verification = verify_installation(installation.path(), &manifest)?;

        let result = install_local(
            installation.path(),
            repository.path(),
            &manifest,
            &verification,
            &InstallCancellation::default(),
            |_| {},
        );

        assert!(matches!(result, Err(InstallError::ContentMismatch { .. })));
        assert_eq!(
            fs::read(installation.path().join("client.jar"))?,
            b"old target"
        );
        Ok(())
    }

    #[test]
    fn temporary_file_is_removed_after_error() -> Result<(), Box<dyn Error>> {
        let installation = tempfile::tempdir()?;
        let repository = tempfile::tempdir()?;
        let manifest_file = fixture_file("nested/client.jar", b"expected");
        write_fixture(repository.path(), &manifest_file.path, b"bad")?;
        let manifest = manifest_with(vec![manifest_file]);
        let verification = verify_installation(installation.path(), &manifest)?;

        let result = install_local(
            installation.path(),
            repository.path(),
            &manifest,
            &verification,
            &InstallCancellation::default(),
            |_| {},
        );

        assert!(result.is_err());
        assert_eq!(fs::read_dir(installation.path().join("nested"))?.count(), 0);
        Ok(())
    }

    #[test]
    fn cancellation_stops_installation_between_chunks() -> Result<(), Box<dyn Error>> {
        let installation = tempfile::tempdir()?;
        let manifest_file = repeated_file("large.bin", 9, COPY_BUFFER_SIZE * 3);
        let manifest = manifest_with(vec![manifest_file.clone()]);
        let verification = verify_installation(installation.path(), &manifest)?;
        let cancellation = InstallCancellation::default();
        let cancellation_from_progress = cancellation.clone();
        let source = GeneratedSource {
            byte: 9,
            size: manifest_file.size,
        };

        let error = test_runtime()
            .block_on(install_files(
                installation.path(),
                &manifest,
                &verification,
                &source,
                &cancellation,
                move |progress| {
                    if progress.installed_bytes() > 0 {
                        cancellation_from_progress.cancel();
                    }
                },
            ))
            .unwrap_err();

        assert!(matches!(error, InstallError::Cancelled));
        assert!(!installation.path().join("large.bin").exists());
        Ok(())
    }

    #[test]
    fn cancellation_preserves_old_target_and_removes_partial_file() -> Result<(), Box<dyn Error>> {
        let installation = tempfile::tempdir()?;
        let manifest_file = repeated_file("large.bin", 4, COPY_BUFFER_SIZE * 3);
        write_fixture(installation.path(), &manifest_file.path, b"old target")?;
        let manifest = manifest_with(vec![manifest_file.clone()]);
        let verification = verify_installation(installation.path(), &manifest)?;
        let cancellation = InstallCancellation::default();
        let cancellation_from_progress = cancellation.clone();
        let source = GeneratedSource {
            byte: 4,
            size: manifest_file.size,
        };

        let result = test_runtime().block_on(install_files(
            installation.path(),
            &manifest,
            &verification,
            &source,
            &cancellation,
            move |progress| {
                if progress.installed_bytes() > 0 {
                    cancellation_from_progress.cancel();
                }
            },
        ));

        assert!(matches!(result, Err(InstallError::Cancelled)));
        assert_eq!(
            fs::read(installation.path().join("large.bin"))?,
            b"old target"
        );
        assert_eq!(fs::read_dir(installation.path())?.count(), 1);
        Ok(())
    }

    #[test]
    fn local_source_rejects_path_traversal() -> Result<(), Box<dyn Error>> {
        let repository = tempfile::tempdir()?;
        let source = LocalFileSource::new(repository.path());
        let file = ManifestFile {
            path: "../outside.bin".to_owned(),
            sha256: "0".repeat(64),
            size: 0,
        };

        let result = test_runtime().block_on(source.fetch(&file));

        assert!(matches!(result, Err(FileSourceError::UnsafePath(_))));
        Ok(())
    }

    #[test]
    fn verification_passes_after_installation() -> Result<(), Box<dyn Error>> {
        let installation = tempfile::tempdir()?;
        let repository = tempfile::tempdir()?;
        let manifest_file = fixture_file("verified-after.bin", b"installed bytes");
        write_fixture(repository.path(), &manifest_file.path, b"installed bytes")?;
        let manifest = manifest_with(vec![manifest_file]);
        let before = verify_installation(installation.path(), &manifest)?;

        install_local(
            installation.path(),
            repository.path(),
            &manifest,
            &before,
            &InstallCancellation::default(),
            |_| {},
        )?;

        let after = verify_installation(installation.path(), &manifest)?;
        assert!(after.is_valid());
        assert_eq!(after.verified_files(), &["verified-after.bin"]);
        Ok(())
    }

    fn install_local(
        installation_root: &Path,
        repository_root: &Path,
        manifest: &PackManifest,
        verification: &VerificationResult,
        cancellation: &InstallCancellation,
        on_progress: impl FnMut(InstallProgress),
    ) -> Result<InstallSummary, InstallError> {
        test_runtime().block_on(install_files(
            installation_root,
            manifest,
            verification,
            &LocalFileSource::new(repository_root),
            cancellation,
            on_progress,
        ))
    }

    fn manifest_with(files: Vec<ManifestFile>) -> PackManifest {
        PackManifest {
            version: "test-pack".to_owned(),
            minecraft_version: "1.21.1".to_owned(),
            files,
        }
    }

    fn fixture_file(path: &str, contents: &[u8]) -> ManifestFile {
        let mut hasher = Sha256::new();
        hasher.update(contents);
        ManifestFile {
            path: path.to_owned(),
            sha256: format!("{:x}", hasher.finalize()),
            size: contents.len() as u64,
        }
    }

    fn repeated_file(path: &str, byte: u8, size: usize) -> ManifestFile {
        let mut hasher = Sha256::new();
        let chunk = [byte; COPY_BUFFER_SIZE];
        let mut remaining = size;
        while remaining > 0 {
            let chunk_size = remaining.min(chunk.len());
            hasher.update(&chunk[..chunk_size]);
            remaining -= chunk_size;
        }
        ManifestFile {
            path: path.to_owned(),
            sha256: format!("{:x}", hasher.finalize()),
            size: size as u64,
        }
    }

    fn write_fixture(root: &Path, path: &str, contents: &[u8]) -> Result<(), io::Error> {
        let file_path = root.join(path);
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(file_path, contents)
    }

    struct GeneratedSource {
        byte: u8,
        size: u64,
    }

    impl FileSource for GeneratedSource {
        fn fetch<'a>(
            &'a self,
            _file: &'a ManifestFile,
        ) -> crate::SourceFuture<'a, Result<Box<dyn crate::FileChunkStream>, FileSourceError>>
        {
            Box::pin(async move {
                Ok(Box::new(GeneratedReader {
                    byte: self.byte,
                    remaining: self.size,
                }) as Box<dyn crate::FileChunkStream>)
            })
        }
    }

    struct GeneratedReader {
        byte: u8,
        remaining: u64,
    }

    impl crate::FileChunkStream for GeneratedReader {
        fn next_chunk(
            &mut self,
        ) -> crate::SourceFuture<'_, Result<Option<Vec<u8>>, FileSourceError>> {
            Box::pin(async move {
                if self.remaining == 0 {
                    return Ok(None);
                }
                let chunk_size = usize::try_from(self.remaining.min(COPY_BUFFER_SIZE as u64))
                    .map_err(|error| FileSourceError::ReadFile {
                        path: PathBuf::from("generated"),
                        manifest_path: "generated".to_owned(),
                        source: io::Error::other(error),
                    })?;
                self.remaining -= chunk_size as u64;
                Ok(Some(vec![self.byte; chunk_size]))
            })
        }
    }

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }
}
