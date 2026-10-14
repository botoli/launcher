use std::{
    error::Error,
    fmt,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use zip::{ZipArchive, result::ZipError};

use super::{
    metadata::VersionMetadata,
    rules::{OperatingSystem, RuleContext, rules_allow},
};

static BACKUP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeArchive {
    pub library_name: String,
    pub classifier: String,
    pub archive_path: PathBuf,
    pub excludes: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NativeExtractionSummary {
    pub archives_processed: usize,
    pub files_extracted: usize,
    pub entries_excluded: usize,
}

#[derive(Debug)]
pub enum NativeExtractionError {
    MissingNativeArtifact {
        library_name: String,
        classifier: String,
    },
    InvalidNativeArtifactPath {
        library_name: String,
        path: String,
    },
    CreateStaging {
        path: PathBuf,
        source: io::Error,
    },
    OpenArchive {
        path: PathBuf,
        source: io::Error,
    },
    InvalidArchive {
        path: PathBuf,
        source: ZipError,
    },
    UnsafeEntry {
        archive_path: PathBuf,
        entry_name: String,
    },
    SymlinkEntry {
        archive_path: PathBuf,
        entry_name: String,
    },
    WriteEntry {
        path: PathBuf,
        source: io::Error,
    },
    Commit {
        path: PathBuf,
        source: io::Error,
    },
    Rollback {
        destination: PathBuf,
        backup: PathBuf,
        commit_source: io::Error,
        rollback_source: io::Error,
    },
    CleanupBackup {
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for NativeExtractionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingNativeArtifact {
                library_name,
                classifier,
            } => write!(
                formatter,
                "library {library_name} has no artifact for native classifier {classifier}"
            ),
            Self::InvalidNativeArtifactPath { library_name, path } => write!(
                formatter,
                "library {library_name} has unsafe native artifact path {path}"
            ),
            Self::CreateStaging { path, source } => write!(
                formatter,
                "failed to create native staging directory near {}: {source}",
                path.display()
            ),
            Self::OpenArchive { path, source } => {
                write!(
                    formatter,
                    "failed to open native archive {}: {source}",
                    path.display()
                )
            }
            Self::InvalidArchive { path, source } => write!(
                formatter,
                "native archive {} is not a valid ZIP/JAR: {source}",
                path.display()
            ),
            Self::UnsafeEntry {
                archive_path,
                entry_name,
            } => write!(
                formatter,
                "native archive {} contains unsafe entry {entry_name}",
                archive_path.display()
            ),
            Self::SymlinkEntry {
                archive_path,
                entry_name,
            } => write!(
                formatter,
                "native archive {} contains symlink entry {entry_name}",
                archive_path.display()
            ),
            Self::WriteEntry { path, source } => {
                write!(
                    formatter,
                    "failed to extract native file {}: {source}",
                    path.display()
                )
            }
            Self::Commit { path, source } => write!(
                formatter,
                "failed to commit prepared natives directory {}: {source}",
                path.display()
            ),
            Self::Rollback {
                destination,
                backup,
                commit_source,
                rollback_source,
            } => write!(
                formatter,
                "failed to commit {} ({commit_source}) and restore backup {} ({rollback_source})",
                destination.display(),
                backup.display()
            ),
            Self::CleanupBackup { path, source } => write!(
                formatter,
                "new natives are valid, but old backup {} could not be removed: {source}",
                path.display()
            ),
        }
    }
}

impl Error for NativeExtractionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CreateStaging { source, .. }
            | Self::OpenArchive { source, .. }
            | Self::WriteEntry { source, .. }
            | Self::Commit { source, .. }
            | Self::CleanupBackup { source, .. } => Some(source),
            Self::InvalidArchive { source, .. } => Some(source),
            Self::Rollback { commit_source, .. } => Some(commit_source),
            Self::MissingNativeArtifact { .. }
            | Self::InvalidNativeArtifactPath { .. }
            | Self::UnsafeEntry { .. }
            | Self::SymlinkEntry { .. } => None,
        }
    }
}

pub fn resolve_native_archives(
    metadata: &VersionMetadata,
    libraries_root: &Path,
    operating_system: OperatingSystem,
) -> Result<Vec<NativeArchive>, NativeExtractionError> {
    let rule_context = RuleContext {
        operating_system,
        features: Default::default(),
    };
    let mut archives = Vec::new();

    for library in &metadata.libraries {
        if !rules_allow(&library.rules, &rule_context) {
            continue;
        }
        let Some(classifier) = library.native_classifier(operating_system) else {
            continue;
        };
        let (_, artifact) = library.native_artifact(operating_system).ok_or_else(|| {
            NativeExtractionError::MissingNativeArtifact {
                library_name: library.name.clone(),
                classifier: classifier.clone(),
            }
        })?;
        let relative_path = safe_relative_path(&artifact.path).ok_or_else(|| {
            NativeExtractionError::InvalidNativeArtifactPath {
                library_name: library.name.clone(),
                path: artifact.path.clone(),
            }
        })?;
        archives.push(NativeArchive {
            library_name: library.name.clone(),
            classifier,
            archive_path: libraries_root.join(relative_path),
            excludes: library
                .extract
                .as_ref()
                .map(|extract| extract.exclude.clone())
                .unwrap_or_default(),
        });
    }

    Ok(archives)
}

pub fn extract_native_libraries(
    metadata: &VersionMetadata,
    libraries_root: &Path,
    natives_directory: &Path,
    operating_system: OperatingSystem,
) -> Result<NativeExtractionSummary, NativeExtractionError> {
    let archives = resolve_native_archives(metadata, libraries_root, operating_system)?;
    extract_archives(&archives, natives_directory)
}

fn extract_archives(
    archives: &[NativeArchive],
    natives_directory: &Path,
) -> Result<NativeExtractionSummary, NativeExtractionError> {
    let parent = usable_parent(natives_directory);
    fs::create_dir_all(parent).map_err(|source| NativeExtractionError::CreateStaging {
        path: parent.to_owned(),
        source,
    })?;
    let staging = tempfile::Builder::new()
        .prefix(".natives-staging-")
        .tempdir_in(parent)
        .map_err(|source| NativeExtractionError::CreateStaging {
            path: parent.to_owned(),
            source,
        })?;
    let mut summary = NativeExtractionSummary::default();

    for native_archive in archives {
        extract_archive(native_archive, staging.path(), &mut summary)?;
        summary.archives_processed += 1;
    }

    commit_staging(staging.path(), natives_directory)?;
    Ok(summary)
}

fn extract_archive(
    native_archive: &NativeArchive,
    staging_directory: &Path,
    summary: &mut NativeExtractionSummary,
) -> Result<(), NativeExtractionError> {
    let file = File::open(&native_archive.archive_path).map_err(|source| {
        NativeExtractionError::OpenArchive {
            path: native_archive.archive_path.clone(),
            source,
        }
    })?;
    let mut archive =
        ZipArchive::new(file).map_err(|source| NativeExtractionError::InvalidArchive {
            path: native_archive.archive_path.clone(),
            source,
        })?;

    for index in 0..archive.len() {
        let mut entry =
            archive
                .by_index(index)
                .map_err(|source| NativeExtractionError::InvalidArchive {
                    path: native_archive.archive_path.clone(),
                    source,
                })?;
        let entry_name = entry.name().to_owned();
        if is_excluded(&entry_name, &native_archive.excludes) {
            summary.entries_excluded += 1;
            continue;
        }
        if is_symlink(&entry) {
            return Err(NativeExtractionError::SymlinkEntry {
                archive_path: native_archive.archive_path.clone(),
                entry_name,
            });
        }
        let relative_path =
            safe_relative_path(&entry_name).ok_or_else(|| NativeExtractionError::UnsafeEntry {
                archive_path: native_archive.archive_path.clone(),
                entry_name: entry_name.clone(),
            })?;
        let destination = staging_directory.join(relative_path);
        if entry.is_dir() {
            fs::create_dir_all(&destination).map_err(|source| {
                NativeExtractionError::WriteEntry {
                    path: destination,
                    source,
                }
            })?;
            continue;
        }

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|source| NativeExtractionError::WriteEntry {
                path: parent.to_owned(),
                source,
            })?;
        }
        let mut output =
            File::create(&destination).map_err(|source| NativeExtractionError::WriteEntry {
                path: destination.clone(),
                source,
            })?;
        io::copy(&mut entry, &mut output).map_err(|source| NativeExtractionError::WriteEntry {
            path: destination,
            source,
        })?;
        summary.files_extracted += 1;
    }
    Ok(())
}

fn is_excluded(entry_name: &str, excludes: &[String]) -> bool {
    let normalized_entry = entry_name.replace('\\', "/");
    excludes.iter().any(|exclude| {
        let normalized_exclude = exclude.replace('\\', "/");
        normalized_entry.starts_with(&normalized_exclude)
    })
}

fn is_symlink(entry: &zip::read::ZipFile<'_>) -> bool {
    entry
        .unix_mode()
        .is_some_and(|mode| mode & 0o170000 == 0o120000)
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

fn usable_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn commit_staging(
    staging_directory: &Path,
    natives_directory: &Path,
) -> Result<(), NativeExtractionError> {
    let backup = unique_backup_path(natives_directory);
    let had_previous = fs::symlink_metadata(natives_directory).is_ok();
    if had_previous {
        fs::rename(natives_directory, &backup).map_err(|source| NativeExtractionError::Commit {
            path: natives_directory.to_owned(),
            source,
        })?;
    }

    if let Err(commit_source) = fs::rename(staging_directory, natives_directory) {
        if had_previous && let Err(rollback_source) = fs::rename(&backup, natives_directory) {
            return Err(NativeExtractionError::Rollback {
                destination: natives_directory.to_owned(),
                backup,
                commit_source,
                rollback_source,
            });
        }
        return Err(NativeExtractionError::Commit {
            path: natives_directory.to_owned(),
            source: commit_source,
        });
    }

    if had_previous {
        remove_path(&backup).map_err(|source| NativeExtractionError::CleanupBackup {
            path: backup,
            source,
        })?;
    }
    Ok(())
}

fn unique_backup_path(natives_directory: &Path) -> PathBuf {
    let parent = usable_parent(natives_directory);
    let name = natives_directory
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("natives"))
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

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, io::Write};

    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use super::*;
    use crate::minecraft::{
        Library, LibraryArtifact, LibraryDownloads, LibraryExtract, VersionDownloads,
    };

    #[test]
    fn extracts_native_jar_with_nested_files_and_excludes() {
        let temp = tempfile::tempdir().unwrap();
        let (metadata, archive_path) = native_metadata(temp.path(), vec!["META-INF/".to_owned()]);
        write_zip(
            &archive_path,
            &[
                ("native.dll", b"native"),
                ("nested/helper.dll", b"nested"),
                ("META-INF/MANIFEST.MF", b"excluded"),
            ],
        );
        let destination = temp.path().join("natives");

        let summary = extract_native_libraries(
            &metadata,
            temp.path(),
            &destination,
            OperatingSystem::Windows,
        )
        .unwrap();

        assert_eq!(fs::read(destination.join("native.dll")).unwrap(), b"native");
        assert_eq!(
            fs::read(destination.join("nested/helper.dll")).unwrap(),
            b"nested"
        );
        assert!(!destination.join("META-INF/MANIFEST.MF").exists());
        assert_eq!(summary.files_extracted, 2);
        assert_eq!(summary.entries_excluded, 1);
    }

    #[test]
    fn rejects_parent_traversal_and_cleans_partial_staging() {
        assert_unsafe_archive_is_rejected("../escaped.dll");
    }

    #[test]
    fn rejects_absolute_entry() {
        assert_unsafe_archive_is_rejected("/absolute.dll");
    }

    #[test]
    fn rejects_windows_drive_entry() {
        assert_unsafe_archive_is_rejected("C:/Windows/evil.dll");
    }

    #[test]
    fn failed_extraction_preserves_old_valid_natives_directory() {
        let temp = tempfile::tempdir().unwrap();
        let (metadata, archive_path) = native_metadata(temp.path(), Vec::new());
        write_zip(
            &archive_path,
            &[("new.dll", b"new"), ("../escaped.dll", b"escape")],
        );
        let destination = temp.path().join("natives");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("old.dll"), b"old").unwrap();

        let error = extract_native_libraries(
            &metadata,
            temp.path(),
            &destination,
            OperatingSystem::Windows,
        )
        .unwrap_err();

        assert!(matches!(error, NativeExtractionError::UnsafeEntry { .. }));
        assert_eq!(fs::read(destination.join("old.dll")).unwrap(), b"old");
        assert!(!destination.join("new.dll").exists());
    }

    fn assert_unsafe_archive_is_rejected(unsafe_name: &str) {
        let temp = tempfile::tempdir().unwrap();
        let (metadata, archive_path) = native_metadata(temp.path(), Vec::new());
        write_zip(
            &archive_path,
            &[("partial.dll", b"partial"), (unsafe_name, b"escape")],
        );
        let destination = temp.path().join("natives");

        let error = extract_native_libraries(
            &metadata,
            temp.path(),
            &destination,
            OperatingSystem::Windows,
        )
        .unwrap_err();

        assert!(matches!(error, NativeExtractionError::UnsafeEntry { .. }));
        assert!(!destination.exists());
        assert!(fs::read_dir(temp.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("staging")
        }));
    }

    fn native_metadata(root: &Path, excludes: Vec<String>) -> (VersionMetadata, PathBuf) {
        let classifier = if cfg!(target_pointer_width = "64") {
            "natives-windows-64"
        } else {
            "natives-windows-32"
        };
        let relative_path = format!("native/{classifier}.jar");
        let archive_path = root.join(&relative_path);
        fs::create_dir_all(archive_path.parent().unwrap()).unwrap();
        let artifact = LibraryArtifact {
            path: relative_path,
            sha1: None,
            size: None,
            url: None,
        };
        let mut classifiers = BTreeMap::new();
        classifiers.insert(classifier.to_owned(), artifact);
        let mut natives = BTreeMap::new();
        natives.insert("windows".to_owned(), "natives-windows-${arch}".to_owned());

        (
            VersionMetadata {
                id: "test".to_owned(),
                main_class: "Main".to_owned(),
                assets: String::new(),
                asset_index: None,
                libraries: vec![Library {
                    name: "test:native:1".to_owned(),
                    downloads: Some(LibraryDownloads {
                        artifact: None,
                        classifiers,
                    }),
                    natives,
                    rules: Vec::new(),
                    extract: Some(LibraryExtract { exclude: excludes }),
                }],
                downloads: VersionDownloads::default(),
                arguments: None,
                legacy_minecraft_arguments: None,
                java_version: None,
                version_type: None,
            },
            archive_path,
        )
    }

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut writer = ZipWriter::new(file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for (name, contents) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(contents).unwrap();
        }
        writer.finish().unwrap();
    }
}
