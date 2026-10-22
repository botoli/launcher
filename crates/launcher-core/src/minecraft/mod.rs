mod acquisition;
mod assets;
mod download;
mod launch;
mod launcher;
mod metadata;
mod natives;
pub mod neoforge;
mod rules;
mod runtime;
mod server_list;
mod version_catalog;

pub use acquisition::{
    AcquisitionError, AcquisitionProgress, AcquisitionSummary, VanillaAcquisitionConfig,
    VersionMetadataFetchError, acquire_vanilla_runtime, fetch_version_metadata,
    fetch_version_metadata_verified,
};
pub use assets::{
    AssetIndexFile, AssetObject, AssetPathError, asset_object_path, asset_object_url,
};
pub use download::{
    AcquisitionCancellation, RuntimeDownloadError, VerifiedDownload, download_verified_file,
};
pub use launch::{
    MinecraftLaunchContext, MinecraftLaunchError, MinecraftPlaceholderValues, PlaceholderError,
    build_vanilla_launch_spec, substitute_placeholders,
};
#[cfg(test)]
pub(crate) use launcher::launch_vanilla_with_dependencies;
#[cfg(test)]
pub(crate) use launcher::tests::{FixtureMode, SyntheticFixture};
pub use launcher::{
    VanillaLaunchCancellation, VanillaLaunchConfig, VanillaLaunchError, VanillaLaunchProgress,
    VanillaLaunchStage, identity_to_launch_context, launch_vanilla, launch_vanilla_with_identity,
    session_to_launch_context,
};
pub use metadata::{
    Argument, ArgumentValue, AssetIndex, DownloadInfo, JavaVersionRequirement, Library,
    LibraryArtifact, LibraryDownloads, LibraryExtract, MinecraftArguments, VersionDownloads,
    VersionMetadata,
};
pub use natives::{
    NativeArchive, NativeExtractionError, NativeExtractionSummary, extract_native_libraries,
    resolve_native_archives,
};
pub use rules::{OperatingSystem, Rule, RuleAction, RuleContext, RuleOs, rules_allow};
pub use runtime::{
    MinecraftRuntimeLayout, MissingRuntimeComponent, PrepareVanillaLaunchError,
    RuntimeValidationError, RuntimeValidationResult, prepare_vanilla_launch_spec,
    validate_vanilla_runtime,
};
pub use version_catalog::{
    FetchVersionByIdError, MAX_VERSION_CATALOG_SIZE, VersionCatalog, VersionCatalogEntry,
    VersionCatalogError, VersionCatalogLatest, VersionResolveError, fetch_version_by_id,
    fetch_version_catalog, resolve_version,
};
