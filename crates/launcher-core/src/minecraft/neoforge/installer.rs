use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use tempfile::NamedTempFile;
use tokio::io::AsyncReadExt;
use zip::ZipArchive;

use crate::{
    JavaRuntime, LaunchSpec, ProcessError, ProcessEvent, launch_process,
    minecraft::{
        AcquisitionCancellation, MinecraftRuntimeLayout, RuntimeDownloadError,
        download::{DownloadDisposition, ensure_verified_file},
    },
};

use super::metadata::{
    MavenCoordinate, NeoForgeInstallProfile, NeoForgeMetadataError, NeoForgeProcessor,
    NeoForgeVersionMetadata, ProfileLibrary, neoforge_version_id,
};

const MAX_CHECKSUM_SIZE: usize = 1024;
const MAX_PROFILE_SIZE: usize = 2 * 1024 * 1024;
const MAX_VERSION_SIZE: usize = 2 * 1024 * 1024;
const DEFAULT_MAVEN_BASE: &str = "https://maven.neoforged.net/releases/";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NeoForgeDistribution {
    pub maven_base_url: reqwest::Url,
}

impl Default for NeoForgeDistribution {
    fn default() -> Self {
        Self {
            maven_base_url: DEFAULT_MAVEN_BASE
                .parse()
                .expect("built-in NeoForge Maven URL is valid"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PreparedNeoForgeRuntime {
    pub profile: NeoForgeInstallProfile,
    pub version: NeoForgeVersionMetadata,
    pub version_json: Vec<u8>,
    pub installer_path: PathBuf,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NeoForgePreparationSummary {
    pub downloaded_files: usize,
    pub reused_files: usize,
    pub processors_run: usize,
    pub cache_hit: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct NeoForgePreparationRequest<'a> {
    pub minecraft_version: &'a str,
    pub loader_version: &'a str,
    pub layout: &'a MinecraftRuntimeLayout,
    pub distribution: &'a NeoForgeDistribution,
}

pub async fn prepare_neoforge_runtime<F>(
    client: &reqwest::Client,
    java: &JavaRuntime,
    request: NeoForgePreparationRequest<'_>,
    cancellation: &AcquisitionCancellation,
    on_progress: F,
) -> Result<(PreparedNeoForgeRuntime, NeoForgePreparationSummary), NeoForgeInstallerError>
where
    F: Fn(u64, u64) + Send + Sync,
{
    let NeoForgePreparationRequest {
        minecraft_version,
        loader_version,
        layout,
        distribution,
    } = request;
    ensure_not_cancelled(cancellation)?;
    let version_id = neoforge_version_id(loader_version)?;
    let installer =
        acquire_installer(client, loader_version, layout, distribution, cancellation).await?;
    tracing::info!(
        loader_version,
        sha1 = %installer.sha1,
        size = installer.size,
        "NeoForge installer verified"
    );

    let installer_path = installer.path.clone();
    let parsed = tokio::task::spawn_blocking(move || parse_installer(&installer_path))
        .await
        .map_err(NeoForgeInstallerError::BlockingTask)??;
    parsed
        .profile
        .validate_pair(minecraft_version, loader_version)?;
    parsed
        .version
        .validate_pair(minecraft_version, loader_version)?;
    let library_index = parsed.profile.library_index()?;

    let total_bytes = library_index
        .values()
        .fold(installer.size, |total, library| {
            total.saturating_add(library.size)
        });
    let mut completed_bytes = installer.size;
    on_progress(completed_bytes, total_bytes);
    let mut summary = NeoForgePreparationSummary::default();
    let libraries_root = layout.libraries_dir();
    for library in library_index.values() {
        ensure_not_cancelled(cancellation)?;
        let destination = libraries_root.join(&library.path);
        let disposition = ensure_verified_file(
            client,
            &library.url,
            &destination,
            library.size,
            &library.sha1,
            cancellation,
            &|bytes| on_progress(completed_bytes.saturating_add(bytes), total_bytes),
        )
        .await
        .map_err(map_download_error)?;
        match disposition {
            DownloadDisposition::AlreadyValid => summary.reused_files += 1,
            DownloadDisposition::Downloaded => summary.downloaded_files += 1,
        }
        completed_bytes = completed_bytes.saturating_add(library.size);
        on_progress(completed_bytes, total_bytes);
    }

    ensure_not_cancelled(cancellation)?;
    let marker_path = runtime_marker_path(layout, &version_id);
    let version_json_path = layout.version_json(&version_id);
    if validate_runtime_marker(
        layout.root(),
        &marker_path,
        minecraft_version,
        loader_version,
        &installer,
        &version_json_path,
    )
    .await?
    {
        summary.cache_hit = true;
        tracing::info!(
            minecraft_version,
            loader_version,
            "NeoForge runtime cache hit"
        );
        return Ok((
            PreparedNeoForgeRuntime {
                profile: parsed.profile,
                version: parsed.version,
                version_json: parsed.version_json,
                installer_path: installer.path,
            },
            summary,
        ));
    }

    fs::create_dir_all(layout.root()).map_err(|source| NeoForgeInstallerError::CreateStaging {
        path: layout.root().to_owned(),
        source,
    })?;
    let staging = tempfile::tempdir_in(layout.root()).map_err(|source| {
        NeoForgeInstallerError::CreateStaging {
            path: layout.root().to_owned(),
            source,
        }
    })?;
    let generated = resolve_data(
        &parsed.profile,
        &library_index,
        &libraries_root,
        staging.path(),
        &installer.path,
        layout.root(),
        &layout.client_jar(minecraft_version),
    )?;

    for (index, processor) in parsed.profile.client_processors().enumerate() {
        ensure_not_cancelled(cancellation)?;
        tracing::info!(
            processor = index + 1,
            artifact = %processor.jar,
            "running NeoForge processor"
        );
        run_processor(
            java,
            processor,
            &library_index,
            &libraries_root,
            &generated.tokens,
            layout.root(),
            cancellation,
        )
        .await?;
        summary.processors_run += 1;
    }

    ensure_not_cancelled(cancellation)?;
    let generated_records = commit_generated_files(&generated.files, layout.root()).await?;
    commit_bytes(&version_json_path, &parsed.version_json).await?;
    let marker = RuntimeMarker {
        schema: 1,
        minecraft_version: minecraft_version.to_owned(),
        loader_version: loader_version.to_owned(),
        installer_sha1: installer.sha1.clone(),
        installer_size: installer.size,
        version_json_sha1: sha1_bytes(&parsed.version_json),
        generated: generated_records,
    };
    let marker_bytes =
        serde_json::to_vec_pretty(&marker).map_err(NeoForgeInstallerError::SerializeMarker)?;
    commit_bytes(&marker_path, &marker_bytes).await?;
    tracing::info!(minecraft_version, loader_version, "NeoForge runtime ready");

    Ok((
        PreparedNeoForgeRuntime {
            profile: parsed.profile,
            version: parsed.version,
            version_json: parsed.version_json,
            installer_path: installer.path,
        },
        summary,
    ))
}

#[derive(Debug)]
struct InstallerArtifact {
    path: PathBuf,
    size: u64,
    sha1: String,
}

async fn acquire_installer(
    client: &reqwest::Client,
    loader_version: &str,
    layout: &MinecraftRuntimeLayout,
    distribution: &NeoForgeDistribution,
    cancellation: &AcquisitionCancellation,
) -> Result<InstallerArtifact, NeoForgeInstallerError> {
    let version_id = neoforge_version_id(loader_version)?;
    let relative = format!("net/neoforged/neoforge/{loader_version}/{version_id}-installer.jar");
    let url = distribution
        .maven_base_url
        .join(&relative)
        .map_err(NeoForgeInstallerError::InstallerUrl)?;
    let checksum_url = reqwest::Url::parse(&format!("{url}.sha1"))
        .map_err(NeoForgeInstallerError::InstallerUrl)?;
    let sha1 = fetch_checksum(client, &checksum_url, cancellation).await?;
    let size = fetch_content_length(client, &url, cancellation).await?;
    let path = layout
        .root()
        .join("installers")
        .join("neoforge")
        .join(loader_version)
        .join(format!("{version_id}-installer.jar"));
    ensure_verified_file(client, &url, &path, size, &sha1, cancellation, &|_| {})
        .await
        .map_err(map_download_error)?;
    Ok(InstallerArtifact { path, size, sha1 })
}

async fn fetch_checksum(
    client: &reqwest::Client,
    url: &reqwest::Url,
    cancellation: &AcquisitionCancellation,
) -> Result<String, NeoForgeInstallerError> {
    let mut response = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(NeoForgeInstallerError::Cancelled),
        result = client.get(url.clone()).send() => result.map_err(|source| NeoForgeInstallerError::Request { url: url.clone(), source })?,
    };
    if !response.status().is_success() {
        return Err(NeoForgeInstallerError::HttpStatus {
            url: url.clone(),
            status: response.status(),
        });
    }
    let mut body = Vec::new();
    while let Some(chunk) = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(NeoForgeInstallerError::Cancelled),
        result = response.chunk() => result.map_err(|source| NeoForgeInstallerError::Request { url: url.clone(), source })?,
    } {
        if body.len().saturating_add(chunk.len()) > MAX_CHECKSUM_SIZE {
            return Err(NeoForgeInstallerError::ChecksumTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    let checksum = std::str::from_utf8(&body)
        .map_err(|_| NeoForgeInstallerError::InvalidChecksum)?
        .split_whitespace()
        .next()
        .ok_or(NeoForgeInstallerError::InvalidChecksum)?
        .to_ascii_lowercase();
    if checksum.len() != 40 || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(NeoForgeInstallerError::InvalidChecksum);
    }
    Ok(checksum)
}

async fn fetch_content_length(
    client: &reqwest::Client,
    url: &reqwest::Url,
    cancellation: &AcquisitionCancellation,
) -> Result<u64, NeoForgeInstallerError> {
    let response = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(NeoForgeInstallerError::Cancelled),
        result = client.head(url.clone()).send() => result.map_err(|source| NeoForgeInstallerError::Request { url: url.clone(), source })?,
    };
    if !response.status().is_success() {
        return Err(NeoForgeInstallerError::HttpStatus {
            url: url.clone(),
            status: response.status(),
        });
    }
    response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|size| *size > 0)
        .ok_or(NeoForgeInstallerError::MissingContentLength)
}

#[derive(Debug)]
struct ParsedInstaller {
    profile: NeoForgeInstallProfile,
    version: NeoForgeVersionMetadata,
    version_json: Vec<u8>,
}

fn parse_installer(path: &Path) -> Result<ParsedInstaller, NeoForgeInstallerError> {
    let profile_json = read_archive_entry(path, "install_profile.json", MAX_PROFILE_SIZE)?;
    let profile: NeoForgeInstallProfile =
        serde_json::from_slice(&profile_json).map_err(NeoForgeInstallerError::InvalidProfile)?;
    let version_entry = profile.json.strip_prefix('/').ok_or_else(|| {
        NeoForgeInstallerError::Metadata(NeoForgeMetadataError::UnsafeArchiveEntry {
            value: profile.json.clone(),
        })
    })?;
    let version_json = read_archive_entry(path, version_entry, MAX_VERSION_SIZE)?;
    let version =
        serde_json::from_slice(&version_json).map_err(NeoForgeInstallerError::InvalidVersion)?;
    Ok(ParsedInstaller {
        profile,
        version,
        version_json,
    })
}

fn read_archive_entry(
    archive_path: &Path,
    entry_name: &str,
    limit: usize,
) -> Result<Vec<u8>, NeoForgeInstallerError> {
    let file =
        fs::File::open(archive_path).map_err(|source| NeoForgeInstallerError::OpenArchive {
            path: archive_path.to_owned(),
            source,
        })?;
    let mut archive = ZipArchive::new(file).map_err(NeoForgeInstallerError::InvalidArchive)?;
    let mut entry = archive.by_name(entry_name).map_err(|source| {
        NeoForgeInstallerError::MissingArchiveEntry {
            entry: entry_name.to_owned(),
            source,
        }
    })?;
    if entry.size() > limit as u64 {
        return Err(NeoForgeInstallerError::ArchiveEntryTooLarge {
            entry: entry_name.to_owned(),
            limit,
        });
    }
    let mut body = Vec::with_capacity(entry.size() as usize);
    entry
        .read_to_end(&mut body)
        .map_err(|source| NeoForgeInstallerError::ReadArchiveEntry {
            entry: entry_name.to_owned(),
            source,
        })?;
    Ok(body)
}

struct ResolvedData {
    tokens: HashMap<String, String>,
    files: Vec<GeneratedFile>,
}

#[derive(Clone, Debug)]
struct GeneratedFile {
    staged: PathBuf,
    destination: PathBuf,
}

fn resolve_data(
    profile: &NeoForgeInstallProfile,
    libraries: &HashMap<String, ProfileLibrary>,
    libraries_root: &Path,
    staging_root: &Path,
    installer_path: &Path,
    runtime_root: &Path,
    minecraft_jar: &Path,
) -> Result<ResolvedData, NeoForgeInstallerError> {
    let mut tokens = HashMap::new();
    let mut files = Vec::new();
    let generated_root = staging_root.join("generated-libraries");
    let embedded_root = staging_root.join("embedded");

    for (name, value) in profile.client_data() {
        let resolved = if let Some(coordinate) = bracketed(value) {
            if let Some(library) = libraries.get(coordinate) {
                libraries_root.join(&library.path)
            } else {
                let coordinate = MavenCoordinate::parse(coordinate)?;
                let staged = generated_root.join(&coordinate.path);
                let destination = libraries_root.join(&coordinate.path);
                if client_processors_reference_token(profile, name) {
                    files.push(GeneratedFile {
                        staged: staged.clone(),
                        destination,
                    });
                }
                staged
            }
        } else if let Some(literal) = quoted(value) {
            tokens.insert(name.to_owned(), literal.to_owned());
            continue;
        } else {
            let entry = value.strip_prefix('/').ok_or_else(|| {
                NeoForgeInstallerError::Metadata(NeoForgeMetadataError::UnsafeArchiveEntry {
                    value: value.to_owned(),
                })
            })?;
            let relative = super::metadata::safe_relative_path(entry).ok_or_else(|| {
                NeoForgeInstallerError::Metadata(NeoForgeMetadataError::UnsafeArchiveEntry {
                    value: value.to_owned(),
                })
            })?;
            let destination = embedded_root.join(relative);
            extract_archive_entry(installer_path, entry, &destination)?;
            destination
        };
        tokens.insert(name.to_owned(), absolute_string(&resolved)?);
    }

    tokens.insert("SIDE".to_owned(), "client".to_owned());
    tokens.insert("MINECRAFT_JAR".to_owned(), absolute_string(minecraft_jar)?);
    tokens.insert("MINECRAFT_VERSION".to_owned(), profile.minecraft.clone());
    tokens.insert("ROOT".to_owned(), absolute_string(runtime_root)?);
    tokens.insert("INSTALLER".to_owned(), absolute_string(installer_path)?);
    tokens.insert("LIBRARY_DIR".to_owned(), absolute_string(libraries_root)?);
    Ok(ResolvedData { tokens, files })
}

fn client_processors_reference_token(profile: &NeoForgeInstallProfile, token: &str) -> bool {
    let placeholder = format!("{{{token}}}");
    profile
        .client_processors()
        .flat_map(|processor| &processor.args)
        .any(|argument| argument.contains(&placeholder))
}

fn extract_archive_entry(
    archive_path: &Path,
    entry_name: &str,
    destination: &Path,
) -> Result<(), NeoForgeInstallerError> {
    let bytes = read_archive_entry(archive_path, entry_name, usize::MAX)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|source| NeoForgeInstallerError::WriteGenerated {
            path: parent.to_owned(),
            source,
        })?;
    }
    fs::write(destination, bytes).map_err(|source| NeoForgeInstallerError::WriteGenerated {
        path: destination.to_owned(),
        source,
    })
}

async fn run_processor(
    java: &JavaRuntime,
    processor: &NeoForgeProcessor,
    libraries: &HashMap<String, ProfileLibrary>,
    libraries_root: &Path,
    tokens: &HashMap<String, String>,
    working_directory: &Path,
    cancellation: &AcquisitionCancellation,
) -> Result<(), NeoForgeInstallerError> {
    let processor_jar = profile_library_path(libraries, libraries_root, &processor.jar)?;
    let mut classpath = vec![processor_jar.clone()];
    for descriptor in &processor.classpath {
        let path = profile_library_path(libraries, libraries_root, descriptor)?;
        if !classpath.contains(&path) {
            classpath.push(path);
        }
    }
    let manifest_path = processor_jar;
    let main_class = tokio::task::spawn_blocking(move || manifest_main_class(&manifest_path))
        .await
        .map_err(NeoForgeInstallerError::BlockingTask)??;
    let mut args = Vec::with_capacity(processor.args.len());
    for argument in &processor.args {
        if let Some(coordinate) = bracketed(argument) {
            args.push(
                profile_library_path(libraries, libraries_root, coordinate)?.into_os_string(),
            );
        } else {
            args.push(OsString::from(replace_tokens(argument, tokens)?));
        }
    }
    let spec = LaunchSpec {
        java_executable: java.executable_path.clone(),
        jvm_args: Vec::new(),
        classpath,
        main_class,
        game_args: args,
        working_directory: working_directory.to_owned(),
    };
    let mut process = launch_process(spec)?;
    loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                process.cancel();
                while process.next_event().await.is_some() {}
                let _ = process.wait().await;
                return Err(NeoForgeInstallerError::Cancelled);
            }
            event = process.next_event() => match event {
                Some(ProcessEvent::Stdout(bytes)) => tracing::info!(target: "neoforge_processor", "{}", String::from_utf8_lossy(&bytes).trim_end()),
                Some(ProcessEvent::Stderr(bytes)) => tracing::warn!(target: "neoforge_processor", "{}", String::from_utf8_lossy(&bytes).trim_end()),
                None => break,
            }
        }
    }
    let exit = process.wait().await?;
    if !exit.status.success() {
        return Err(NeoForgeInstallerError::ProcessorFailed {
            artifact: processor.jar.clone(),
            status: exit.status,
        });
    }
    Ok(())
}

fn profile_library_path(
    libraries: &HashMap<String, ProfileLibrary>,
    libraries_root: &Path,
    descriptor: &str,
) -> Result<PathBuf, NeoForgeInstallerError> {
    libraries
        .get(descriptor)
        .map(|library| libraries_root.join(&library.path))
        .ok_or_else(|| NeoForgeInstallerError::UnknownProcessorLibrary {
            descriptor: descriptor.to_owned(),
        })
}

fn manifest_main_class(path: &Path) -> Result<String, NeoForgeInstallerError> {
    let manifest = read_archive_entry(path, "META-INF/MANIFEST.MF", 1024 * 1024)?;
    let manifest =
        String::from_utf8(manifest).map_err(|_| NeoForgeInstallerError::InvalidManifest {
            path: path.to_owned(),
        })?;
    let mut logical_lines: Vec<String> = Vec::new();
    for line in manifest.lines() {
        if let Some(continuation) = line.strip_prefix(' ') {
            if let Some(previous) = logical_lines.last_mut() {
                previous.push_str(continuation);
            }
        } else {
            logical_lines.push(line.trim_end_matches('\r').to_owned());
        }
    }
    logical_lines
        .iter()
        .find_map(|line| line.strip_prefix("Main-Class:").map(str::trim))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| NeoForgeInstallerError::MissingProcessorMainClass {
            path: path.to_owned(),
        })
}

fn replace_tokens(
    value: &str,
    tokens: &HashMap<String, String>,
) -> Result<String, NeoForgeInstallerError> {
    let mut result = String::new();
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\\' => result.push(characters.next().ok_or_else(|| {
                NeoForgeInstallerError::InvalidProcessorArgument {
                    value: value.to_owned(),
                }
            })?),
            '{' => {
                let mut name = String::new();
                loop {
                    match characters.next() {
                        Some('}') => break,
                        Some(character) => name.push(character),
                        None => {
                            return Err(NeoForgeInstallerError::InvalidProcessorArgument {
                                value: value.to_owned(),
                            });
                        }
                    }
                }
                result.push_str(
                    tokens
                        .get(&name)
                        .ok_or_else(|| NeoForgeInstallerError::MissingProcessorToken { name })?,
                );
            }
            '\'' => loop {
                match characters.next() {
                    Some('\'') => break,
                    Some(character) => result.push(character),
                    None => {
                        return Err(NeoForgeInstallerError::InvalidProcessorArgument {
                            value: value.to_owned(),
                        });
                    }
                }
            },
            character => result.push(character),
        }
    }
    Ok(result)
}

fn bracketed(value: &str) -> Option<&str> {
    value.strip_prefix('[')?.strip_suffix(']')
}

fn quoted(value: &str) -> Option<&str> {
    value.strip_prefix('\'')?.strip_suffix('\'')
}

fn absolute_string(path: &Path) -> Result<String, NeoForgeInstallerError> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(NeoForgeInstallerError::CurrentDirectory)?
            .join(path)
    };
    path.into_os_string()
        .into_string()
        .map_err(|_| NeoForgeInstallerError::NonUnicodePath)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeMarker {
    schema: u32,
    minecraft_version: String,
    loader_version: String,
    installer_sha1: String,
    installer_size: u64,
    version_json_sha1: String,
    generated: Vec<GeneratedRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct GeneratedRecord {
    path: PathBuf,
    size: u64,
    sha1: String,
}

async fn validate_runtime_marker(
    runtime_root: &Path,
    marker_path: &Path,
    minecraft_version: &str,
    loader_version: &str,
    installer: &InstallerArtifact,
    version_json_path: &Path,
) -> Result<bool, NeoForgeInstallerError> {
    let marker_bytes = match tokio::fs::read(marker_path).await {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(NeoForgeInstallerError::ReadMarker {
                path: marker_path.to_owned(),
                source,
            });
        }
    };
    let marker: RuntimeMarker = match serde_json::from_slice(&marker_bytes) {
        Ok(marker) => marker,
        Err(_) => return Ok(false),
    };
    if marker.schema != 1
        || marker.minecraft_version != minecraft_version
        || marker.loader_version != loader_version
        || marker.installer_sha1 != installer.sha1
        || marker.installer_size != installer.size
    {
        return Ok(false);
    }
    if !file_matches(version_json_path, None, &marker.version_json_sha1).await? {
        return Ok(false);
    }
    for generated in &marker.generated {
        if !generated.path.starts_with(runtime_root)
            || !file_matches(&generated.path, Some(generated.size), &generated.sha1).await?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn file_matches(
    path: &Path,
    expected_size: Option<u64>,
    expected_sha1: &str,
) -> Result<bool, NeoForgeInstallerError> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(NeoForgeInstallerError::ReadGenerated {
                path: path.to_owned(),
                source,
            });
        }
    };
    if !metadata.is_file() || expected_size.is_some_and(|size| metadata.len() != size) {
        return Ok(false);
    }
    Ok(hash_file(path).await? == expected_sha1)
}

async fn commit_generated_files(
    files: &[GeneratedFile],
    runtime_root: &Path,
) -> Result<Vec<GeneratedRecord>, NeoForgeInstallerError> {
    let mut records = Vec::with_capacity(files.len());
    for file in files {
        if !file.destination.starts_with(runtime_root) {
            return Err(NeoForgeInstallerError::InvalidGeneratedOutput {
                path: file.destination.clone(),
            });
        }
        let metadata = tokio::fs::metadata(&file.staged).await.map_err(|source| {
            NeoForgeInstallerError::ReadGenerated {
                path: file.staged.clone(),
                source,
            }
        })?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(NeoForgeInstallerError::InvalidGeneratedOutput {
                path: file.staged.clone(),
            });
        }
        if file
            .destination
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("jar"))
        {
            let jar_path = file.staged.clone();
            tokio::task::spawn_blocking(move || validate_zip(&jar_path))
                .await
                .map_err(NeoForgeInstallerError::BlockingTask)??;
        }
        let sha1 = hash_file(&file.staged).await?;
        commit_file(&file.staged, &file.destination).await?;
        records.push(GeneratedRecord {
            path: file.destination.clone(),
            size: metadata.len(),
            sha1,
        });
    }
    Ok(records)
}

fn validate_zip(path: &Path) -> Result<(), NeoForgeInstallerError> {
    let file = fs::File::open(path).map_err(|source| NeoForgeInstallerError::ReadGenerated {
        path: path.to_owned(),
        source,
    })?;
    let archive =
        ZipArchive::new(file).map_err(|_| NeoForgeInstallerError::InvalidGeneratedOutput {
            path: path.to_owned(),
        })?;
    if archive.is_empty() {
        return Err(NeoForgeInstallerError::InvalidGeneratedOutput {
            path: path.to_owned(),
        });
    }
    Ok(())
}

async fn hash_file(path: &Path) -> Result<String, NeoForgeInstallerError> {
    let mut file = tokio::fs::File::open(path).await.map_err(|source| {
        NeoForgeInstallerError::ReadGenerated {
            path: path.to_owned(),
            source,
        }
    })?;
    let mut hasher = Sha1::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).await.map_err(|source| {
            NeoForgeInstallerError::ReadGenerated {
                path: path.to_owned(),
                source,
            }
        })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

async fn commit_file(source: &Path, destination: &Path) -> Result<(), NeoForgeInstallerError> {
    let source = source.to_owned();
    let destination = destination.to_owned();
    tokio::task::spawn_blocking(move || {
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|source| NeoForgeInstallerError::WriteGenerated {
            path: parent.to_owned(),
            source,
        })?;
        let mut temporary = NamedTempFile::new_in(parent).map_err(|source| {
            NeoForgeInstallerError::WriteGenerated {
                path: parent.to_owned(),
                source,
            }
        })?;
        let mut input = fs::File::open(&source).map_err(|source_error| {
            NeoForgeInstallerError::ReadGenerated {
                path: source.clone(),
                source: source_error,
            }
        })?;
        io::copy(&mut input, temporary.as_file_mut()).map_err(|source| {
            NeoForgeInstallerError::WriteGenerated {
                path: temporary.path().to_owned(),
                source,
            }
        })?;
        temporary.as_file_mut().sync_all().map_err(|source| {
            NeoForgeInstallerError::WriteGenerated {
                path: temporary.path().to_owned(),
                source,
            }
        })?;
        if destination.exists() {
            fs::remove_file(&destination).map_err(|source| {
                NeoForgeInstallerError::WriteGenerated {
                    path: destination.clone(),
                    source,
                }
            })?;
        }
        temporary.persist(&destination).map_err(|error| {
            NeoForgeInstallerError::WriteGenerated {
                path: destination,
                source: error.error,
            }
        })?;
        Ok(())
    })
    .await
    .map_err(NeoForgeInstallerError::BlockingTask)?
}

async fn commit_bytes(destination: &Path, bytes: &[u8]) -> Result<(), NeoForgeInstallerError> {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    tokio::fs::create_dir_all(parent).await.map_err(|source| {
        NeoForgeInstallerError::WriteGenerated {
            path: parent.to_owned(),
            source,
        }
    })?;
    let staging =
        tempfile::tempdir_in(parent).map_err(|source| NeoForgeInstallerError::CreateStaging {
            path: parent.to_owned(),
            source,
        })?;
    let source = staging.path().join("contents");
    tokio::fs::write(&source, bytes)
        .await
        .map_err(|source_error| NeoForgeInstallerError::WriteGenerated {
            path: source.clone(),
            source: source_error,
        })?;
    commit_file(&source, destination).await
}

fn runtime_marker_path(layout: &MinecraftRuntimeLayout, version_id: &str) -> PathBuf {
    layout
        .version_dir(version_id)
        .join(".launcher-runtime.json")
}

fn sha1_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha1::digest(bytes))
}

fn ensure_not_cancelled(
    cancellation: &AcquisitionCancellation,
) -> Result<(), NeoForgeInstallerError> {
    if cancellation.is_cancelled() {
        Err(NeoForgeInstallerError::Cancelled)
    } else {
        Ok(())
    }
}

fn map_download_error(error: RuntimeDownloadError) -> NeoForgeInstallerError {
    if matches!(error, RuntimeDownloadError::Cancelled) {
        NeoForgeInstallerError::Cancelled
    } else {
        NeoForgeInstallerError::Download(error)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NeoForgeInstallerError {
    #[error("NeoForge preparation was cancelled")]
    Cancelled,
    #[error("invalid NeoForge distribution metadata: {0}")]
    Metadata(#[from] NeoForgeMetadataError),
    #[error("failed to construct NeoForge installer URL: {0}")]
    InstallerUrl(url::ParseError),
    #[error("request for {url} failed")]
    Request {
        url: reqwest::Url,
        #[source]
        source: reqwest::Error,
    },
    #[error("request for {url} returned {status}")]
    HttpStatus {
        url: reqwest::Url,
        status: reqwest::StatusCode,
    },
    #[error("NeoForge checksum response is too large")]
    ChecksumTooLarge,
    #[error("NeoForge checksum response is invalid")]
    InvalidChecksum,
    #[error("NeoForge installer response has no content length")]
    MissingContentLength,
    #[error("NeoForge artifact download failed: {0}")]
    Download(#[from] RuntimeDownloadError),
    #[error("failed to open NeoForge archive {path}")]
    OpenArchive {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("NeoForge installer is not a valid ZIP archive: {0}")]
    InvalidArchive(zip::result::ZipError),
    #[error("NeoForge installer is missing entry {entry}")]
    MissingArchiveEntry {
        entry: String,
        #[source]
        source: zip::result::ZipError,
    },
    #[error("NeoForge installer entry {entry} exceeds {limit} bytes")]
    ArchiveEntryTooLarge { entry: String, limit: usize },
    #[error("failed to read NeoForge installer entry {entry}")]
    ReadArchiveEntry {
        entry: String,
        #[source]
        source: io::Error,
    },
    #[error("NeoForge install_profile.json is invalid: {0}")]
    InvalidProfile(serde_json::Error),
    #[error("NeoForge version.json is invalid: {0}")]
    InvalidVersion(serde_json::Error),
    #[error("failed to create NeoForge staging area under {path}")]
    CreateStaging {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to write NeoForge generated file {path}")]
    WriteGenerated {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to read NeoForge generated file {path}")]
    ReadGenerated {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("NeoForge processor did not produce a valid output at {path}")]
    InvalidGeneratedOutput { path: PathBuf },
    #[error("NeoForge processor references unknown library {descriptor}")]
    UnknownProcessorLibrary { descriptor: String },
    #[error("NeoForge processor JAR {path} has no Main-Class")]
    MissingProcessorMainClass { path: PathBuf },
    #[error("NeoForge processor JAR {path} has an invalid manifest")]
    InvalidManifest { path: PathBuf },
    #[error("invalid NeoForge processor argument {value}")]
    InvalidProcessorArgument { value: String },
    #[error("NeoForge processor argument references missing token {name}")]
    MissingProcessorToken { name: String },
    #[error("NeoForge processor {artifact} exited with {status}")]
    ProcessorFailed {
        artifact: String,
        status: std::process::ExitStatus,
    },
    #[error("NeoForge processor process failed: {0}")]
    Process(#[from] ProcessError),
    #[error("NeoForge filesystem task failed: {0}")]
    BlockingTask(tokio::task::JoinError),
    #[error("failed to read NeoForge runtime marker {path}")]
    ReadMarker {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to serialize NeoForge runtime marker: {0}")]
    SerializeMarker(serde_json::Error),
    #[error("failed to determine current directory: {0}")]
    CurrentDirectory(io::Error),
    #[error("NeoForge runtime path is not valid Unicode")]
    NonUnicodePath,
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io::Write, time::Duration};

    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use super::*;
    use crate::{
        build_http_client,
        http::test_support::{TestResponse, TestServer},
    };

    const INSTALLER_PATH: &str = "/net/neoforged/neoforge/21.1.248/neoforge-21.1.248-installer.jar";

    #[test]
    fn ignores_unreferenced_intermediate_data_outputs() {
        let profile = NeoForgeInstallProfile::from_json(
            r#"{
                "spec": 1,
                "version": "neoforge-21.1.248",
                "minecraft": "1.21.1",
                "json": "/version.json",
                "data": {
                    "MC_UNPACKED": {"client": "[net.minecraft:client:1.21.1:unpacked]"},
                    "PATCHED": {"client": "[net.neoforged:neoforge:21.1.248:client]"}
                },
                "processors": [
                    {"jar": "tool:binarypatcher:1", "args": ["--output", "{PATCHED}"]},
                    {"sides": ["server"], "jar": "tool:server:1", "args": ["{MC_UNPACKED}"]}
                ],
                "libraries": []
            }"#,
        )
        .unwrap();

        assert!(client_processors_reference_token(&profile, "PATCHED"));
        assert!(!client_processors_reference_token(&profile, "MC_UNPACKED"));
    }

    #[tokio::test]
    async fn prepares_1211_211248_runtime_and_reuses_valid_cache() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let library = b"synthetic NeoForge library".to_vec();
        let library_sha1 = sha1_bytes(&library);
        let library_url = server.url("libraries/example/library.jar")?;
        let installer = synthetic_installer(&library_url, library.len() as u64, &library_sha1, 0)?;
        route_distribution(&server, installer.clone(), &library);
        let runtime = tempfile::tempdir()?;
        let layout = MinecraftRuntimeLayout::new(runtime.path());
        let java = JavaRuntime {
            executable_path: PathBuf::from("unused-java"),
            major_version: 21,
        };
        let distribution = NeoForgeDistribution {
            maven_base_url: server.url("")?,
        };
        let client = build_http_client()?;
        let cancellation = AcquisitionCancellation::default();

        let (prepared, first) = prepare_neoforge_runtime(
            &client,
            &java,
            NeoForgePreparationRequest {
                minecraft_version: "1.21.1",
                loader_version: "21.1.248",
                layout: &layout,
                distribution: &distribution,
            },
            &cancellation,
            |_, _| {},
        )
        .await?;

        assert_eq!(prepared.profile.minecraft, "1.21.1");
        assert_eq!(prepared.version.id, "neoforge-21.1.248");
        assert_eq!(first.downloaded_files, 1);
        assert!(!first.cache_hit);
        assert_eq!(
            fs::read(
                layout
                    .libraries_dir()
                    .join("org/example/library/1/library-1.jar")
            )?,
            library
        );
        assert!(layout.version_json("neoforge-21.1.248").is_file());
        assert!(runtime_marker_path(&layout, "neoforge-21.1.248").is_file());
        assert!(prepared.installer_path.starts_with(runtime.path()));

        let (_, second) = prepare_neoforge_runtime(
            &client,
            &java,
            NeoForgePreparationRequest {
                minecraft_version: "1.21.1",
                loader_version: "21.1.248",
                layout: &layout,
                distribution: &distribution,
            },
            &cancellation,
            |_, _| {},
        )
        .await?;

        assert!(second.cache_hit);
        assert_eq!(second.processors_run, 0);
        assert_eq!(second.reused_files, 1);
        assert_eq!(server.request_count("/libraries/example/library.jar"), 1);
        assert_eq!(server.request_count(INSTALLER_PATH), 3);
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_during_installer_download_leaves_no_partial_runtime()
    -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let installer =
            synthetic_installer(&server.url("unused.jar")?, 0, &sha1_bytes(&[]), 512 * 1024)?;
        let checksum = sha1_bytes(&installer);
        server.route(
            &format!("{INSTALLER_PATH}.sha1"),
            TestResponse::ok(checksum),
        );
        server.route(
            INSTALLER_PATH,
            TestResponse::slow(installer, 1024, Duration::from_millis(5)),
        );
        let runtime = tempfile::tempdir()?;
        let layout = MinecraftRuntimeLayout::new(runtime.path());
        let java = JavaRuntime {
            executable_path: PathBuf::from("unused-java"),
            major_version: 21,
        };
        let distribution = NeoForgeDistribution {
            maven_base_url: server.url("")?,
        };
        let cancellation = AcquisitionCancellation::default();
        let task_cancellation = cancellation.clone();
        let client = build_http_client()?;
        let task_layout = layout.clone();
        let task = tokio::spawn(async move {
            prepare_neoforge_runtime(
                &client,
                &java,
                NeoForgePreparationRequest {
                    minecraft_version: "1.21.1",
                    loader_version: "21.1.248",
                    layout: &task_layout,
                    distribution: &distribution,
                },
                &task_cancellation,
                |_, _| {},
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(40)).await;
        cancellation.cancel();
        let error = task.await?.unwrap_err();

        assert!(matches!(error, NeoForgeInstallerError::Cancelled));
        assert!(!layout.version_dir("neoforge-21.1.248").exists());
        assert!(
            !layout
                .root()
                .join("installers/neoforge/21.1.248/neoforge-21.1.248-installer.jar")
                .exists()
        );
        Ok(())
    }

    fn route_distribution(server: &TestServer, installer: Vec<u8>, library: &[u8]) {
        server.route(
            &format!("{INSTALLER_PATH}.sha1"),
            TestResponse::ok(sha1_bytes(&installer)),
        );
        server.route(INSTALLER_PATH, TestResponse::ok(installer));
        server.route(
            "/libraries/example/library.jar",
            TestResponse::ok(library.to_vec()),
        );
    }

    fn synthetic_installer(
        library_url: &reqwest::Url,
        library_size: u64,
        library_sha1: &str,
        filler_size: usize,
    ) -> Result<Vec<u8>, Box<dyn Error>> {
        let profile = serde_json::json!({
            "spec": 1,
            "version": "neoforge-21.1.248",
            "minecraft": "1.21.1",
            "json": "/version.json",
            "data": {
                "MC_UNPACKED": {
                    "client": "[net.minecraft:client:1.21.1-20240808.144430:unpacked]"
                }
            },
            "processors": [],
            "libraries": if library_size == 0 { Vec::<serde_json::Value>::new() } else { vec![serde_json::json!({
                "name": "org.example:library:1",
                "downloads": {"artifact": {
                    "path": "org/example/library/1/library-1.jar",
                    "url": library_url,
                    "size": library_size,
                    "sha1": library_sha1
                }}
            })] }
        });
        let version = serde_json::json!({
            "id": "neoforge-21.1.248",
            "inheritsFrom": "1.21.1",
            "mainClass": "cpw.mods.bootstraplauncher.BootstrapLauncher",
            "arguments": {"jvm": [], "game": []},
            "libraries": []
        });
        let cursor = std::io::Cursor::new(Vec::new());
        let mut archive = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        archive.start_file("install_profile.json", options)?;
        archive.write_all(&serde_json::to_vec(&profile)?)?;
        archive.start_file("version.json", options)?;
        archive.write_all(&serde_json::to_vec(&version)?)?;
        if filler_size > 0 {
            archive.start_file("filler.bin", options)?;
            archive.write_all(&vec![0x5a; filler_size])?;
        }
        Ok(archive.finish()?.into_inner())
    }
}
