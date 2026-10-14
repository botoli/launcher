use std::{
    collections::{BTreeMap, HashMap, HashSet},
    error::Error,
    fmt,
    path::{Component, Path, PathBuf},
};

use serde::Deserialize;

use crate::minecraft::{Argument, Library, MinecraftArguments, VersionMetadata};

pub const SUPPORTED_INSTALL_PROFILE_SPEC: u32 = 1;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NeoForgeInstallProfile {
    pub spec: u32,
    pub version: String,
    pub minecraft: String,
    pub json: String,
    #[serde(default)]
    pub data: BTreeMap<String, NeoForgeSidedValue>,
    #[serde(default)]
    pub processors: Vec<NeoForgeProcessor>,
    #[serde(default)]
    pub libraries: Vec<Library>,
}

impl NeoForgeInstallProfile {
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    pub fn client_processors(&self) -> impl Iterator<Item = &NeoForgeProcessor> {
        self.processors
            .iter()
            .filter(|processor| processor.applies_to_client())
    }

    pub fn client_data(&self) -> impl Iterator<Item = (&str, &str)> {
        self.data
            .iter()
            .map(|(name, value)| (name.as_str(), value.client.as_str()))
    }

    pub fn validate_pair(
        &self,
        minecraft_version: &str,
        loader_version: &str,
    ) -> Result<(), NeoForgeMetadataError> {
        let expected_version_id = neoforge_version_id(loader_version)?;
        if self.spec != SUPPORTED_INSTALL_PROFILE_SPEC {
            return Err(NeoForgeMetadataError::UnsupportedProfileSpec { found: self.spec });
        }
        if self.minecraft != minecraft_version {
            return Err(NeoForgeMetadataError::MinecraftVersionMismatch {
                requested: minecraft_version.to_owned(),
                distribution: self.minecraft.clone(),
            });
        }
        if self.version != expected_version_id {
            return Err(NeoForgeMetadataError::LoaderVersionMismatch {
                requested: loader_version.to_owned(),
                distribution: self.version.clone(),
            });
        }
        if !self.json.starts_with('/') || !is_safe_archive_entry(&self.json[1..]) {
            return Err(NeoForgeMetadataError::UnsafeArchiveEntry {
                value: self.json.clone(),
            });
        }
        Ok(())
    }

    pub fn library_index(&self) -> Result<HashMap<String, ProfileLibrary>, NeoForgeMetadataError> {
        let mut libraries = HashMap::new();
        for library in &self.libraries {
            let artifact = library
                .downloads
                .as_ref()
                .and_then(|downloads| downloads.artifact.as_ref())
                .ok_or_else(|| NeoForgeMetadataError::MissingLibraryField {
                    library: library.name.clone(),
                    field: "downloads.artifact",
                })?;
            let path = safe_relative_path(&artifact.path).ok_or_else(|| {
                NeoForgeMetadataError::UnsafeLibraryPath {
                    library: library.name.clone(),
                    path: artifact.path.clone(),
                }
            })?;
            let size = artifact
                .size
                .ok_or_else(|| NeoForgeMetadataError::MissingLibraryField {
                    library: library.name.clone(),
                    field: "size",
                })?;
            let sha1 = artifact.sha1.clone().ok_or_else(|| {
                NeoForgeMetadataError::MissingLibraryField {
                    library: library.name.clone(),
                    field: "sha1",
                }
            })?;
            let url = artifact
                .url
                .as_deref()
                .ok_or_else(|| NeoForgeMetadataError::MissingLibraryField {
                    library: library.name.clone(),
                    field: "url",
                })?
                .parse()
                .map_err(|source| NeoForgeMetadataError::InvalidLibraryUrl {
                    library: library.name.clone(),
                    source,
                })?;
            let profile_library = ProfileLibrary {
                name: library.name.clone(),
                path,
                url,
                size,
                sha1,
            };
            if libraries
                .insert(library.name.clone(), profile_library)
                .is_some()
            {
                return Err(NeoForgeMetadataError::DuplicateLibrary {
                    library: library.name.clone(),
                });
            }
        }
        Ok(libraries)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct NeoForgeSidedValue {
    pub client: String,
    #[serde(default)]
    pub server: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct NeoForgeProcessor {
    #[serde(default)]
    pub sides: Vec<String>,
    pub jar: String,
    #[serde(default)]
    pub classpath: Vec<String>,
    #[serde(default)]
    pub args: Vec<String>,
}

impl NeoForgeProcessor {
    pub fn applies_to_client(&self) -> bool {
        self.sides.is_empty() || self.sides.iter().any(|side| side == "client")
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NeoForgeVersionMetadata {
    pub id: String,
    pub main_class: String,
    pub inherits_from: String,
    #[serde(default)]
    pub arguments: MinecraftArguments,
    #[serde(default)]
    pub libraries: Vec<Library>,
    #[serde(default, rename = "type")]
    pub version_type: Option<String>,
}

impl NeoForgeVersionMetadata {
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    pub fn validate_pair(
        &self,
        minecraft_version: &str,
        loader_version: &str,
    ) -> Result<(), NeoForgeMetadataError> {
        let expected_id = neoforge_version_id(loader_version)?;
        if self.id != expected_id {
            return Err(NeoForgeMetadataError::LoaderVersionMismatch {
                requested: loader_version.to_owned(),
                distribution: self.id.clone(),
            });
        }
        if self.inherits_from != minecraft_version {
            return Err(NeoForgeMetadataError::MinecraftVersionMismatch {
                requested: minecraft_version.to_owned(),
                distribution: self.inherits_from.clone(),
            });
        }
        if self.main_class.trim().is_empty() {
            return Err(NeoForgeMetadataError::MissingMainClass);
        }
        Ok(())
    }

    pub fn merge_with_vanilla(&self, vanilla: &VersionMetadata) -> VersionMetadata {
        let child_modules: HashSet<String> = self
            .libraries
            .iter()
            .filter_map(|library| maven_module_key(&library.name))
            .collect();
        let mut libraries = self.libraries.clone();
        libraries.extend(
            vanilla
                .libraries
                .iter()
                .filter(|library| {
                    maven_module_key(&library.name)
                        .is_none_or(|module| !child_modules.contains(&module))
                })
                .cloned(),
        );

        let mut jvm = vanilla
            .arguments
            .as_ref()
            .map(|arguments| arguments.jvm.clone())
            .unwrap_or_default();
        jvm.extend(self.arguments.jvm.clone());
        let mut game = vanilla
            .arguments
            .as_ref()
            .map(|arguments| arguments.game.clone())
            .unwrap_or_else(|| legacy_game_arguments(vanilla));
        game.extend(self.arguments.game.clone());

        VersionMetadata {
            id: self.id.clone(),
            main_class: self.main_class.clone(),
            assets: vanilla.assets.clone(),
            asset_index: vanilla.asset_index.clone(),
            libraries,
            downloads: vanilla.downloads.clone(),
            arguments: Some(MinecraftArguments { jvm, game }),
            legacy_minecraft_arguments: None,
            java_version: vanilla.java_version.clone(),
            version_type: self
                .version_type
                .clone()
                .or_else(|| vanilla.version_type.clone()),
        }
    }
}

fn legacy_game_arguments(vanilla: &VersionMetadata) -> Vec<Argument> {
    vanilla
        .legacy_minecraft_arguments
        .as_ref()
        .map(|arguments| vec![Argument::Plain(arguments.clone())])
        .unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileLibrary {
    pub name: String,
    pub path: PathBuf,
    pub url: reqwest::Url,
    pub size: u64,
    pub sha1: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MavenCoordinate {
    pub descriptor: String,
    pub path: PathBuf,
}

impl MavenCoordinate {
    pub fn parse(value: &str) -> Result<Self, NeoForgeMetadataError> {
        let (coordinate, extension) = value.split_once('@').unwrap_or((value, "jar"));
        if extension.is_empty() || !extension.chars().all(is_safe_coordinate_character) {
            return Err(NeoForgeMetadataError::InvalidMavenCoordinate {
                value: value.to_owned(),
            });
        }
        let components: Vec<&str> = coordinate.split(':').collect();
        if !(components.len() == 3 || components.len() == 4)
            || components.iter().any(|component| {
                component.is_empty() || !component.chars().all(is_safe_coordinate_character)
            })
        {
            return Err(NeoForgeMetadataError::InvalidMavenCoordinate {
                value: value.to_owned(),
            });
        }
        let group = components[0];
        let artifact = components[1];
        let version = components[2];
        let classifier = components.get(3).copied();
        let mut filename = format!("{artifact}-{version}");
        if let Some(classifier) = classifier {
            filename.push('-');
            filename.push_str(classifier);
        }
        filename.push('.');
        filename.push_str(extension);
        let mut path = PathBuf::new();
        for segment in group.split('.') {
            if segment.is_empty() {
                return Err(NeoForgeMetadataError::InvalidMavenCoordinate {
                    value: value.to_owned(),
                });
            }
            path.push(segment);
        }
        path.push(artifact);
        path.push(version);
        path.push(filename);
        Ok(Self {
            descriptor: value.to_owned(),
            path,
        })
    }
}

pub fn neoforge_version_id(loader_version: &str) -> Result<String, NeoForgeMetadataError> {
    if !is_safe_component(loader_version) {
        return Err(NeoForgeMetadataError::UnsafeVersion {
            value: loader_version.to_owned(),
        });
    }
    Ok(format!("neoforge-{loader_version}"))
}

pub fn safe_relative_path(value: &str) -> Option<PathBuf> {
    let path = Path::new(value);
    if value.is_empty() || value.contains('\0') {
        return None;
    }
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => safe.push(value),
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir => return None,
        }
    }
    (!safe.as_os_str().is_empty()).then_some(safe)
}

pub fn is_safe_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | '+')
        })
}

fn is_safe_archive_entry(value: &str) -> bool {
    safe_relative_path(value).is_some()
}

fn is_safe_coordinate_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | '+')
}

fn maven_module_key(value: &str) -> Option<String> {
    let coordinate = value.split_once('@').map_or(value, |(value, _)| value);
    let mut components = coordinate.split(':');
    Some(format!("{}:{}", components.next()?, components.next()?))
}

#[derive(Debug)]
pub enum NeoForgeMetadataError {
    UnsupportedProfileSpec {
        found: u32,
    },
    MinecraftVersionMismatch {
        requested: String,
        distribution: String,
    },
    LoaderVersionMismatch {
        requested: String,
        distribution: String,
    },
    UnsafeVersion {
        value: String,
    },
    UnsafeArchiveEntry {
        value: String,
    },
    UnsafeLibraryPath {
        library: String,
        path: String,
    },
    MissingLibraryField {
        library: String,
        field: &'static str,
    },
    InvalidLibraryUrl {
        library: String,
        source: url::ParseError,
    },
    DuplicateLibrary {
        library: String,
    },
    InvalidMavenCoordinate {
        value: String,
    },
    MissingMainClass,
}

impl fmt::Display for NeoForgeMetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedProfileSpec { found } => write!(
                formatter,
                "unsupported NeoForge install profile spec {found}"
            ),
            Self::MinecraftVersionMismatch {
                requested,
                distribution,
            } => write!(
                formatter,
                "NeoForge distribution targets Minecraft {distribution}, requested {requested}"
            ),
            Self::LoaderVersionMismatch {
                requested,
                distribution,
            } => write!(
                formatter,
                "NeoForge distribution version {distribution} does not match requested loader {requested}"
            ),
            Self::UnsafeVersion { value } => write!(formatter, "unsafe NeoForge version {value}"),
            Self::UnsafeArchiveEntry { value } => {
                write!(formatter, "unsafe NeoForge installer entry {value}")
            }
            Self::UnsafeLibraryPath { library, path } => write!(
                formatter,
                "NeoForge library {library} has unsafe path {path}"
            ),
            Self::MissingLibraryField { library, field } => {
                write!(formatter, "NeoForge library {library} is missing {field}")
            }
            Self::InvalidLibraryUrl { library, .. } => {
                write!(formatter, "NeoForge library {library} has an invalid URL")
            }
            Self::DuplicateLibrary { library } => write!(
                formatter,
                "NeoForge profile contains duplicate library {library}"
            ),
            Self::InvalidMavenCoordinate { value } => {
                write!(formatter, "invalid Maven coordinate {value}")
            }
            Self::MissingMainClass => {
                formatter.write_str("NeoForge version metadata has no main class")
            }
        }
    }
}

impl Error for NeoForgeMetadataError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidLibraryUrl { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_maven_coordinates() {
        let coordinate =
            MavenCoordinate::parse("net.neoforged:neoforge:21.1.248:client@jar").unwrap();
        assert_eq!(
            coordinate.path,
            PathBuf::from("net/neoforged/neoforge/21.1.248/neoforge-21.1.248-client.jar")
        );
    }

    #[test]
    fn rejects_unsafe_coordinates_and_paths() {
        assert!(MavenCoordinate::parse("../bad:artifact:1").is_err());
        assert!(safe_relative_path("../outside.jar").is_none());
        assert!(neoforge_version_id("../bad").is_err());
    }

    #[test]
    fn parses_and_validates_1211_211248_distribution_metadata() {
        let profile = NeoForgeInstallProfile::from_json(
            r#"{
                "spec": 1,
                "version": "neoforge-21.1.248",
                "minecraft": "1.21.1",
                "json": "/version.json",
                "data": {},
                "processors": [{
                    "sides": ["client"],
                    "jar": "net.neoforged.installertools:installertools:2.1.2",
                    "classpath": [],
                    "args": ["--task", "MCP_DATA"]
                }],
                "libraries": [{
                    "name": "net.neoforged.fancymodloader:loader:4.0.43",
                    "downloads": {"artifact": {
                        "path": "net/neoforged/fancymodloader/loader/4.0.43/loader-4.0.43.jar",
                        "sha1": "0123456789012345678901234567890123456789",
                        "size": 505633,
                        "url": "https://maven.neoforged.net/releases/net/neoforged/fancymodloader/loader/4.0.43/loader-4.0.43.jar"
                    }}
                }]
            }"#,
        )
        .unwrap();
        let version = NeoForgeVersionMetadata::from_json(
            r#"{
                "id": "neoforge-21.1.248",
                "inheritsFrom": "1.21.1",
                "mainClass": "cpw.mods.bootstraplauncher.BootstrapLauncher",
                "arguments": {
                    "game": ["--fml.neoForgeVersion", "21.1.248", "--launchTarget", "forgeclient"],
                    "jvm": ["-DlibraryDirectory=${library_directory}"]
                },
                "libraries": []
            }"#,
        )
        .unwrap();

        profile.validate_pair("1.21.1", "21.1.248").unwrap();
        version.validate_pair("1.21.1", "21.1.248").unwrap();
        let libraries = profile.library_index().unwrap();
        let loader = &libraries["net.neoforged.fancymodloader:loader:4.0.43"];
        assert_eq!(loader.size, 505633);
        assert_eq!(
            loader.path,
            PathBuf::from("net/neoforged/fancymodloader/loader/4.0.43/loader-4.0.43.jar")
        );
        assert_eq!(
            version.main_class,
            "cpw.mods.bootstraplauncher.BootstrapLauncher"
        );
        assert_eq!(profile.client_processors().count(), 1);
    }
}
