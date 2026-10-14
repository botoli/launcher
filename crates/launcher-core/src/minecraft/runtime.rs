use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

use crate::{JavaRuntime, LaunchSpec};

use super::{
    launch::{MinecraftLaunchContext, MinecraftLaunchError, build_vanilla_launch_spec},
    metadata::VersionMetadata,
    natives::{NativeExtractionError, extract_native_libraries, resolve_native_archives},
    rules::{OperatingSystem, RuleContext, rules_allow},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MinecraftRuntimeLayout {
    root: PathBuf,
}

impl MinecraftRuntimeLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn versions_dir(&self) -> PathBuf {
        self.root.join("versions")
    }

    pub fn libraries_dir(&self) -> PathBuf {
        self.root.join("libraries")
    }

    pub fn assets_dir(&self) -> PathBuf {
        self.root.join("assets")
    }

    pub fn version_dir(&self, version_id: &str) -> PathBuf {
        self.versions_dir().join(version_id)
    }

    pub fn client_jar(&self, version_id: &str) -> PathBuf {
        self.version_dir(version_id)
            .join(format!("{version_id}.jar"))
    }

    pub fn version_json(&self, version_id: &str) -> PathBuf {
        self.version_dir(version_id)
            .join(format!("{version_id}.json"))
    }

    pub fn natives_dir(&self, version_id: &str) -> PathBuf {
        self.version_dir(version_id).join("natives")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MissingRuntimeComponent {
    ClientJar {
        path: PathBuf,
    },
    Library {
        library_name: String,
        path: PathBuf,
    },
    NativeJar {
        library_name: String,
        classifier: String,
        path: PathBuf,
    },
    AssetsRoot {
        path: PathBuf,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimeValidationResult {
    pub missing_components: Vec<MissingRuntimeComponent>,
}

impl RuntimeValidationResult {
    pub fn is_valid(&self) -> bool {
        self.missing_components.is_empty()
    }
}

#[derive(Debug)]
pub enum RuntimeValidationError {
    InvalidLibraryArtifactPath { library_name: String, path: String },
    NativeSelection(NativeExtractionError),
}

impl fmt::Display for RuntimeValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLibraryArtifactPath { library_name, path } => write!(
                formatter,
                "library {library_name} has unsafe artifact path {path}"
            ),
            Self::NativeSelection(source) => {
                write!(formatter, "failed to resolve native archives: {source}")
            }
        }
    }
}

impl Error for RuntimeValidationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NativeSelection(source) => Some(source),
            Self::InvalidLibraryArtifactPath { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum PrepareVanillaLaunchError {
    Validation(RuntimeValidationError),
    IncompleteRuntime(RuntimeValidationResult),
    NativeExtraction(NativeExtractionError),
    LaunchSpec(MinecraftLaunchError),
}

impl fmt::Display for PrepareVanillaLaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(source) => write!(formatter, "runtime validation failed: {source}"),
            Self::IncompleteRuntime(result) => write!(
                formatter,
                "vanilla runtime is missing {} required component(s)",
                result.missing_components.len()
            ),
            Self::NativeExtraction(source) => {
                write!(formatter, "native extraction failed: {source}")
            }
            Self::LaunchSpec(source) => write!(formatter, "failed to build LaunchSpec: {source}"),
        }
    }
}

impl Error for PrepareVanillaLaunchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Validation(source) => Some(source),
            Self::NativeExtraction(source) => Some(source),
            Self::LaunchSpec(source) => Some(source),
            Self::IncompleteRuntime(_) => None,
        }
    }
}

pub fn validate_vanilla_runtime(
    metadata: &VersionMetadata,
    layout: &MinecraftRuntimeLayout,
    operating_system: OperatingSystem,
) -> Result<RuntimeValidationResult, RuntimeValidationError> {
    let mut missing_components = Vec::new();
    let client_jar = layout.client_jar(&metadata.id);
    if !client_jar.is_file() {
        missing_components.push(MissingRuntimeComponent::ClientJar { path: client_jar });
    }

    let libraries_dir = layout.libraries_dir();
    let rule_context = RuleContext {
        operating_system,
        features: Default::default(),
    };
    for library in &metadata.libraries {
        if !rules_allow(&library.rules, &rule_context) {
            continue;
        }
        let Some(artifact_path) = library.artifact_path() else {
            continue;
        };
        let relative_path = safe_relative_path(artifact_path).ok_or_else(|| {
            RuntimeValidationError::InvalidLibraryArtifactPath {
                library_name: library.name.clone(),
                path: artifact_path.to_owned(),
            }
        })?;
        let path = libraries_dir.join(relative_path);
        if !path.is_file() {
            missing_components.push(MissingRuntimeComponent::Library {
                library_name: library.name.clone(),
                path,
            });
        }
    }

    let native_archives = resolve_native_archives(metadata, &libraries_dir, operating_system)
        .map_err(RuntimeValidationError::NativeSelection)?;
    for archive in native_archives {
        if !archive.archive_path.is_file() {
            missing_components.push(MissingRuntimeComponent::NativeJar {
                library_name: archive.library_name,
                classifier: archive.classifier,
                path: archive.archive_path,
            });
        }
    }

    if (metadata.asset_index.is_some() || !metadata.assets.is_empty())
        && !layout.assets_dir().is_dir()
    {
        missing_components.push(MissingRuntimeComponent::AssetsRoot {
            path: layout.assets_dir(),
        });
    }

    Ok(RuntimeValidationResult { missing_components })
}

pub fn prepare_vanilla_launch_spec(
    metadata: &VersionMetadata,
    java_runtime: &JavaRuntime,
    layout: &MinecraftRuntimeLayout,
    launch_context: &MinecraftLaunchContext,
) -> Result<LaunchSpec, PrepareVanillaLaunchError> {
    let operating_system = OperatingSystem::Windows;
    let validation = validate_vanilla_runtime(metadata, layout, operating_system)
        .map_err(PrepareVanillaLaunchError::Validation)?;
    if !validation.is_valid() {
        return Err(PrepareVanillaLaunchError::IncompleteRuntime(validation));
    }

    let libraries_dir = layout.libraries_dir();
    let natives_dir = layout.natives_dir(&metadata.id);
    extract_native_libraries(metadata, &libraries_dir, &natives_dir, operating_system)
        .map_err(PrepareVanillaLaunchError::NativeExtraction)?;

    let mut effective_context = launch_context.clone();
    effective_context.assets_root = layout.assets_dir();
    effective_context.natives_directory = natives_dir;
    build_vanilla_launch_spec(
        metadata,
        java_runtime,
        &libraries_dir,
        &layout.client_jar(&metadata.id),
        &effective_context,
    )
    .map_err(PrepareVanillaLaunchError::LaunchSpec)
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

#[cfg(test)]
mod tests {
    use std::{fs, io::Write};

    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use super::*;

    const MODERN_METADATA: &str = include_str!("fixtures/modern.json");

    #[test]
    fn runtime_layout_builds_deterministic_paths() {
        let layout = MinecraftRuntimeLayout::new(PathBuf::from(r"C:\Runtime Root"));

        assert_eq!(layout.root(), Path::new(r"C:\Runtime Root"));
        assert_eq!(
            layout.versions_dir(),
            PathBuf::from(r"C:\Runtime Root\versions")
        );
        assert_eq!(
            layout.libraries_dir(),
            PathBuf::from(r"C:\Runtime Root\libraries")
        );
        assert_eq!(
            layout.assets_dir(),
            PathBuf::from(r"C:\Runtime Root\assets")
        );
        assert_eq!(
            layout.client_jar("1.21-test"),
            PathBuf::from(r"C:\Runtime Root\versions\1.21-test\1.21-test.jar")
        );
        assert_eq!(
            layout.version_json("1.21-test"),
            PathBuf::from(r"C:\Runtime Root\versions\1.21-test\1.21-test.json")
        );
        assert_eq!(
            layout.natives_dir("1.21-test"),
            PathBuf::from(r"C:\Runtime Root\versions\1.21-test\natives")
        );
    }

    #[test]
    fn validation_reports_missing_client_jar() {
        let runtime = complete_runtime();
        fs::remove_file(runtime.layout.client_jar(&runtime.metadata.id)).unwrap();

        let result = validate(&runtime);

        assert!(
            result
                .missing_components
                .iter()
                .any(|component| matches!(component, MissingRuntimeComponent::ClientJar { .. }))
        );
    }

    #[test]
    fn validation_reports_missing_classpath_library() {
        let runtime = complete_runtime();
        let allowed_library = runtime
            .layout
            .libraries_dir()
            .join("org/example/allowed/1.0/allowed-1.0.jar");
        fs::remove_file(allowed_library).unwrap();

        let result = validate(&runtime);

        assert!(result.missing_components.iter().any(|component| matches!(
            component,
            MissingRuntimeComponent::Library { library_name, .. }
                if library_name == "org.example:allowed:1.0"
        )));
    }

    #[test]
    fn validation_reports_missing_native_jar() {
        let runtime = complete_runtime();
        fs::remove_file(&runtime.native_archive).unwrap();

        let result = validate(&runtime);

        assert!(result.missing_components.iter().any(|component| matches!(
            component,
            MissingRuntimeComponent::NativeJar { library_name, .. }
                if library_name == "org.example:native:1.0"
        )));
    }

    #[test]
    fn validation_accepts_complete_runtime() {
        let runtime = complete_runtime();

        let result = validate(&runtime);

        assert!(result.is_valid());
        assert!(result.missing_components.is_empty());
    }

    #[test]
    fn prepare_extracts_natives_and_returns_launch_spec() {
        let runtime = complete_runtime();
        let context = MinecraftLaunchContext {
            username: "Player".to_owned(),
            uuid: "uuid".to_owned(),
            access_token: "token".to_owned(),
            user_type: "msa".to_owned(),
            game_directory: runtime.layout.root().to_owned(),
            quick_play_multiplayer: None,
            assets_root: PathBuf::from("ignored-assets-path"),
            natives_directory: PathBuf::from("ignored-natives-path"),
            launcher_name: "Launcher Core".to_owned(),
            launcher_version: "0.1.0".to_owned(),
        };

        let spec = prepare_vanilla_launch_spec(
            &runtime.metadata,
            &JavaRuntime {
                executable_path: PathBuf::from("java21.exe"),
                major_version: 21,
            },
            &runtime.layout,
            &context,
        )
        .unwrap();

        let natives_dir = runtime.layout.natives_dir(&runtime.metadata.id);
        assert_eq!(spec.main_class, "net.minecraft.client.main.Main");
        assert_eq!(spec.working_directory, runtime.layout.root());
        assert!(spec.jvm_args.iter().any(|argument| {
            argument
                .to_string_lossy()
                .contains(natives_dir.to_string_lossy().as_ref())
        }));
        assert_eq!(fs::read(natives_dir.join("native.dll")).unwrap(), b"native");
    }

    struct CompleteRuntime {
        _temp: tempfile::TempDir,
        layout: MinecraftRuntimeLayout,
        metadata: VersionMetadata,
        native_archive: PathBuf,
    }

    fn complete_runtime() -> CompleteRuntime {
        let temp = tempfile::tempdir().unwrap();
        let layout = MinecraftRuntimeLayout::new(temp.path());
        let metadata = VersionMetadata::from_json(MODERN_METADATA).unwrap();
        write_file(&layout.client_jar(&metadata.id), b"client");
        write_file(
            &layout
                .libraries_dir()
                .join("org/example/allowed/1.0/allowed-1.0.jar"),
            b"library",
        );
        write_file(
            &layout
                .libraries_dir()
                .join("org/example/native/1.0/native-1.0.jar"),
            b"library",
        );
        fs::create_dir_all(layout.assets_dir()).unwrap();

        let native_archive =
            resolve_native_archives(&metadata, &layout.libraries_dir(), OperatingSystem::Windows)
                .unwrap()
                .remove(0)
                .archive_path;
        write_native_zip(&native_archive);

        CompleteRuntime {
            _temp: temp,
            layout,
            metadata,
            native_archive,
        }
    }

    fn validate(runtime: &CompleteRuntime) -> RuntimeValidationResult {
        validate_vanilla_runtime(&runtime.metadata, &runtime.layout, OperatingSystem::Windows)
            .unwrap()
    }

    fn write_file(path: &Path, contents: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn write_native_zip(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = fs::File::create(path).unwrap();
        let mut writer = ZipWriter::new(file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        writer.start_file("native.dll", options).unwrap();
        writer.write_all(b"native").unwrap();
        writer.finish().unwrap();
    }
}
