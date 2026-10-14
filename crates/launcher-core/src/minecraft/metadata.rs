use std::collections::BTreeMap;

use serde::Deserialize;

use super::rules::{OperatingSystem, Rule};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VersionMetadata {
    pub id: String,
    pub main_class: String,
    pub assets: String,
    #[serde(default)]
    pub asset_index: Option<AssetIndex>,
    #[serde(default)]
    pub libraries: Vec<Library>,
    #[serde(default)]
    pub downloads: VersionDownloads,
    #[serde(default)]
    pub arguments: Option<MinecraftArguments>,
    #[serde(default, rename = "minecraftArguments")]
    pub legacy_minecraft_arguments: Option<String>,
    #[serde(default)]
    pub java_version: Option<JavaVersionRequirement>,
    #[serde(default, rename = "type")]
    pub version_type: Option<String>,
}

impl VersionMetadata {
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AssetIndex {
    pub id: String,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub total_size: Option<u64>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct VersionDownloads {
    #[serde(default)]
    pub client: Option<DownloadInfo>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct DownloadInfo {
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct JavaVersionRequirement {
    #[serde(default)]
    pub component: Option<String>,
    pub major_version: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Library {
    pub name: String,
    #[serde(default)]
    pub downloads: Option<LibraryDownloads>,
    #[serde(default)]
    pub natives: BTreeMap<String, String>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub extract: Option<LibraryExtract>,
}

impl Library {
    pub fn artifact_path(&self) -> Option<&str> {
        self.downloads
            .as_ref()?
            .artifact
            .as_ref()
            .map(|artifact| artifact.path.as_str())
    }

    pub fn native_classifier(&self, operating_system: OperatingSystem) -> Option<String> {
        let template = self.natives.get(operating_system.metadata_name())?;
        Some(template.replace("${arch}", architecture_bits()))
    }

    pub fn native_artifact(
        &self,
        operating_system: OperatingSystem,
    ) -> Option<(String, &LibraryArtifact)> {
        let classifier = self.native_classifier(operating_system)?;
        let artifact = self.downloads.as_ref()?.classifiers.get(&classifier)?;
        Some((classifier, artifact))
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct LibraryExtract {
    #[serde(default)]
    pub exclude: Vec<String>,
}

fn architecture_bits() -> &'static str {
    if cfg!(target_pointer_width = "64") {
        "64"
    } else {
        "32"
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct LibraryDownloads {
    #[serde(default)]
    pub artifact: Option<LibraryArtifact>,
    #[serde(default)]
    pub classifiers: BTreeMap<String, LibraryArtifact>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct LibraryArtifact {
    pub path: String,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct MinecraftArguments {
    #[serde(default)]
    pub jvm: Vec<Argument>,
    #[serde(default)]
    pub game: Vec<Argument>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Argument {
    Plain(String),
    Conditional {
        rules: Vec<Rule>,
        value: ArgumentValue,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ArgumentValue {
    String(String),
    Array(Vec<String>),
}

impl ArgumentValue {
    pub fn values(&self) -> &[String] {
        match self {
            Self::String(value) => std::slice::from_ref(value),
            Self::Array(values) => values,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODERN_METADATA: &str = include_str!("fixtures/modern.json");
    const LEGACY_METADATA: &str = include_str!("fixtures/legacy.json");

    #[test]
    fn parses_modern_metadata_and_ignores_unknown_fields() {
        let metadata = VersionMetadata::from_json(MODERN_METADATA).unwrap();

        assert_eq!(metadata.id, "1.21-test");
        assert_eq!(metadata.main_class, "net.minecraft.client.main.Main");
        assert_eq!(metadata.asset_index.as_ref().unwrap().id, "test-assets");
        assert_eq!(metadata.libraries.len(), 3);
        assert_eq!(metadata.java_version.unwrap().major_version, 21);
        assert!(metadata.arguments.is_some());
    }

    #[test]
    fn parses_legacy_minecraft_arguments() {
        let metadata = VersionMetadata::from_json(LEGACY_METADATA).unwrap();

        assert_eq!(
            metadata.legacy_minecraft_arguments.as_deref(),
            Some("--username ${auth_player_name} --message \"value with spaces\"")
        );
        assert!(metadata.arguments.is_none());
    }

    #[test]
    fn selects_windows_native_classifier_and_artifact_path() {
        let metadata = VersionMetadata::from_json(MODERN_METADATA).unwrap();
        let native_library = &metadata.libraries[2];
        let expected_classifier = if cfg!(target_pointer_width = "64") {
            "natives-windows-64"
        } else {
            "natives-windows-32"
        };

        let (classifier, artifact) = native_library
            .native_artifact(OperatingSystem::Windows)
            .unwrap();

        assert_eq!(classifier, expected_classifier);
        assert_eq!(
            artifact.path,
            format!("org/example/native/1.0/native-1.0-{expected_classifier}.jar")
        );
    }
}
