use std::{
    error::Error,
    fmt,
    fs::{self, File},
    io::{self, BufReader, Read},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::manifest::PackManifest;

const HASH_BUFFER_SIZE: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidManifestPathReason {
    Empty,
    Absolute,
    ParentTraversal,
    AmbiguousComponent,
}

impl fmt::Display for InvalidManifestPathReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Empty => "path is empty",
            Self::Absolute => "path is absolute",
            Self::ParentTraversal => "path contains a parent traversal component",
            Self::AmbiguousComponent => "path contains an empty or current-directory component",
        };
        formatter.write_str(message)
    }
}

#[derive(Debug)]
pub enum VerificationError {
    RootDirectory {
        path: PathBuf,
        source: io::Error,
    },
    RootIsNotDirectory {
        path: PathBuf,
    },
    InvalidManifestPath {
        path: String,
        reason: InvalidManifestPathReason,
    },
    PathEscapesRoot {
        path: String,
        resolved_path: PathBuf,
    },
    InspectPath {
        path: PathBuf,
        source: io::Error,
    },
    OpenFile {
        path: PathBuf,
        source: io::Error,
    },
    ReadFile {
        path: PathBuf,
        source: io::Error,
    },
    InvalidSha256 {
        path: String,
        sha256: String,
    },
}

impl fmt::Display for VerificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootDirectory { path, .. } => {
                write!(
                    formatter,
                    "failed to access root directory {}",
                    path.display()
                )
            }
            Self::RootIsNotDirectory { path } => {
                write!(
                    formatter,
                    "installation root is not a directory: {}",
                    path.display()
                )
            }
            Self::InvalidManifestPath { path, reason } => {
                write!(formatter, "invalid manifest path {path:?}: {reason}")
            }
            Self::PathEscapesRoot {
                path,
                resolved_path,
            } => write!(
                formatter,
                "manifest path {path:?} resolves outside the installation root: {}",
                resolved_path.display()
            ),
            Self::InspectPath { path, .. } => {
                write!(formatter, "failed to inspect path {}", path.display())
            }
            Self::OpenFile { path, .. } => {
                write!(formatter, "failed to open file {}", path.display())
            }
            Self::ReadFile { path, .. } => {
                write!(formatter, "failed to read file {}", path.display())
            }
            Self::InvalidSha256 { path, sha256 } => write!(
                formatter,
                "manifest file {path:?} has an invalid SHA-256 value {sha256:?}"
            ),
        }
    }
}

impl Error for VerificationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::RootDirectory { source, .. }
            | Self::InspectPath { source, .. }
            | Self::OpenFile { source, .. }
            | Self::ReadFile { source, .. } => Some(source),
            Self::RootIsNotDirectory { .. }
            | Self::InvalidManifestPath { .. }
            | Self::PathEscapesRoot { .. }
            | Self::InvalidSha256 { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CorruptFile {
    path: String,
    expected_sha256: String,
    actual_sha256: String,
    expected_size: u64,
    actual_size: u64,
}

impl CorruptFile {
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn expected_sha256(&self) -> &str {
        &self.expected_sha256
    }

    pub fn actual_sha256(&self) -> &str {
        &self.actual_sha256
    }

    pub const fn expected_size(&self) -> u64 {
        self.expected_size
    }

    pub const fn actual_size(&self) -> u64 {
        self.actual_size
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerificationResult {
    verified_files: Vec<String>,
    missing_files: Vec<String>,
    corrupt_files: Vec<CorruptFile>,
}

impl VerificationResult {
    pub fn verified_files(&self) -> &[String] {
        &self.verified_files
    }

    pub fn missing_files(&self) -> &[String] {
        &self.missing_files
    }

    pub fn corrupt_files(&self) -> &[CorruptFile] {
        &self.corrupt_files
    }

    pub fn is_valid(&self) -> bool {
        self.missing_files.is_empty() && self.corrupt_files.is_empty()
    }
}

pub fn verify_installation(
    root_dir: impl AsRef<Path>,
    manifest: &PackManifest,
) -> Result<VerificationResult, VerificationError> {
    let canonical_root = canonicalize_root(root_dir.as_ref())?;

    let mut result = VerificationResult::default();
    for manifest_file in &manifest.files {
        validate_sha256(&manifest_file.path, &manifest_file.sha256)?;
        let file_path = resolve_manifest_path(&canonical_root, &manifest_file.path)?;

        let file = match File::open(&file_path) {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                result.missing_files.push(manifest_file.path.clone());
                continue;
            }
            Err(source) => {
                return Err(VerificationError::OpenFile {
                    path: file_path,
                    source,
                });
            }
        };

        let (actual_sha256, actual_size) = hash_file(file, &file_path)?;
        if actual_size == manifest_file.size
            && actual_sha256.eq_ignore_ascii_case(&manifest_file.sha256)
        {
            result.verified_files.push(manifest_file.path.clone());
        } else {
            result.corrupt_files.push(CorruptFile {
                path: manifest_file.path.clone(),
                expected_sha256: manifest_file.sha256.clone(),
                actual_sha256,
                expected_size: manifest_file.size,
                actual_size,
            });
        }
    }

    Ok(result)
}

pub(crate) fn canonicalize_root(root_dir: &Path) -> Result<PathBuf, VerificationError> {
    let canonical_root =
        fs::canonicalize(root_dir).map_err(|source| VerificationError::RootDirectory {
            path: root_dir.to_owned(),
            source,
        })?;
    let root_metadata =
        fs::metadata(&canonical_root).map_err(|source| VerificationError::RootDirectory {
            path: canonical_root.clone(),
            source,
        })?;
    if !root_metadata.is_dir() {
        return Err(VerificationError::RootIsNotDirectory {
            path: canonical_root,
        });
    }

    Ok(canonical_root)
}

pub(crate) fn resolve_manifest_path(
    canonical_root: &Path,
    manifest_path: &str,
) -> Result<PathBuf, VerificationError> {
    let relative_path = normalize_manifest_path(manifest_path)?;
    let file_path = canonical_root.join(relative_path);
    ensure_path_stays_within_root(canonical_root, &file_path, manifest_path)?;
    Ok(file_path)
}

fn normalize_manifest_path(path: &str) -> Result<PathBuf, VerificationError> {
    let components = validated_manifest_path_components(path)?;
    let mut normalized = PathBuf::new();
    for component in components {
        normalized.push(component);
    }
    Ok(normalized)
}

pub(crate) fn validated_manifest_path_components(
    path: &str,
) -> Result<Vec<String>, VerificationError> {
    if path.is_empty() {
        return Err(invalid_path(path, InvalidManifestPathReason::Empty));
    }

    let portable_path = path.replace('\\', "/");
    let bytes = portable_path.as_bytes();
    let has_windows_prefix = bytes.len() >= 2 && bytes[1] == b':';
    if portable_path.starts_with('/') || has_windows_prefix || Path::new(path).is_absolute() {
        return Err(invalid_path(path, InvalidManifestPathReason::Absolute));
    }

    let mut components = Vec::new();
    for component in portable_path.split('/') {
        match component {
            ".." => {
                return Err(invalid_path(
                    path,
                    InvalidManifestPathReason::ParentTraversal,
                ));
            }
            "" | "." => {
                return Err(invalid_path(
                    path,
                    InvalidManifestPathReason::AmbiguousComponent,
                ));
            }
            value => components.push(value.to_owned()),
        }
    }

    Ok(components)
}

fn invalid_path(path: &str, reason: InvalidManifestPathReason) -> VerificationError {
    VerificationError::InvalidManifestPath {
        path: path.to_owned(),
        reason,
    }
}

fn validate_sha256(path: &str, sha256: &str) -> Result<(), VerificationError> {
    if sha256.len() == 64 && sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(VerificationError::InvalidSha256 {
            path: path.to_owned(),
            sha256: sha256.to_owned(),
        })
    }
}

fn ensure_path_stays_within_root(
    canonical_root: &Path,
    candidate: &Path,
    manifest_path: &str,
) -> Result<(), VerificationError> {
    let mut existing_ancestor = candidate;

    loop {
        match fs::canonicalize(existing_ancestor) {
            Ok(resolved_path) => {
                if resolved_path.starts_with(canonical_root) {
                    return Ok(());
                }
                return Err(VerificationError::PathEscapesRoot {
                    path: manifest_path.to_owned(),
                    resolved_path,
                });
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                existing_ancestor =
                    existing_ancestor
                        .parent()
                        .ok_or_else(|| VerificationError::InspectPath {
                            path: candidate.to_owned(),
                            source,
                        })?;
            }
            Err(source) => {
                return Err(VerificationError::InspectPath {
                    path: existing_ancestor.to_owned(),
                    source,
                });
            }
        }
    }
}

fn hash_file(file: File, path: &Path) -> Result<(String, u64), VerificationError> {
    let mut reader = BufReader::with_capacity(HASH_BUFFER_SIZE, file);
    let mut buffer = [0_u8; HASH_BUFFER_SIZE];
    let mut hasher = Sha256::new();
    let mut size = 0_u64;

    loop {
        let read_count =
            reader
                .read(&mut buffer)
                .map_err(|source| VerificationError::ReadFile {
                    path: path.to_owned(),
                    source,
                })?;
        if read_count == 0 {
            break;
        }

        hasher.update(&buffer[..read_count]);
        size += read_count as u64;
    }

    Ok((format!("{:x}", hasher.finalize()), size))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::manifest::ManifestFile;

    #[test]
    fn correct_file_is_reported_as_verified() -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let file = write_fixture(root.path(), "client.jar", b"valid client")?;
        let manifest = manifest_with(vec![file]);

        let result = verify_installation(root.path(), &manifest)?;

        assert!(result.is_valid());
        assert_eq!(result.verified_files(), &["client.jar"]);
        assert!(result.missing_files().is_empty());
        assert!(result.corrupt_files().is_empty());
        Ok(())
    }

    #[test]
    fn absent_file_is_reported_as_missing() -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let manifest = manifest_with(vec![ManifestFile {
            path: "missing.jar".to_owned(),
            sha256: "0".repeat(64),
            size: 128,
        }]);

        let result = verify_installation(root.path(), &manifest)?;

        assert!(!result.is_valid());
        assert_eq!(result.missing_files(), &["missing.jar"]);
        assert!(result.verified_files().is_empty());
        assert!(result.corrupt_files().is_empty());
        Ok(())
    }

    #[test]
    fn wrong_hash_is_reported_as_corrupt() -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let mut file = write_fixture(root.path(), "broken.jar", b"actual bytes")?;
        file.sha256 = "0".repeat(64);
        let manifest = manifest_with(vec![file]);

        let result = verify_installation(root.path(), &manifest)?;

        assert!(!result.is_valid());
        assert_eq!(result.corrupt_files().len(), 1);
        assert_eq!(result.corrupt_files()[0].path(), "broken.jar");
        assert_ne!(
            result.corrupt_files()[0].actual_sha256(),
            result.corrupt_files()[0].expected_sha256()
        );
        Ok(())
    }

    #[test]
    fn multiple_files_are_classified_in_one_result() -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let valid = write_fixture(root.path(), "valid.bin", b"valid")?;
        let mut corrupt = write_fixture(root.path(), "corrupt.bin", b"corrupt")?;
        corrupt.size += 1;
        let missing = ManifestFile {
            path: "missing.bin".to_owned(),
            sha256: "f".repeat(64),
            size: 10,
        };
        let manifest = manifest_with(vec![valid, corrupt, missing]);

        let result = verify_installation(root.path(), &manifest)?;

        assert_eq!(result.verified_files(), &["valid.bin"]);
        assert_eq!(result.missing_files(), &["missing.bin"]);
        assert_eq!(result.corrupt_files().len(), 1);
        assert_eq!(result.corrupt_files()[0].path(), "corrupt.bin");
        Ok(())
    }

    #[test]
    fn empty_manifest_is_valid() -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;

        let result = verify_installation(root.path(), &PackManifest::default())?;

        assert!(result.is_valid());
        assert!(result.verified_files().is_empty());
        assert!(result.missing_files().is_empty());
        assert!(result.corrupt_files().is_empty());
        Ok(())
    }

    #[test]
    fn parent_traversal_is_rejected() -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let manifest = manifest_with(vec![ManifestFile {
            path: "../file".to_owned(),
            sha256: "0".repeat(64),
            size: 0,
        }]);

        let error = verify_installation(root.path(), &manifest).unwrap_err();

        assert!(matches!(
            error,
            VerificationError::InvalidManifestPath {
                reason: InvalidManifestPathReason::ParentTraversal,
                ..
            }
        ));
        Ok(())
    }

    #[test]
    fn absolute_path_is_rejected() -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let manifest = manifest_with(vec![ManifestFile {
            path: "C:\\outside\\file".to_owned(),
            sha256: "0".repeat(64),
            size: 0,
        }]);

        let error = verify_installation(root.path(), &manifest).unwrap_err();

        assert!(matches!(
            error,
            VerificationError::InvalidManifestPath {
                reason: InvalidManifestPathReason::Absolute,
                ..
            }
        ));
        Ok(())
    }

    #[test]
    fn nested_relative_path_is_verified() -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let file = write_fixture(root.path(), "versions/1.21.1/client.jar", b"nested client")?;
        let manifest = manifest_with(vec![file]);

        let result = verify_installation(root.path(), &manifest)?;

        assert!(result.is_valid());
        assert_eq!(result.verified_files(), &["versions/1.21.1/client.jar"]);
        Ok(())
    }

    #[test]
    fn large_file_is_hashed_in_streaming_chunks() -> Result<(), Box<dyn Error>> {
        const CHUNK_COUNT: usize = 80;

        let root = tempfile::tempdir()?;
        let relative_path = "large/client.bin";
        let file_path = root.path().join(relative_path);
        fs::create_dir_all(root.path().join("large"))?;

        let chunk = [0xA5_u8; HASH_BUFFER_SIZE];
        let mut file = File::create(&file_path)?;
        let mut expected_hasher = Sha256::new();
        for _ in 0..CHUNK_COUNT {
            file.write_all(&chunk)?;
            expected_hasher.update(chunk);
        }
        file.flush()?;

        let manifest = manifest_with(vec![ManifestFile {
            path: relative_path.to_owned(),
            sha256: format!("{:x}", expected_hasher.finalize()),
            size: (HASH_BUFFER_SIZE * CHUNK_COUNT) as u64,
        }]);

        let result = verify_installation(root.path(), &manifest)?;

        assert!(result.is_valid());
        assert_eq!(result.verified_files(), &[relative_path]);
        Ok(())
    }

    #[test]
    fn verification_result_exposes_mismatch_details() -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let mut file = write_fixture(root.path(), "changed.dat", b"new content")?;
        let actual_size = file.size;
        file.sha256 = "1".repeat(64);
        file.size = 999;
        let manifest = manifest_with(vec![file]);

        let result = verify_installation(root.path(), &manifest)?;
        let mismatch = &result.corrupt_files()[0];

        assert_eq!(mismatch.path(), "changed.dat");
        assert_eq!(mismatch.expected_sha256(), "1".repeat(64));
        assert_eq!(mismatch.expected_size(), 999);
        assert_eq!(mismatch.actual_size(), actual_size);
        assert!(!mismatch.actual_sha256().is_empty());
        Ok(())
    }

    fn manifest_with(files: Vec<ManifestFile>) -> PackManifest {
        PackManifest {
            version: "test-pack".to_owned(),
            minecraft_version: "1.21.1".to_owned(),
            files,
        }
    }

    fn write_fixture(
        root: &Path,
        relative_path: &str,
        contents: &[u8],
    ) -> Result<ManifestFile, io::Error> {
        let file_path = root.join(relative_path);
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&file_path, contents)?;

        let mut hasher = Sha256::new();
        hasher.update(contents);
        Ok(ManifestFile {
            path: relative_path.to_owned(),
            sha256: format!("{:x}", hasher.finalize()),
            size: contents.len() as u64,
        })
    }
}
