use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackManifest {
    pub version: String,
    pub minecraft_version: String,
    pub files: Vec<ManifestFile>,
}

impl PackManifest {
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestFile {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trips_through_json() -> Result<(), serde_json::Error> {
        let manifest = PackManifest {
            version: "2026.08.07".to_owned(),
            minecraft_version: "1.21.1".to_owned(),
            files: vec![ManifestFile {
                path: "mods/example.jar".to_owned(),
                sha256: "a".repeat(64),
                size: 42,
            }],
        };

        let json = manifest.to_json_pretty()?;
        let decoded = PackManifest::from_json(&json)?;

        assert_eq!(decoded, manifest);
        Ok(())
    }
}
