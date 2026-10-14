use std::{
    env::JoinPathsError,
    error::Error,
    ffi::{OsStr, OsString},
    fmt,
    path::{Component, Path, PathBuf},
};

use crate::{JavaRuntime, LaunchSpec};

use super::{
    metadata::{Argument, VersionMetadata},
    rules::{RuleContext, rules_allow},
};

#[derive(Clone, PartialEq, Eq)]
pub struct MinecraftLaunchContext {
    pub username: String,
    pub uuid: String,
    pub access_token: String,
    pub user_type: String,
    pub game_directory: PathBuf,
    pub quick_play_multiplayer: Option<String>,
    pub assets_root: PathBuf,
    pub natives_directory: PathBuf,
    pub launcher_name: String,
    pub launcher_version: String,
}

impl fmt::Debug for MinecraftLaunchContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MinecraftLaunchContext")
            .field("username", &self.username)
            .field("uuid", &self.uuid)
            .field("access_token", &"<redacted>")
            .field("user_type", &self.user_type)
            .field("game_directory", &self.game_directory)
            .field("quick_play_multiplayer", &self.quick_play_multiplayer)
            .field("assets_root", &self.assets_root)
            .field("natives_directory", &self.natives_directory)
            .field("launcher_name", &self.launcher_name)
            .field("launcher_version", &self.launcher_version)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MinecraftPlaceholderValues {
    pub auth_player_name: OsString,
    pub version_name: OsString,
    pub game_directory: OsString,
    pub quick_play_multiplayer: Option<OsString>,
    pub assets_root: OsString,
    pub assets_index_name: Option<OsString>,
    pub auth_uuid: OsString,
    pub auth_access_token: OsString,
    pub client_id: OsString,
    pub auth_xuid: OsString,
    pub user_type: OsString,
    pub version_type: Option<OsString>,
    pub natives_directory: OsString,
    pub launcher_name: OsString,
    pub launcher_version: OsString,
    pub classpath: OsString,
    pub library_directory: OsString,
    pub classpath_separator: OsString,
}

impl MinecraftPlaceholderValues {
    fn get(&self, name: &str) -> Result<&OsStr, PlaceholderError> {
        let value = match name {
            "auth_player_name" => Some(self.auth_player_name.as_os_str()),
            "version_name" => Some(self.version_name.as_os_str()),
            "game_directory" => Some(self.game_directory.as_os_str()),
            "quickPlayMultiplayer" => self.quick_play_multiplayer.as_deref(),
            "assets_root" => Some(self.assets_root.as_os_str()),
            "assets_index_name" => self.assets_index_name.as_deref(),
            "auth_uuid" => Some(self.auth_uuid.as_os_str()),
            "auth_access_token" => Some(self.auth_access_token.as_os_str()),
            "clientid" => Some(self.client_id.as_os_str()),
            "auth_xuid" => Some(self.auth_xuid.as_os_str()),
            "user_type" => Some(self.user_type.as_os_str()),
            "version_type" => self.version_type.as_deref(),
            "natives_directory" => Some(self.natives_directory.as_os_str()),
            "launcher_name" => Some(self.launcher_name.as_os_str()),
            "launcher_version" => Some(self.launcher_version.as_os_str()),
            "classpath" => Some(self.classpath.as_os_str()),
            "library_directory" => Some(self.library_directory.as_os_str()),
            "classpath_separator" => Some(self.classpath_separator.as_os_str()),
            _ => {
                return Err(PlaceholderError::UnknownPlaceholder {
                    name: name.to_owned(),
                });
            }
        };

        value.ok_or_else(|| PlaceholderError::MissingValue {
            name: name.to_owned(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaceholderError {
    UnknownPlaceholder { name: String },
    MissingValue { name: String },
    UnclosedPlaceholder,
}

impl fmt::Display for PlaceholderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPlaceholder { name } => {
                write!(formatter, "unknown placeholder ${{{name}}}")
            }
            Self::MissingValue { name } => {
                write!(formatter, "placeholder ${{{name}}} has no value")
            }
            Self::UnclosedPlaceholder => {
                formatter.write_str("placeholder is missing a closing brace")
            }
        }
    }
}

impl Error for PlaceholderError {}

#[derive(Debug)]
pub enum MinecraftLaunchError {
    JavaVersionMismatch {
        required_major: u32,
        runtime_major: u32,
    },
    InvalidLibraryPath {
        library_name: String,
        path: String,
    },
    InvalidClasspath {
        source: JoinPathsError,
    },
    Placeholder(PlaceholderError),
    InvalidLegacyArguments {
        reason: &'static str,
    },
}

impl fmt::Display for MinecraftLaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::JavaVersionMismatch {
                required_major,
                runtime_major,
            } => write!(
                formatter,
                "Minecraft requires Java {required_major}, but Java {runtime_major} was provided"
            ),
            Self::InvalidLibraryPath { library_name, path } => {
                write!(
                    formatter,
                    "library {library_name} has unsafe artifact path {path}"
                )
            }
            Self::InvalidClasspath { .. } => {
                formatter.write_str("Minecraft classpath contains a path-list separator")
            }
            Self::Placeholder(source) => write!(formatter, "failed to expand argument: {source}"),
            Self::InvalidLegacyArguments { reason } => {
                write!(formatter, "invalid legacy minecraftArguments: {reason}")
            }
        }
    }
}

impl Error for MinecraftLaunchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidClasspath { source } => Some(source),
            Self::Placeholder(source) => Some(source),
            Self::JavaVersionMismatch { .. }
            | Self::InvalidLibraryPath { .. }
            | Self::InvalidLegacyArguments { .. } => None,
        }
    }
}

impl From<PlaceholderError> for MinecraftLaunchError {
    fn from(error: PlaceholderError) -> Self {
        Self::Placeholder(error)
    }
}

pub fn substitute_placeholders(
    input: &str,
    values: &MinecraftPlaceholderValues,
) -> Result<OsString, PlaceholderError> {
    let mut output = OsString::new();
    let mut cursor = 0;

    while let Some(relative_start) = input[cursor..].find("${") {
        let start = cursor + relative_start;
        output.push(&input[cursor..start]);

        let name_start = start + 2;
        let relative_end = input[name_start..]
            .find('}')
            .ok_or(PlaceholderError::UnclosedPlaceholder)?;
        let end = name_start + relative_end;
        output.push(values.get(&input[name_start..end])?);
        cursor = end + 1;
    }

    output.push(&input[cursor..]);
    Ok(output)
}

pub fn build_vanilla_launch_spec(
    metadata: &VersionMetadata,
    java_runtime: &JavaRuntime,
    libraries_root: &Path,
    client_jar: &Path,
    context: &MinecraftLaunchContext,
) -> Result<LaunchSpec, MinecraftLaunchError> {
    if let Some(java_version) = &metadata.java_version
        && java_version.major_version != java_runtime.major_version
    {
        return Err(MinecraftLaunchError::JavaVersionMismatch {
            required_major: java_version.major_version,
            runtime_major: java_runtime.major_version,
        });
    }

    let mut rule_context = RuleContext::windows();
    rule_context.features.insert(
        "is_quick_play_multiplayer".to_owned(),
        context.quick_play_multiplayer.is_some(),
    );
    let mut classpath = collect_classpath(metadata, libraries_root, &rule_context)?;
    classpath.push(client_jar.to_owned());
    let joined_classpath = std::env::join_paths(&classpath)
        .map_err(|source| MinecraftLaunchError::InvalidClasspath { source })?;
    let placeholder_values = MinecraftPlaceholderValues {
        auth_player_name: OsString::from(&context.username),
        version_name: OsString::from(&metadata.id),
        game_directory: context.game_directory.as_os_str().to_owned(),
        quick_play_multiplayer: context.quick_play_multiplayer.as_ref().map(OsString::from),
        assets_root: context.assets_root.as_os_str().to_owned(),
        assets_index_name: metadata
            .asset_index
            .as_ref()
            .map(|asset_index| OsString::from(&asset_index.id)),
        auth_uuid: OsString::from(&context.uuid),
        auth_access_token: OsString::from(&context.access_token),
        client_id: OsString::new(),
        auth_xuid: OsString::new(),
        user_type: OsString::from(&context.user_type),
        version_type: metadata.version_type.as_ref().map(OsString::from),
        natives_directory: context.natives_directory.as_os_str().to_owned(),
        launcher_name: OsString::from(&context.launcher_name),
        launcher_version: OsString::from(&context.launcher_version),
        classpath: joined_classpath,
        library_directory: libraries_root.as_os_str().to_owned(),
        classpath_separator: OsString::from(if cfg!(windows) { ";" } else { ":" }),
    };

    let (jvm_arguments, game_arguments) = if let Some(arguments) = &metadata.arguments {
        (
            resolve_modern_arguments(&arguments.jvm, &rule_context),
            resolve_modern_arguments(&arguments.game, &rule_context),
        )
    } else {
        (
            Vec::new(),
            metadata
                .legacy_minecraft_arguments
                .as_deref()
                .map(split_legacy_arguments)
                .transpose()?
                .unwrap_or_default(),
        )
    };
    let jvm_arguments = remove_structural_classpath_arguments(jvm_arguments);
    let jvm_args = expand_arguments(jvm_arguments, &placeholder_values)?;
    let game_args = expand_arguments(game_arguments, &placeholder_values)?;

    Ok(LaunchSpec {
        java_executable: java_runtime.executable_path.clone(),
        jvm_args,
        classpath,
        main_class: metadata.main_class.clone(),
        game_args,
        working_directory: context.game_directory.clone(),
    })
}

fn collect_classpath(
    metadata: &VersionMetadata,
    libraries_root: &Path,
    rule_context: &RuleContext,
) -> Result<Vec<PathBuf>, MinecraftLaunchError> {
    metadata
        .libraries
        .iter()
        .filter(|library| rules_allow(&library.rules, rule_context))
        .filter_map(|library| {
            library
                .artifact_path()
                .map(|path| (library.name.as_str(), path))
        })
        .map(|(library_name, path)| {
            if is_safe_relative_path(path) {
                Ok(libraries_root.join(path))
            } else {
                Err(MinecraftLaunchError::InvalidLibraryPath {
                    library_name: library_name.to_owned(),
                    path: path.to_owned(),
                })
            }
        })
        .collect()
}

fn is_safe_relative_path(path: &str) -> bool {
    let mut has_component = false;
    for component in Path::new(path).components() {
        match component {
            Component::Normal(_) => has_component = true,
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir => return false,
        }
    }
    has_component
}

fn resolve_modern_arguments(arguments: &[Argument], rule_context: &RuleContext) -> Vec<String> {
    let mut resolved = Vec::new();
    for argument in arguments {
        match argument {
            Argument::Plain(value) => resolved.push(value.clone()),
            Argument::Conditional { rules, value } if rules_allow(rules, rule_context) => {
                resolved.extend(value.values().iter().cloned());
            }
            Argument::Conditional { .. } => {}
        }
    }
    resolved
}

fn remove_structural_classpath_arguments(arguments: Vec<String>) -> Vec<String> {
    let mut filtered = Vec::with_capacity(arguments.len());
    let mut arguments = arguments.into_iter().peekable();
    while let Some(argument) = arguments.next() {
        if matches!(argument.as_str(), "-cp" | "-classpath" | "--class-path")
            && arguments.peek().is_some_and(|next| next == "${classpath}")
        {
            arguments.next();
        } else {
            filtered.push(argument);
        }
    }
    filtered
}

fn expand_arguments(
    arguments: Vec<String>,
    values: &MinecraftPlaceholderValues,
) -> Result<Vec<OsString>, PlaceholderError> {
    arguments
        .iter()
        .map(|argument| substitute_placeholders(argument, values))
        .collect()
}

fn split_legacy_arguments(input: &str) -> Result<Vec<String>, MinecraftLaunchError> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut token_started = false;

    for character in input.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            token_started = true;
            continue;
        }

        match character {
            '\\' => {
                escaped = true;
                token_started = true;
            }
            '"' | '\'' if quote.is_none() => {
                quote = Some(character);
                token_started = true;
            }
            character if quote == Some(character) => quote = None,
            character if character.is_whitespace() && quote.is_none() => {
                if token_started {
                    arguments.push(std::mem::take(&mut current));
                    token_started = false;
                }
            }
            character => {
                current.push(character);
                token_started = true;
            }
        }
    }

    if quote.is_some() {
        return Err(MinecraftLaunchError::InvalidLegacyArguments {
            reason: "unterminated quote",
        });
    }
    if escaped {
        current.push('\\');
    }
    if token_started {
        arguments.push(current);
    }
    Ok(arguments)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    const MODERN_METADATA: &str = include_str!("fixtures/modern.json");
    const LEGACY_METADATA: &str = include_str!("fixtures/legacy.json");

    #[test]
    fn builds_modern_jvm_arguments_and_removes_duplicate_classpath_pair() {
        let spec = modern_spec().unwrap();

        assert_eq!(
            spec.jvm_args,
            [
                OsString::from("-Xmx2G"),
                OsString::from(r"-Djava.library.path=C:\Minecraft Test\natives"),
                OsString::from("-Dlauncher=Launcher Core/0.1.0"),
            ]
        );
        assert!(!spec.jvm_args.iter().any(|argument| argument == "-cp"));
    }

    #[test]
    fn builds_modern_game_arguments_and_expands_array_values() {
        let spec = modern_spec().unwrap();

        assert!(contains_pair(&spec.game_args, "--username", "Test Player"));
        assert!(contains_pair(&spec.game_args, "--version", "1.21-test"));
        assert!(contains_pair(
            &spec.game_args,
            "--gameDir",
            r"C:\Minecraft Test"
        ));
    }

    #[test]
    fn unsupported_feature_argument_is_not_applied() {
        let spec = modern_spec().unwrap();

        assert!(!spec.game_args.iter().any(|argument| argument == "--width"));
        assert!(!spec.game_args.iter().any(|argument| argument == "1920"));
    }

    #[test]
    fn applies_quick_play_multiplayer_from_launch_context() {
        let mut metadata = modern_metadata();
        metadata.arguments.as_mut().unwrap().game.push(
            serde_json::from_value(serde_json::json!({
                "rules": [{
                    "action": "allow",
                    "features": {"is_quick_play_multiplayer": true}
                }],
                "value": ["--quickPlayMultiplayer", "${quickPlayMultiplayer}"]
            }))
            .unwrap(),
        );
        let mut context = launch_context();
        context.quick_play_multiplayer = Some("server.example:25565".to_owned());

        let spec = build_vanilla_launch_spec(
            &metadata,
            &JavaRuntime {
                executable_path: PathBuf::from("java21.exe"),
                major_version: 21,
            },
            Path::new(r"C:\Minecraft Test\libraries"),
            Path::new(r"C:\Minecraft Test\versions\1.21-test\client.jar"),
            &context,
        )
        .unwrap();

        assert!(contains_pair(
            &spec.game_args,
            "--quickPlayMultiplayer",
            "server.example:25565"
        ));
    }

    #[test]
    fn substitutes_multiple_known_placeholders_without_chained_replace() {
        let metadata = modern_metadata();
        let context = launch_context();
        let classpath = std::env::join_paths([PathBuf::from("library.jar")]).unwrap();
        let values = placeholder_values(&metadata, &context, classpath);

        let expanded = substitute_placeholders(
            "${launcher_name}:${auth_player_name}:${natives_directory}",
            &values,
        )
        .unwrap();

        assert_eq!(
            expanded,
            OsString::from(r"Launcher Core:Test Player:C:\Minecraft Test\natives")
        );
    }

    #[test]
    fn unknown_placeholder_returns_typed_error() {
        let metadata = modern_metadata();
        let context = launch_context();
        let values = placeholder_values(&metadata, &context, OsString::new());

        let error = substitute_placeholders("${unknown_value}", &values).unwrap_err();

        assert_eq!(
            error,
            PlaceholderError::UnknownPlaceholder {
                name: "unknown_value".to_owned()
            }
        );
    }

    #[test]
    fn classpath_contains_allowed_libraries_and_client_jar() {
        let spec = modern_spec().unwrap();

        assert_eq!(
            spec.classpath,
            [
                PathBuf::from(
                    r"C:\Minecraft Test\libraries\org/example/allowed/1.0/allowed-1.0.jar"
                ),
                PathBuf::from(r"C:\Minecraft Test\libraries\org/example/native/1.0/native-1.0.jar"),
                PathBuf::from(r"C:\Minecraft Test\versions\1.21-test\client.jar"),
            ]
        );
    }

    #[test]
    fn classpath_paths_with_spaces_have_no_shell_quotes() {
        let spec = modern_spec().unwrap();
        let joined = std::env::join_paths(&spec.classpath).unwrap();
        let joined = joined.to_string_lossy();

        assert!(joined.contains("Minecraft Test"));
        assert!(!joined.contains('"'));
        assert!(!joined.contains('\''));
    }

    #[test]
    fn transfers_main_class_and_working_directory() {
        let spec = modern_spec().unwrap();

        assert_eq!(spec.main_class, "net.minecraft.client.main.Main");
        assert_eq!(spec.working_directory, PathBuf::from(r"C:\Minecraft Test"));
    }

    #[test]
    fn rejects_java_version_mismatch() {
        let metadata = modern_metadata();
        let runtime = JavaRuntime {
            executable_path: PathBuf::from("java17.exe"),
            major_version: 17,
        };

        let error = build_vanilla_launch_spec(
            &metadata,
            &runtime,
            Path::new(r"C:\Minecraft Test\libraries"),
            Path::new(r"C:\Minecraft Test\client.jar"),
            &launch_context(),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            MinecraftLaunchError::JavaVersionMismatch {
                required_major: 21,
                runtime_major: 17
            }
        ));
    }

    #[test]
    fn builds_legacy_game_arguments_with_quoted_value() {
        let metadata = VersionMetadata::from_json(LEGACY_METADATA).unwrap();
        let runtime = JavaRuntime {
            executable_path: PathBuf::from("java.exe"),
            major_version: 8,
        };
        let spec = build_vanilla_launch_spec(
            &metadata,
            &runtime,
            Path::new("libraries"),
            Path::new("legacy-client.jar"),
            &launch_context(),
        )
        .unwrap();

        assert!(contains_pair(&spec.game_args, "--username", "Test Player"));
        assert!(contains_pair(
            &spec.game_args,
            "--message",
            "value with spaces"
        ));
    }

    #[test]
    fn full_synthetic_vanilla_metadata_builds_launch_spec() {
        let spec = modern_spec().unwrap();

        assert_eq!(spec.java_executable, PathBuf::from("java21.exe"));
        assert_eq!(spec.classpath.len(), 3);
        assert_eq!(spec.jvm_args.len(), 3);
        assert!(spec.game_args.len() > 10);
        assert_eq!(spec.main_class, "net.minecraft.client.main.Main");
        assert_eq!(spec.working_directory, PathBuf::from(r"C:\Minecraft Test"));
    }

    fn modern_spec() -> Result<LaunchSpec, MinecraftLaunchError> {
        build_vanilla_launch_spec(
            &modern_metadata(),
            &JavaRuntime {
                executable_path: PathBuf::from("java21.exe"),
                major_version: 21,
            },
            Path::new(r"C:\Minecraft Test\libraries"),
            Path::new(r"C:\Minecraft Test\versions\1.21-test\client.jar"),
            &launch_context(),
        )
    }

    fn modern_metadata() -> VersionMetadata {
        VersionMetadata::from_json(MODERN_METADATA).unwrap()
    }

    fn launch_context() -> MinecraftLaunchContext {
        MinecraftLaunchContext {
            username: "Test Player".to_owned(),
            uuid: "00000000-0000-0000-0000-000000000000".to_owned(),
            access_token: "test-access-token".to_owned(),
            user_type: "msa".to_owned(),
            game_directory: PathBuf::from(r"C:\Minecraft Test"),
            quick_play_multiplayer: None,
            assets_root: PathBuf::from(r"C:\Minecraft Test\assets"),
            natives_directory: PathBuf::from(r"C:\Minecraft Test\natives"),
            launcher_name: "Launcher Core".to_owned(),
            launcher_version: "0.1.0".to_owned(),
        }
    }

    fn placeholder_values(
        metadata: &VersionMetadata,
        context: &MinecraftLaunchContext,
        classpath: OsString,
    ) -> MinecraftPlaceholderValues {
        MinecraftPlaceholderValues {
            auth_player_name: OsString::from(&context.username),
            version_name: OsString::from(&metadata.id),
            game_directory: context.game_directory.as_os_str().to_owned(),
            quick_play_multiplayer: context.quick_play_multiplayer.as_ref().map(OsString::from),
            assets_root: context.assets_root.as_os_str().to_owned(),
            assets_index_name: metadata
                .asset_index
                .as_ref()
                .map(|index| OsString::from(&index.id)),
            auth_uuid: OsString::from(&context.uuid),
            auth_access_token: OsString::from(&context.access_token),
            client_id: OsString::new(),
            auth_xuid: OsString::new(),
            user_type: OsString::from(&context.user_type),
            version_type: metadata.version_type.as_ref().map(OsString::from),
            natives_directory: context.natives_directory.as_os_str().to_owned(),
            launcher_name: OsString::from(&context.launcher_name),
            launcher_version: OsString::from(&context.launcher_version),
            classpath,
            library_directory: OsString::from(r"C:\Minecraft Test\libraries"),
            classpath_separator: OsString::from(if cfg!(windows) { ";" } else { ":" }),
        }
    }

    fn contains_pair(arguments: &[OsString], name: &str, value: &str) -> bool {
        arguments
            .windows(2)
            .any(|pair| pair == [OsStr::new(name), OsStr::new(value)])
    }
}
