use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tempfile::{NamedTempFile, TempPath};
use tokio::io::AsyncWriteExt;
use zip::ZipArchive;

use crate::ClientConfig;

pub const MAX_MODS_ZIP_SIZE: u64 = 1024 * 1024 * 1024;
pub const MAX_EXTRACTED_MODS_SIZE: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_MODS_ZIP_ENTRIES: usize = 10_000;
const STATE_FILE_NAME: &str = "state.json";
const MODS_DIRECTORY_NAME: &str = "mods";
static BACKUP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Default)]
pub struct ModsSyncCancellation(crate::AcquisitionCancellation);

impl ModsSyncCancellation {
    pub fn cancel(&self) {
        self.0.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    pub async fn cancelled(&self) {
        self.0.cancelled().await;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModsSyncStage {
    #[default]
    Checking,
    Requesting,
    Downloading,
    Inspecting,
    Extracting,
    Committing,
    Complete,
    Unchanged,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModsSyncProgress {
    pub stage: ModsSyncStage,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub elapsed: std::time::Duration,
    pub idle_for: std::time::Duration,
    pub bytes_per_second: u64,
    pub current_file: Option<String>,
    pub files_completed: usize,
    pub files_total: Option<usize>,
    pub file_bytes: u64,
    pub file_total: Option<u64>,
}

impl ModsSyncProgress {
    pub fn fraction(&self) -> Option<f32> {
        match self.stage {
            ModsSyncStage::Downloading => self
                .total
                .filter(|total| *total > 0)
                .map(|total| (self.downloaded as f64 / total as f64).clamp(0.0, 1.0) as f32),
            ModsSyncStage::Inspecting | ModsSyncStage::Extracting => {
                self.files_total.filter(|total| *total > 0).map(|total| {
                    let current = self
                        .file_total
                        .filter(|size| *size > 0)
                        .map_or(0.0, |size| self.file_bytes as f64 / size as f64);
                    ((self.files_completed as f64 + current) / total as f64).clamp(0.0, 1.0) as f32
                })
            }
            ModsSyncStage::Complete | ModsSyncStage::Unchanged => Some(1.0),
            _ => None,
        }
    }
}

struct ModsProgressReporter {
    callback: Arc<dyn Fn(ModsSyncProgress) + Send + Sync>,
    snapshot: ModsSyncProgress,
    started: Instant,
    last_data: Instant,
    last_emit: Instant,
    speed_sample: Instant,
    speed_bytes: u64,
    last_log: Instant,
}

impl ModsProgressReporter {
    fn new(callback: impl Fn(ModsSyncProgress) + Send + Sync + 'static) -> Self {
        let now = Instant::now();
        Self {
            callback: Arc::new(callback),
            snapshot: ModsSyncProgress::default(),
            started: now,
            last_data: now,
            last_emit: now,
            speed_sample: now,
            speed_bytes: 0,
            last_log: now,
        }
    }

    fn phase(&mut self, stage: ModsSyncStage) {
        self.snapshot.stage = stage;
        self.snapshot.current_file = match stage {
            ModsSyncStage::Requesting | ModsSyncStage::Downloading => Some("mods.zip".to_owned()),
            _ => None,
        };
        self.snapshot.file_bytes = 0;
        self.snapshot.file_total = None;
        self.last_data = Instant::now();
        if stage == ModsSyncStage::Downloading {
            self.speed_sample = self.last_data;
        }
        tracing::info!(?stage, "mods synchronization stage");
        self.emit(true);
    }

    fn received(&mut self, bytes: u64) {
        self.snapshot.downloaded = bytes;
        self.last_data = Instant::now();
        self.emit(false);
    }

    fn emit(&mut self, force: bool) {
        let now = Instant::now();
        self.snapshot.elapsed = now.duration_since(self.started);
        self.snapshot.idle_for = if matches!(
            self.snapshot.stage,
            ModsSyncStage::Requesting | ModsSyncStage::Downloading
        ) {
            now.duration_since(self.last_data)
        } else {
            Duration::ZERO
        };
        if self.snapshot.stage == ModsSyncStage::Downloading {
            let sample_time = now.duration_since(self.speed_sample);
            if sample_time >= Duration::from_millis(250) {
                self.snapshot.bytes_per_second =
                    (self.snapshot.downloaded.saturating_sub(self.speed_bytes) as f64
                        / sample_time.as_secs_f64()) as u64;
                self.speed_sample = now;
                self.speed_bytes = self.snapshot.downloaded;
            }
        }
        if force || now.duration_since(self.last_emit) >= Duration::from_millis(250) {
            (self.callback)(self.snapshot.clone());
            self.last_emit = now;
        }
        if matches!(
            self.snapshot.stage,
            ModsSyncStage::Requesting | ModsSyncStage::Downloading
        ) && now.duration_since(self.last_log) >= Duration::from_secs(5)
        {
            tracing::info!(stage = ?self.snapshot.stage, downloaded = self.snapshot.downloaded, total = self.snapshot.total,
                bytes_per_second = self.snapshot.bytes_per_second, elapsed_secs = self.snapshot.elapsed.as_secs(), idle_secs = self.snapshot.idle_for.as_secs(), "mods transfer progress");
            self.last_log = now;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModsSyncOutcome {
    Unchanged,
    Updated { files_installed: usize },
}

#[derive(Debug, thiserror::Error)]
pub enum ModsSyncError {
    #[error("mods synchronization was cancelled")]
    Cancelled,
    #[error("failed to prepare launcher data directory {path}")]
    CreateDataDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to read launcher state {path}")]
    ReadState {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to request mods archive from {url}")]
    Request {
        url: reqwest::Url,
        #[source]
        source: reqwest::Error,
    },
    #[error("mods archive request for {url} returned {status}")]
    HttpStatus {
        url: reqwest::Url,
        status: reqwest::StatusCode,
    },
    #[error("mods archive exceeds the {limit} byte size limit")]
    ArchiveTooLarge { limit: u64 },
    #[error("failed to create a temporary file in {path}")]
    CreateTemporaryFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to write temporary mods archive {path}")]
    WriteArchive {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("mods extraction task failed")]
    ExtractionTask(#[source] tokio::task::JoinError),
    #[error("mods archive is invalid")]
    InvalidArchive(#[source] zip::result::ZipError),
    #[error("mods archive contains {count} entries; maximum is {limit}")]
    TooManyEntries { count: usize, limit: usize },
    #[error("mods archive entry is unsafe: {entry}")]
    UnsafeEntry { entry: String },
    #[error("mods archive contains a symlink-like entry: {entry}")]
    SymlinkEntry { entry: String },
    #[error("mods archive contains an unsupported special entry: {entry}")]
    SpecialEntry { entry: String },
    #[error("mods archive mixes root files with a mods/ directory")]
    MixedLayout,
    #[error("extracted mods exceed the {limit} byte size limit")]
    ExtractedTooLarge { limit: u64 },
    #[error("failed to create mods staging directory in {path}")]
    CreateStaging {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to write extracted entry {path}")]
    WriteEntry {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to serialize launcher state")]
    SerializeState(#[source] serde_json::Error),
    #[error("failed to commit {operation} at {path}")]
    Commit {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to roll back {operation}; recovery backup remains at {backup}")]
    Rollback {
        operation: &'static str,
        backup: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct LauncherLocalState {
    #[serde(default)]
    mods_hash: Option<String>,
}

pub async fn sync_mods<F>(
    client: &reqwest::Client,
    config: &ClientConfig,
    data_directory: &Path,
    cancellation: &ModsSyncCancellation,
    on_progress: F,
) -> Result<ModsSyncOutcome, ModsSyncError>
where
    F: Fn(ModsSyncProgress) + Send + Sync + 'static,
{
    sync_mods_impl(
        client,
        config,
        data_directory,
        cancellation,
        false,
        on_progress,
    )
    .await
}

/// Reinstalls the managed mods directory even when the saved pack hash matches.
/// Other game files are preserved; replacement uses the normal staging/rollback path.
pub async fn reinstall_mods<F>(
    client: &reqwest::Client,
    config: &ClientConfig,
    data_directory: &Path,
    cancellation: &ModsSyncCancellation,
    on_progress: F,
) -> Result<ModsSyncOutcome, ModsSyncError>
where
    F: Fn(ModsSyncProgress) + Send + Sync + 'static,
{
    sync_mods_impl(
        client,
        config,
        data_directory,
        cancellation,
        true,
        on_progress,
    )
    .await
}

pub(crate) async fn sync_mods_impl<F>(
    client: &reqwest::Client,
    config: &ClientConfig,
    data_directory: &Path,
    cancellation: &ModsSyncCancellation,
    force: bool,
    on_progress: F,
) -> Result<ModsSyncOutcome, ModsSyncError>
where
    F: Fn(ModsSyncProgress) + Send + Sync + 'static,
{
    let mut reporter = ModsProgressReporter::new(on_progress);
    reporter.phase(ModsSyncStage::Checking);
    ensure_not_cancelled(cancellation)?;
    tokio::fs::create_dir_all(data_directory)
        .await
        .map_err(|source| ModsSyncError::CreateDataDirectory {
            path: data_directory.to_owned(),
            source,
        })?;

    let state_path = data_directory.join(STATE_FILE_NAME);
    let local_state = read_local_state(&state_path).await?;
    if !force && local_state.mods_hash.as_deref() == Some(config.mods_hash.as_str()) {
        reporter.phase(ModsSyncStage::Unchanged);
        return Ok(ModsSyncOutcome::Unchanged);
    }

    ensure_not_cancelled(cancellation)?;
    reporter.phase(ModsSyncStage::Requesting);
    let request = client.get(config.mods_url.clone()).send();
    tokio::pin!(request);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat.tick().await;
    let mut response = loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ModsSyncError::Cancelled),
            result = &mut request => break result.map_err(|source| ModsSyncError::Request { url: config.mods_url.clone(), source })?,
            _ = heartbeat.tick() => reporter.emit(true),
        }
    };
    let status = response.status();
    if !status.is_success() {
        return Err(ModsSyncError::HttpStatus {
            url: config.mods_url.clone(),
            status,
        });
    }
    let total = response.content_length();
    tracing::info!(%status, total, "mods archive response received");
    if total.is_some_and(|length| length > MAX_MODS_ZIP_SIZE) {
        return Err(ModsSyncError::ArchiveTooLarge {
            limit: MAX_MODS_ZIP_SIZE,
        });
    }

    let temporary = NamedTempFile::new_in(data_directory).map_err(|source| {
        ModsSyncError::CreateTemporaryFile {
            path: data_directory.to_owned(),
            source,
        }
    })?;
    let archive_path = temporary.path().to_owned();
    let (file, archive_guard) = temporary.into_parts();
    let mut output = tokio::fs::File::from_std(file);
    let mut downloaded = 0_u64;
    reporter.snapshot.total = total;
    reporter.phase(ModsSyncStage::Downloading);

    loop {
        let chunk = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ModsSyncError::Cancelled),
            result = response.chunk() => result.map_err(|source| ModsSyncError::Request { url: config.mods_url.clone(), source })?,
            _ = heartbeat.tick() => { reporter.emit(true); continue; }
        };
        let Some(chunk) = chunk else {
            break;
        };
        ensure_not_cancelled(cancellation)?;
        downloaded =
            downloaded
                .checked_add(chunk.len() as u64)
                .ok_or(ModsSyncError::ArchiveTooLarge {
                    limit: MAX_MODS_ZIP_SIZE,
                })?;
        if downloaded > MAX_MODS_ZIP_SIZE {
            return Err(ModsSyncError::ArchiveTooLarge {
                limit: MAX_MODS_ZIP_SIZE,
            });
        }
        output
            .write_all(&chunk)
            .await
            .map_err(|source| ModsSyncError::WriteArchive {
                path: archive_path.clone(),
                source,
            })?;
        reporter.received(downloaded);
    }
    output
        .flush()
        .await
        .map_err(|source| ModsSyncError::WriteArchive {
            path: archive_path.clone(),
            source,
        })?;
    drop(output);
    reporter.emit(true);
    tracing::info!(
        downloaded,
        elapsed_secs = reporter.snapshot.elapsed.as_secs(),
        "mods archive download completed"
    );

    let data_directory = data_directory.to_owned();
    let remote_hash = config.mods_hash.clone();
    let task_cancellation = cancellation.clone();
    let result = tokio::task::spawn_blocking(move || {
        extract_and_commit(
            &archive_path,
            &data_directory,
            remote_hash,
            &task_cancellation,
            &mut reporter,
        )
    })
    .await
    .map_err(ModsSyncError::ExtractionTask)?;
    drop(archive_guard);
    result
}

async fn read_local_state(path: &Path) -> Result<LauncherLocalState, ModsSyncError> {
    match tokio::fs::read(path).await {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(state) => Ok(state),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "ignoring invalid launcher state");
                Ok(LauncherLocalState::default())
            }
        },
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            Ok(LauncherLocalState::default())
        }
        Err(source) => Err(ModsSyncError::ReadState {
            path: path.to_owned(),
            source,
        }),
    }
}

fn extract_and_commit(
    archive_path: &Path,
    data_directory: &Path,
    remote_hash: String,
    cancellation: &ModsSyncCancellation,
    reporter: &mut ModsProgressReporter,
) -> Result<ModsSyncOutcome, ModsSyncError> {
    ensure_not_cancelled(cancellation)?;
    let staging = tempfile::Builder::new()
        .prefix(".mods-staging-")
        .tempdir_in(data_directory)
        .map_err(|source| ModsSyncError::CreateStaging {
            path: data_directory.to_owned(),
            source,
        })?;
    let file = File::open(archive_path).map_err(|source| ModsSyncError::WriteArchive {
        path: archive_path.to_owned(),
        source,
    })?;
    reporter.phase(ModsSyncStage::Inspecting);
    let mut archive = ZipArchive::new(file).map_err(ModsSyncError::InvalidArchive)?;
    reporter.snapshot.files_total = Some(archive.len());
    let contents = inspect_layout(&mut archive, cancellation, reporter)?;
    reporter.snapshot.files_completed = 0;
    reporter.snapshot.files_total = Some(contents.file_count);
    reporter.phase(ModsSyncStage::Extracting);
    let files_installed = extract_archive(
        &mut archive,
        staging.path(),
        contents.layout,
        cancellation,
        reporter,
    )?;
    ensure_not_cancelled(cancellation)?;
    reporter.phase(ModsSyncStage::Committing);

    let mut state_file = NamedTempFile::new_in(data_directory).map_err(|source| {
        ModsSyncError::CreateTemporaryFile {
            path: data_directory.to_owned(),
            source,
        }
    })?;
    serde_json::to_writer_pretty(
        state_file.as_file_mut(),
        &LauncherLocalState {
            mods_hash: Some(remote_hash),
        },
    )
    .map_err(ModsSyncError::SerializeState)?;
    state_file
        .as_file_mut()
        .write_all(b"\n")
        .map_err(|source| ModsSyncError::WriteEntry {
            path: state_file.path().to_owned(),
            source,
        })?;
    state_file
        .as_file_mut()
        .sync_all()
        .map_err(|source| ModsSyncError::WriteEntry {
            path: state_file.path().to_owned(),
            source,
        })?;

    commit_installation(staging, state_file.into_temp_path(), data_directory)?;
    reporter.phase(ModsSyncStage::Complete);
    Ok(ModsSyncOutcome::Updated { files_installed })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArchiveLayout {
    Root,
    ModsDirectory,
}

struct ArchiveContents {
    layout: ArchiveLayout,
    file_count: usize,
}

fn inspect_layout(
    archive: &mut ZipArchive<File>,
    cancellation: &ModsSyncCancellation,
    reporter: &mut ModsProgressReporter,
) -> Result<ArchiveContents, ModsSyncError> {
    if archive.len() > MAX_MODS_ZIP_ENTRIES {
        return Err(ModsSyncError::TooManyEntries {
            count: archive.len(),
            limit: MAX_MODS_ZIP_ENTRIES,
        });
    }

    let mut wrapped = None;
    let mut extracted_size = 0_u64;
    let mut destinations = HashSet::new();
    let mut file_count = 0;
    for index in 0..archive.len() {
        ensure_not_cancelled(cancellation)?;
        let entry = archive
            .by_index(index)
            .map_err(ModsSyncError::InvalidArchive)?;
        validate_entry_type(&entry)?;
        reporter.snapshot.current_file = Some(entry.name().to_owned());
        let path = safe_relative_path(entry.name()).ok_or_else(|| ModsSyncError::UnsafeEntry {
            entry: entry.name().to_owned(),
        })?;
        if entry.is_dir() {
            reporter.snapshot.files_completed = index + 1;
            reporter.emit(false);
            continue;
        }

        let is_wrapped = first_component_is_mods(&path);
        if wrapped.is_some_and(|current| current != is_wrapped) {
            return Err(ModsSyncError::MixedLayout);
        }
        wrapped = Some(is_wrapped);
        let destination =
            destination_path(&path, is_wrapped).ok_or_else(|| ModsSyncError::UnsafeEntry {
                entry: entry.name().to_owned(),
            })?;
        let collision_key = destination
            .to_string_lossy()
            .replace('\\', "/")
            .to_lowercase();
        if !destinations.insert(collision_key) {
            return Err(ModsSyncError::UnsafeEntry {
                entry: entry.name().to_owned(),
            });
        }

        extracted_size =
            extracted_size
                .checked_add(entry.size())
                .ok_or(ModsSyncError::ExtractedTooLarge {
                    limit: MAX_EXTRACTED_MODS_SIZE,
                })?;
        if extracted_size > MAX_EXTRACTED_MODS_SIZE {
            return Err(ModsSyncError::ExtractedTooLarge {
                limit: MAX_EXTRACTED_MODS_SIZE,
            });
        }
        file_count += 1;
        reporter.snapshot.files_completed = index + 1;
        reporter.emit(false);
    }
    reporter.emit(true);
    let layout = if wrapped.unwrap_or(false) {
        ArchiveLayout::ModsDirectory
    } else {
        ArchiveLayout::Root
    };
    Ok(ArchiveContents { layout, file_count })
}

fn extract_archive(
    archive: &mut ZipArchive<File>,
    staging_directory: &Path,
    layout: ArchiveLayout,
    cancellation: &ModsSyncCancellation,
    reporter: &mut ModsProgressReporter,
) -> Result<usize, ModsSyncError> {
    let mut files_installed = 0;
    let mut extracted_size = 0_u64;
    let wrapped = layout == ArchiveLayout::ModsDirectory;
    let mut buffer = [0_u8; 64 * 1024];

    for index in 0..archive.len() {
        ensure_not_cancelled(cancellation)?;
        let mut entry = archive
            .by_index(index)
            .map_err(ModsSyncError::InvalidArchive)?;
        validate_entry_type(&entry)?;
        let relative =
            safe_relative_path(entry.name()).ok_or_else(|| ModsSyncError::UnsafeEntry {
                entry: entry.name().to_owned(),
            })?;
        let Some(relative) = destination_path(&relative, wrapped) else {
            continue;
        };
        let destination = staging_directory.join(&relative);
        if entry.is_dir() {
            fs::create_dir_all(&destination).map_err(|source| ModsSyncError::WriteEntry {
                path: destination,
                source,
            })?;
            continue;
        }

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|source| ModsSyncError::WriteEntry {
                path: parent.to_owned(),
                source,
            })?;
        }
        reporter.snapshot.current_file = Some(relative.to_string_lossy().replace('\\', "/"));
        reporter.snapshot.file_total = Some(entry.size());
        reporter.snapshot.file_bytes = 0;
        tracing::info!(file = ?relative, completed = files_installed, total = reporter.snapshot.files_total, "extracting mod file");
        reporter.emit(true);
        let mut output =
            File::create(&destination).map_err(|source| ModsSyncError::WriteEntry {
                path: destination.clone(),
                source,
            })?;
        loop {
            ensure_not_cancelled(cancellation)?;
            let read = entry
                .read(&mut buffer)
                .map_err(|source| ModsSyncError::WriteEntry {
                    path: destination.clone(),
                    source,
                })?;
            if read == 0 {
                break;
            }
            extracted_size = extracted_size.checked_add(read as u64).ok_or(
                ModsSyncError::ExtractedTooLarge {
                    limit: MAX_EXTRACTED_MODS_SIZE,
                },
            )?;
            if extracted_size > MAX_EXTRACTED_MODS_SIZE {
                return Err(ModsSyncError::ExtractedTooLarge {
                    limit: MAX_EXTRACTED_MODS_SIZE,
                });
            }
            output
                .write_all(&buffer[..read])
                .map_err(|source| ModsSyncError::WriteEntry {
                    path: destination.clone(),
                    source,
                })?;
            reporter.snapshot.file_bytes += read as u64;
            reporter.emit(false);
        }
        output
            .sync_all()
            .map_err(|source| ModsSyncError::WriteEntry {
                path: destination,
                source,
            })?;
        files_installed += 1;
        reporter.snapshot.files_completed = files_installed;
        reporter.snapshot.file_bytes = 0;
        reporter.snapshot.file_total = None;
        reporter.emit(true);
    }
    Ok(files_installed)
}

fn validate_entry_type(entry: &zip::read::ZipFile<'_>) -> Result<(), ModsSyncError> {
    let Some(mode) = entry.unix_mode() else {
        return Ok(());
    };
    let file_type = mode & 0o170000;
    if file_type == 0o120000 {
        return Err(ModsSyncError::SymlinkEntry {
            entry: entry.name().to_owned(),
        });
    }
    if !matches!(file_type, 0 | 0o040000 | 0o100000) {
        return Err(ModsSyncError::SpecialEntry {
            entry: entry.name().to_owned(),
        });
    }
    Ok(())
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

fn first_component_is_mods(path: &Path) -> bool {
    path.components()
        .next()
        .is_some_and(|component| component.as_os_str() == MODS_DIRECTORY_NAME)
}

fn destination_path(path: &Path, wrapped: bool) -> Option<PathBuf> {
    if !wrapped {
        return Some(path.to_owned());
    }

    let mut components = path.components();
    if components.next()?.as_os_str() != MODS_DIRECTORY_NAME {
        return None;
    }
    let destination: PathBuf = components.collect();
    (!destination.as_os_str().is_empty()).then_some(destination)
}

fn commit_installation(
    staging: tempfile::TempDir,
    state_temporary: TempPath,
    data_directory: &Path,
) -> Result<(), ModsSyncError> {
    let mods_path = data_directory.join(MODS_DIRECTORY_NAME);
    let state_path = data_directory.join(STATE_FILE_NAME);
    let mods_backup = unique_backup_path(&mods_path);
    let state_backup = unique_backup_path(&state_path);
    let had_mods = fs::symlink_metadata(&mods_path).is_ok();
    let had_state = fs::symlink_metadata(&state_path).is_ok();

    if had_mods {
        fs::rename(&mods_path, &mods_backup).map_err(|source| ModsSyncError::Commit {
            operation: "mods backup",
            path: mods_path.clone(),
            source,
        })?;
    }

    if let Err(source) = fs::rename(staging.path(), &mods_path) {
        restore_backup(&mods_path, &mods_backup, had_mods, "mods", false)?;
        return Err(ModsSyncError::Commit {
            operation: "mods installation",
            path: mods_path,
            source,
        });
    }

    if had_state && let Err(source) = fs::rename(&state_path, &state_backup) {
        restore_backup(&mods_path, &mods_backup, had_mods, "mods", true)?;
        return Err(ModsSyncError::Commit {
            operation: "state backup",
            path: state_path,
            source,
        });
    }

    if let Err(error) = state_temporary.persist(&state_path) {
        if had_state {
            fs::rename(&state_backup, &state_path).map_err(|source| ModsSyncError::Rollback {
                operation: "state",
                backup: state_backup.clone(),
                source,
            })?;
        }
        restore_backup(&mods_path, &mods_backup, had_mods, "mods", true)?;
        return Err(ModsSyncError::Commit {
            operation: "state installation",
            path: state_path,
            source: error.error,
        });
    }

    cleanup_backup(&mods_backup);
    cleanup_backup(&state_backup);
    Ok(())
}

fn restore_backup(
    destination: &Path,
    backup: &Path,
    had_previous: bool,
    operation: &'static str,
    remove_destination: bool,
) -> Result<(), ModsSyncError> {
    if remove_destination && fs::symlink_metadata(destination).is_ok() {
        remove_path(destination).map_err(|source| ModsSyncError::Rollback {
            operation,
            backup: backup.to_owned(),
            source,
        })?;
    }
    if had_previous {
        fs::rename(backup, destination).map_err(|source| ModsSyncError::Rollback {
            operation,
            backup: backup.to_owned(),
            source,
        })?;
    }
    Ok(())
}

fn cleanup_backup(path: &Path) {
    if fs::symlink_metadata(path).is_ok()
        && let Err(error) = remove_path(path)
    {
        tracing::warn!(%error, path = %path.display(), "failed to clean mods backup");
    }
}

fn unique_backup_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("launcher"))
        .to_string_lossy();
    loop {
        let sequence = BACKUP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{name}-backup-{}-{sequence}", std::process::id()));
        if fs::symlink_metadata(&candidate).is_err() {
            return candidate;
        }
    }
}

fn remove_path(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        fs::remove_file(path)
    } else {
        fs::remove_dir_all(path)
    }
}

fn ensure_not_cancelled(cancellation: &ModsSyncCancellation) -> Result<(), ModsSyncError> {
    if cancellation.is_cancelled() {
        Err(ModsSyncError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io::Write};

    use tempfile::TempDir;
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use crate::{
        build_http_client,
        http::test_support::{TestResponse, TestServer},
    };

    use super::*;

    #[tokio::test]
    async fn downloads_once_and_skips_matching_hash() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route(
            "/mods.zip",
            TestResponse::ok(zip_bytes(&[("mods/a.jar", b"a")])?),
        );
        let root = TempDir::new()?;
        let config = client_config(server.url("mods.zip")?, "hash-1");

        let first = sync(&server, &config, &root).await?;
        let second = sync(&server, &config, &root).await?;

        assert_eq!(first, ModsSyncOutcome::Updated { files_installed: 1 });
        assert_eq!(second, ModsSyncOutcome::Unchanged);
        assert_eq!(server.request_count("/mods.zip"), 1);
        assert_eq!(fs::read(root.path().join("mods/a.jar"))?, b"a");
        assert_eq!(read_hash(root.path())?, Some("hash-1".to_owned()));
        Ok(())
    }

    #[tokio::test]
    async fn forced_reinstall_restores_same_hash_and_preserves_personal_files()
    -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route(
            "/mods.zip",
            TestResponse::ok(zip_bytes(&[("mods/a.jar", b"a"), ("mods/b.jar", b"b")])?),
        );
        let root = TempDir::new()?;
        let config = client_config(server.url("mods.zip")?, "same");
        sync(&server, &config, &root).await?;
        fs::remove_file(root.path().join("mods/a.jar"))?;
        fs::write(root.path().join("mods/b.jar"), b"corrupt")?;
        fs::write(root.path().join("mods/stale.jar"), b"stale")?;
        let personal = [
            "config/mod.toml",
            "options.txt",
            "saves/world/level.dat",
            "screenshots/picture.png",
            "servers.dat",
        ];
        for relative in personal {
            let path = root.path().join(relative);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, relative.as_bytes())?;
        }
        let result = reinstall_mods(
            &build_http_client()?,
            &config,
            root.path(),
            &ModsSyncCancellation::default(),
            |_| {},
        )
        .await?;
        assert_eq!(result, ModsSyncOutcome::Updated { files_installed: 2 });
        assert_eq!(server.request_count("/mods.zip"), 2);
        assert_eq!(fs::read(root.path().join("mods/a.jar"))?, b"a");
        assert_eq!(fs::read(root.path().join("mods/b.jar"))?, b"b");
        assert!(!root.path().join("mods/stale.jar").exists());
        for relative in personal {
            assert_eq!(fs::read(root.path().join(relative))?, relative.as_bytes());
        }
        assert_eq!(read_hash(root.path())?, Some("same".to_owned()));
        Ok(())
    }

    #[tokio::test]
    async fn forced_reinstall_failure_preserves_same_hash_and_working_mods()
    -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route("/mods.zip", TestResponse::ok(b"invalid zip".to_vec()));
        let root = TempDir::new()?;
        fs::create_dir(root.path().join("mods"))?;
        fs::write(root.path().join("mods/old.jar"), b"old")?;
        fs::write(root.path().join("state.json"), r#"{"modsHash":"same"}"#)?;
        let error = reinstall_mods(
            &build_http_client()?,
            &client_config(server.url("mods.zip")?, "same"),
            root.path(),
            &ModsSyncCancellation::default(),
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ModsSyncError::InvalidArchive(_)));
        assert_eq!(fs::read(root.path().join("mods/old.jar"))?, b"old");
        assert_eq!(read_hash(root.path())?, Some("same".to_owned()));
        assert_eq!(fs::read_dir(root.path())?.count(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn forced_reinstall_cancelled_extraction_preserves_same_hash_and_working_mods()
    -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route(
            "/mods.zip",
            TestResponse::ok(zip_bytes(&[("mods/a.jar", b"a"), ("mods/b.jar", b"b")])?),
        );
        let root = TempDir::new()?;
        fs::create_dir(root.path().join("mods"))?;
        fs::write(root.path().join("mods/old.jar"), b"old")?;
        fs::write(root.path().join("state.json"), r#"{"modsHash":"same"}"#)?;
        let cancellation = ModsSyncCancellation::default();
        let cancel = cancellation.clone();
        let error = reinstall_mods(
            &build_http_client()?,
            &client_config(server.url("mods.zip")?, "same"),
            root.path(),
            &cancellation,
            move |event| {
                if event.stage == ModsSyncStage::Extracting
                    && event.current_file.as_deref() == Some("b.jar")
                {
                    cancel.cancel();
                }
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ModsSyncError::Cancelled));
        assert_eq!(fs::read(root.path().join("mods/old.jar"))?, b"old");
        assert_eq!(read_hash(root.path())?, Some("same".to_owned()));
        assert_eq!(fs::read_dir(root.path())?.count(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn reports_archive_phases_and_each_extracted_mod() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route(
            "/mods.zip",
            TestResponse::ok(zip_bytes(&[
                ("mods/a.jar", b"a"),
                ("mods/sub/b.jar", b"second"),
            ])?),
        );
        let root = TempDir::new()?;
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = events.clone();
        sync_mods(
            &build_http_client()?,
            &client_config(server.url("mods.zip")?, "new"),
            root.path(),
            &ModsSyncCancellation::default(),
            move |event| capture.lock().unwrap().push(event),
        )
        .await?;
        let events = events.lock().unwrap();
        assert_eq!(events.first().unwrap().stage, ModsSyncStage::Checking);
        for stage in [
            ModsSyncStage::Requesting,
            ModsSyncStage::Downloading,
            ModsSyncStage::Inspecting,
            ModsSyncStage::Extracting,
            ModsSyncStage::Committing,
            ModsSyncStage::Complete,
        ] {
            assert!(
                events.iter().any(|event| event.stage == stage),
                "missing stage {stage:?}"
            );
        }
        for file in ["a.jar", "sub/b.jar"] {
            assert!(
                events
                    .iter()
                    .any(|event| event.stage == ModsSyncStage::Extracting
                        && event.current_file.as_deref() == Some(file)),
                "missing mod {file}"
            );
        }
        let done = events.last().unwrap();
        assert_eq!(done.files_completed, 2);
        assert_eq!(done.files_total, Some(2));
        assert_eq!(done.fraction(), Some(1.0));
        Ok(())
    }

    #[tokio::test]
    async fn unknown_size_download_reports_bytes_and_still_installs_mods()
    -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let archive = zip_bytes(&[("mods/one.jar", b"one")])?;
        let expected = archive.len() as u64;
        server.route(
            "/mods.zip",
            TestResponse::slow(archive, 8, Duration::from_millis(20)).chunked(),
        );
        let root = TempDir::new()?;
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = events.clone();
        sync_mods(
            &build_http_client()?,
            &client_config(server.url("mods.zip")?, "streamed"),
            root.path(),
            &ModsSyncCancellation::default(),
            move |event| capture.lock().unwrap().push(event),
        )
        .await?;
        let events = events.lock().unwrap();
        let downloads: Vec<_> = events
            .iter()
            .filter(|event| event.stage == ModsSyncStage::Downloading)
            .collect();
        assert!(
            downloads
                .iter()
                .all(|event| event.total.is_none() && event.fraction().is_none())
        );
        assert_eq!(downloads.last().unwrap().downloaded, expected);
        assert!(downloads.iter().any(|event| event.bytes_per_second > 0));
        assert_eq!(fs::read(root.path().join("mods/one.jar"))?, b"one");
        Ok(())
    }

    #[tokio::test]
    async fn waiting_for_data_reports_heartbeat_and_can_be_cancelled_immediately()
    -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route(
            "/mods.zip",
            TestResponse::slow(
                zip_bytes(&[("new.jar", b"new")])?,
                4,
                Duration::from_secs(60),
            ),
        );
        let root = TempDir::new()?;
        fs::create_dir(root.path().join("mods"))?;
        fs::write(root.path().join("mods/old.jar"), b"old")?;
        fs::write(root.path().join("state.json"), r#"{"modsHash":"old"}"#)?;
        let cancellation = ModsSyncCancellation::default();
        let cancel = cancellation.clone();
        let error = tokio::time::timeout(
            Duration::from_secs(3),
            sync_mods(
                &build_http_client()?,
                &client_config(server.url("mods.zip")?, "new"),
                root.path(),
                &cancellation,
                move |event| {
                    if event.stage == ModsSyncStage::Downloading
                        && event.idle_for >= Duration::from_secs(1)
                    {
                        cancel.cancel();
                    }
                },
            ),
        )
        .await?
        .unwrap_err();
        assert!(matches!(error, ModsSyncError::Cancelled));
        assert_eq!(fs::read(root.path().join("mods/old.jar"))?, b"old");
        assert_eq!(read_hash(root.path())?, Some("old".to_owned()));
        assert_eq!(fs::read_dir(root.path())?.count(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_on_a_named_mod_preserves_the_previous_set() -> Result<(), Box<dyn Error>>
    {
        let server = TestServer::start().await?;
        server.route(
            "/mods.zip",
            TestResponse::ok(zip_bytes(&[("mods/a.jar", b"a"), ("mods/b.jar", b"b")])?),
        );
        let root = TempDir::new()?;
        fs::create_dir(root.path().join("mods"))?;
        fs::write(root.path().join("mods/old.jar"), b"old")?;
        fs::write(root.path().join("state.json"), r#"{"modsHash":"old"}"#)?;
        let cancellation = ModsSyncCancellation::default();
        let cancel = cancellation.clone();
        let error = sync_mods(
            &build_http_client()?,
            &client_config(server.url("mods.zip")?, "new"),
            root.path(),
            &cancellation,
            move |event| {
                if event.stage == ModsSyncStage::Extracting
                    && event.current_file.as_deref() == Some("b.jar")
                {
                    cancel.cancel();
                }
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ModsSyncError::Cancelled));
        assert_eq!(fs::read(root.path().join("mods/old.jar"))?, b"old");
        assert!(!root.path().join("mods/a.jar").exists());
        assert_eq!(read_hash(root.path())?, Some("old".to_owned()));
        assert_eq!(fs::read_dir(root.path())?.count(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn active_mods_download_can_outlast_a_read_timeout() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let archive = zip_bytes(&[("mods/a.jar", b"expected mod")])?;
        server.route(
            "/mods.zip",
            TestResponse::slow(archive, 4, std::time::Duration::from_millis(20)),
        );
        let root = TempDir::new()?;
        let client = crate::http::build_http_client_with_read_timeout(
            std::time::Duration::from_millis(250),
        )?;
        let started = std::time::Instant::now();
        let result = sync_mods(
            &client,
            &client_config(server.url("mods.zip")?, "slow"),
            root.path(),
            &ModsSyncCancellation::default(),
            |_| {},
        )
        .await?;
        assert!(started.elapsed() > std::time::Duration::from_millis(250));
        assert_eq!(result, ModsSyncOutcome::Updated { files_installed: 1 });
        assert_eq!(fs::read(root.path().join("mods/a.jar"))?, b"expected mod");
        assert_eq!(read_hash(root.path())?, Some("slow".to_owned()));
        Ok(())
    }

    #[tokio::test]
    async fn stalled_mods_download_times_out_and_preserves_existing_installation()
    -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        server.route(
            "/mods.zip",
            TestResponse::slow(
                zip_bytes(&[("new.jar", b"new")])?,
                4,
                std::time::Duration::from_secs(1),
            ),
        );
        let root = TempDir::new()?;
        fs::create_dir(root.path().join("mods"))?;
        fs::write(root.path().join("mods/old.jar"), b"old")?;
        fs::write(root.path().join("state.json"), r#"{"modsHash":"old"}"#)?;
        let client = crate::http::build_http_client_with_read_timeout(
            std::time::Duration::from_millis(100),
        )?;
        let error = sync_mods(
            &client,
            &client_config(server.url("mods.zip")?, "new"),
            root.path(),
            &ModsSyncCancellation::default(),
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ModsSyncError::Request { source, .. } if source.is_timeout()));
        assert_eq!(fs::read(root.path().join("mods/old.jar"))?, b"old");
        assert_eq!(read_hash(root.path())?, Some("old".to_owned()));
        assert_eq!(
            fs::read_dir(root.path())?.count(),
            2,
            "temporary download must be removed"
        );
        Ok(())
    }

    #[tokio::test]
    async fn changed_hash_replaces_mods() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let root = TempDir::new()?;
        server.route(
            "/mods.zip",
            TestResponse::ok(zip_bytes(&[("old.jar", b"old")])?),
        );
        sync(
            &server,
            &client_config(server.url("mods.zip")?, "old"),
            &root,
        )
        .await?;
        server.route(
            "/mods.zip",
            TestResponse::ok(zip_bytes(&[("mods/new.jar", b"new")])?),
        );

        sync(
            &server,
            &client_config(server.url("mods.zip")?, "new"),
            &root,
        )
        .await?;

        assert!(!root.path().join("mods/old.jar").exists());
        assert_eq!(fs::read(root.path().join("mods/new.jar"))?, b"new");
        assert_eq!(read_hash(root.path())?, Some("new".to_owned()));
        assert_eq!(server.request_count("/mods.zip"), 2);
        Ok(())
    }

    #[tokio::test]
    async fn invalid_zip_preserves_working_mods_and_hash() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let root = TempDir::new()?;
        server.route(
            "/mods.zip",
            TestResponse::ok(zip_bytes(&[("old.jar", b"old")])?),
        );
        sync(
            &server,
            &client_config(server.url("mods.zip")?, "old"),
            &root,
        )
        .await?;
        server.route("/mods.zip", TestResponse::ok(b"not a zip".to_vec()));

        let error = sync(
            &server,
            &client_config(server.url("mods.zip")?, "new"),
            &root,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ModsSyncError::InvalidArchive(_)));
        assert_eq!(fs::read(root.path().join("mods/old.jar"))?, b"old");
        assert_eq!(read_hash(root.path())?, Some("old".to_owned()));
        Ok(())
    }

    #[tokio::test]
    async fn blocks_zip_slip_and_preserves_working_mods() -> Result<(), Box<dyn Error>> {
        for unsafe_path in [
            "../escape.jar",
            "/absolute.jar",
            "C:/drive.jar",
            r"\\server\share\unc.jar",
            "mods/../../escape.jar",
        ] {
            let server = TestServer::start().await?;
            let root = TempDir::new()?;
            fs::create_dir(root.path().join("mods"))?;
            fs::write(root.path().join("mods/working.jar"), b"working")?;
            fs::write(root.path().join("state.json"), r#"{"modsHash":"old"}"#)?;
            server.route(
                "/mods.zip",
                TestResponse::ok(zip_bytes(&[(unsafe_path, b"bad")])?),
            );

            let error = sync(
                &server,
                &client_config(server.url("mods.zip")?, "new"),
                &root,
            )
            .await
            .unwrap_err();

            assert!(matches!(error, ModsSyncError::UnsafeEntry { .. }));
            assert_eq!(fs::read(root.path().join("mods/working.jar"))?, b"working");
            assert_eq!(read_hash(root.path())?, Some("old".to_owned()));
        }
        Ok(())
    }

    #[tokio::test]
    async fn rejects_mixed_layout() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let root = TempDir::new()?;
        server.route(
            "/mods.zip",
            TestResponse::ok(zip_bytes(&[("mods/a.jar", b"a"), ("b.jar", b"b")])?),
        );

        let error = sync(
            &server,
            &client_config(server.url("mods.zip")?, "new"),
            &root,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ModsSyncError::MixedLayout));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_symlink_entries_without_materializing_them() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let root = TempDir::new()?;
        fs::create_dir(root.path().join("mods"))?;
        fs::write(root.path().join("mods/working.jar"), b"working")?;
        fs::write(root.path().join("state.json"), r#"{"modsHash":"old"}"#)?;
        server.route("/mods.zip", TestResponse::ok(symlink_zip()?));

        let error = sync(
            &server,
            &client_config(server.url("mods.zip")?, "new"),
            &root,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ModsSyncError::SymlinkEntry { .. }));
        assert_eq!(fs::read(root.path().join("mods/working.jar"))?, b"working");
        assert!(!root.path().join("mods/link.jar").exists());
        assert_eq!(read_hash(root.path())?, Some("old".to_owned()));
        Ok(())
    }

    async fn sync(
        server: &TestServer,
        config: &ClientConfig,
        root: &TempDir,
    ) -> Result<ModsSyncOutcome, ModsSyncError> {
        let _ = server;
        sync_mods(
            &build_http_client().unwrap(),
            config,
            root.path(),
            &ModsSyncCancellation::default(),
            |_| {},
        )
        .await
    }

    fn client_config(mods_url: reqwest::Url, hash: &str) -> ClientConfig {
        ClientConfig {
            version: "1.21.1".to_owned(),
            loader: "neoforge".to_owned(),
            loader_version: "21.1.248".to_owned(),
            java_version: 21,
            min_memory: 2048,
            max_memory: 4096,
            jvm_args: vec!["-Dfile.encoding=UTF-8".to_owned()],
            servers: Vec::new(),
            mods_hash: hash.to_owned(),
            mods_url,
            server_ip: "server:25565".to_owned(),
        }
    }

    fn zip_bytes(entries: &[(&str, &[u8])]) -> Result<Vec<u8>, zip::result::ZipError> {
        let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, contents) in entries {
            writer.start_file(*name, options)?;
            writer.write_all(contents)?;
        }
        Ok(writer.finish()?.into_inner())
    }

    fn symlink_zip() -> Result<Vec<u8>, zip::result::ZipError> {
        let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer.add_symlink(
            "mods/link.jar",
            "../outside.jar",
            SimpleFileOptions::default(),
        )?;
        Ok(writer.finish()?.into_inner())
    }

    fn read_hash(root: &Path) -> Result<Option<String>, Box<dyn Error>> {
        let state: LauncherLocalState =
            serde_json::from_slice(&fs::read(root.join("state.json"))?)?;
        Ok(state.mods_hash)
    }
}
