use std::{ffi::OsString, sync::Arc};

use crate::{
    ClientConfig, JavaError, LaunchSpec, MinecraftLaunchIdentity, ProcessError, ProcessHandle,
    find_java, launch_process,
    minecraft::{
        AcquisitionCancellation, AcquisitionError, MinecraftRuntimeLayout, OperatingSystem,
        VanillaAcquisitionConfig, VanillaLaunchConfig, VersionCatalogError,
        VersionMetadataFetchError, VersionResolveError, acquire_vanilla_runtime,
        build_vanilla_launch_spec, extract_native_libraries, fetch_version_catalog,
        fetch_version_metadata_verified, identity_to_launch_context, resolve_version,
    },
};

use super::{
    installer::{
        NeoForgeDistribution, NeoForgeInstallerError, NeoForgePreparationRequest,
        prepare_neoforge_runtime,
    },
    metadata::{NeoForgeMetadataError, NeoForgeVersionMetadata},
};

#[derive(Clone, Debug)]
pub struct NeoForgeLaunchConfig {
    pub minecraft_version: String,
    pub loader_version: String,
    pub java_major: u32,
    pub min_memory: u32,
    pub max_memory: u32,
    pub jvm_args: Vec<String>,
    pub vanilla: VanillaLaunchConfig,
    pub distribution: NeoForgeDistribution,
}

impl NeoForgeLaunchConfig {
    pub fn from_client_config(vanilla: VanillaLaunchConfig, config: &ClientConfig) -> Self {
        Self {
            minecraft_version: config.version.clone(),
            loader_version: config.loader_version.clone(),
            java_major: config.java_version,
            min_memory: config.min_memory,
            max_memory: config.max_memory,
            jvm_args: config.jvm_args.clone(),
            vanilla,
            distribution: NeoForgeDistribution::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NeoForgeLaunchStage {
    ResolvingVersion,
    CheckingJava,
    AcquiringVanilla,
    AcquiringNeoForge,
    PreparingRuntime,
    Launching,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NeoForgeLaunchProgress {
    pub stage: NeoForgeLaunchStage,
    pub progress: Option<f32>,
}

#[derive(Clone, Debug, Default)]
pub struct NeoForgeLaunchCancellation {
    acquisition: AcquisitionCancellation,
}

impl NeoForgeLaunchCancellation {
    pub fn cancel(&self) {
        self.acquisition.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.acquisition.is_cancelled()
    }

    pub async fn cancelled(&self) {
        self.acquisition.cancelled().await;
    }
}

pub async fn launch_neoforge_with_identity<F>(
    client: &reqwest::Client,
    identity: &MinecraftLaunchIdentity,
    config: &NeoForgeLaunchConfig,
    cancellation: &NeoForgeLaunchCancellation,
    on_progress: F,
) -> Result<ProcessHandle, NeoForgeLaunchError>
where
    F: Fn(NeoForgeLaunchProgress) + Send + Sync + 'static,
{
    let on_progress: Arc<dyn Fn(NeoForgeLaunchProgress) + Send + Sync> = Arc::new(on_progress);
    ensure_not_cancelled(cancellation)?;
    report(&on_progress, NeoForgeLaunchStage::ResolvingVersion, None);
    tracing::info!(
        minecraft_version = %config.minecraft_version,
        loader_version = %config.loader_version,
        "selected NeoForge loader"
    );
    let catalog = cancellable(
        cancellation,
        fetch_version_catalog(client, &config.vanilla.catalog_url),
    )
    .await?
    .map_err(NeoForgeLaunchError::VersionCatalog)?;
    let entry = resolve_version(&catalog, &config.minecraft_version)
        .map_err(NeoForgeLaunchError::VersionResolve)?;
    let metadata_url = reqwest::Url::parse(&entry.url).map_err(|source| {
        NeoForgeLaunchError::InvalidMetadataUrl {
            version_id: entry.id.clone(),
            source,
        }
    })?;
    let vanilla_metadata = cancellable(
        cancellation,
        fetch_version_metadata_verified(client, &metadata_url, &entry.sha1),
    )
    .await?
    .map_err(NeoForgeLaunchError::VersionMetadata)?;

    report(&on_progress, NeoForgeLaunchStage::CheckingJava, None);
    let java = cancellable(cancellation, find_java(config.java_major))
        .await?
        .map_err(NeoForgeLaunchError::Java)?;

    let layout = MinecraftRuntimeLayout::new(&config.vanilla.runtime_root);
    report(
        &on_progress,
        NeoForgeLaunchStage::AcquiringVanilla,
        Some(0.0),
    );
    let vanilla_progress = Arc::clone(&on_progress);
    acquire_vanilla_runtime(
        client,
        &vanilla_metadata,
        &layout,
        OperatingSystem::Windows,
        &VanillaAcquisitionConfig::new(config.vanilla.assets_base_url.clone()),
        &cancellation.acquisition,
        move |progress| {
            report(
                &vanilla_progress,
                NeoForgeLaunchStage::AcquiringVanilla,
                Some(progress.fraction()),
            );
        },
    )
    .await
    .map_err(map_acquisition_error)?;

    report(
        &on_progress,
        NeoForgeLaunchStage::AcquiringNeoForge,
        Some(0.0),
    );
    let neoforge_progress = Arc::clone(&on_progress);
    let (prepared, _) = prepare_neoforge_runtime(
        client,
        &java,
        NeoForgePreparationRequest {
            minecraft_version: &config.minecraft_version,
            loader_version: &config.loader_version,
            layout: &layout,
            distribution: &config.distribution,
        },
        &cancellation.acquisition,
        move |completed, total| {
            let fraction = if total == 0 {
                1.0
            } else {
                (completed as f64 / total as f64).clamp(0.0, 1.0) as f32
            };
            report(
                &neoforge_progress,
                NeoForgeLaunchStage::AcquiringNeoForge,
                Some(fraction),
            );
        },
    )
    .await
    .map_err(map_installer_error)?;

    report(&on_progress, NeoForgeLaunchStage::PreparingRuntime, None);
    ensure_not_cancelled(cancellation)?;
    let mut vanilla_config = config.vanilla.clone();
    vanilla_config.version_id = config.minecraft_version.clone();
    vanilla_config.java_major_override = Some(config.java_major);
    let context = identity_to_launch_context(identity, &vanilla_config);
    let spec = build_neoforge_launch_spec(
        &vanilla_metadata,
        &prepared.version,
        &java,
        &layout,
        &context,
        NeoForgeJvmOptions {
            min_memory: config.min_memory,
            max_memory: config.max_memory,
            backend_jvm_args: &config.jvm_args,
        },
    )?;

    ensure_not_cancelled(cancellation)?;
    report(&on_progress, NeoForgeLaunchStage::Launching, None);
    tracing::info!(
        main_class = %spec.main_class,
        loader_version = %config.loader_version,
        "launching NeoForge"
    );
    if let Some(gate) = &config.vanilla.telegram_auth {
        let user = cancellable(cancellation, gate.verify())
            .await?
            .map_err(NeoForgeLaunchError::Authentication)?;
        if user.nickname != identity.username() {
            return Err(NeoForgeLaunchError::Authentication(
                crate::auth::telegram::TelegramAuthError::Unauthorized,
            ));
        }
    }
    ensure_not_cancelled(cancellation)?;
    let mut process = launch_process(spec).map_err(NeoForgeLaunchError::Process)?;
    if cancellation.is_cancelled() {
        process.cancel();
        while process.next_event().await.is_some() {}
        let _ = process.wait().await;
        return Err(NeoForgeLaunchError::Cancelled);
    }
    Ok(process)
}

#[derive(Clone, Copy, Debug)]
pub struct NeoForgeJvmOptions<'a> {
    pub min_memory: u32,
    pub max_memory: u32,
    pub backend_jvm_args: &'a [String],
}

pub fn build_neoforge_launch_spec(
    vanilla: &crate::minecraft::VersionMetadata,
    neoforge: &NeoForgeVersionMetadata,
    java: &crate::JavaRuntime,
    layout: &MinecraftRuntimeLayout,
    context: &crate::minecraft::MinecraftLaunchContext,
    jvm: NeoForgeJvmOptions<'_>,
) -> Result<LaunchSpec, NeoForgeLaunchError> {
    let NeoForgeJvmOptions {
        min_memory,
        max_memory,
        backend_jvm_args,
    } = jvm;
    validate_memory(min_memory, max_memory)?;
    validate_heap_arguments(
        "backend jvmArgs",
        backend_jvm_args.iter().map(String::as_str),
    )?;
    let mut merged = neoforge.merge_with_vanilla(vanilla);
    let natives_directory = layout.natives_dir(&merged.id);
    extract_native_libraries(
        &merged,
        &layout.libraries_dir(),
        &natives_directory,
        OperatingSystem::Windows,
    )
    .map_err(NeoForgeLaunchError::NativeExtraction)?;
    let mut effective_context = context.clone();
    effective_context.assets_root = layout.assets_dir();
    effective_context.natives_directory = natives_directory;
    merged.id.clone_from(&vanilla.id);
    let mut spec = build_vanilla_launch_spec(
        &merged,
        java,
        &layout.libraries_dir(),
        &layout.client_jar(&vanilla.id),
        &effective_context,
    )
    .map_err(NeoForgeLaunchError::LaunchSpec)?;
    validate_heap_arguments(
        "distribution metadata",
        spec.jvm_args
            .iter()
            .map(|argument| argument.to_string_lossy())
            .map(|argument| argument.into_owned())
            .collect::<Vec<_>>()
            .iter()
            .map(String::as_str),
    )?;
    let mut jvm_args = vec![
        OsString::from(format!("-Xms{min_memory}M")),
        OsString::from(format!("-Xmx{max_memory}M")),
    ];
    jvm_args.extend(backend_jvm_args.iter().map(OsString::from));
    jvm_args.append(&mut spec.jvm_args);
    spec.jvm_args = jvm_args;
    Ok(spec)
}

fn validate_memory(min_memory: u32, max_memory: u32) -> Result<(), NeoForgeLaunchError> {
    if min_memory == 0 || max_memory == 0 || min_memory > max_memory {
        return Err(NeoForgeLaunchError::InvalidMemory {
            min_memory,
            max_memory,
        });
    }
    Ok(())
}

fn validate_heap_arguments<'a>(
    argument_source: &'static str,
    arguments: impl IntoIterator<Item = &'a str>,
) -> Result<(), NeoForgeLaunchError> {
    for argument in arguments {
        if is_heap_argument(argument) {
            return Err(NeoForgeLaunchError::ConflictingMemoryArgument {
                argument_source,
                argument: argument.to_owned(),
            });
        }
    }
    Ok(())
}

fn is_heap_argument(argument: &str) -> bool {
    argument.starts_with("-Xms")
        || argument.starts_with("-Xmx")
        || argument.starts_with("-XX:InitialHeapSize=")
        || argument.starts_with("-XX:MaxHeapSize=")
}

fn report(
    callback: &Arc<dyn Fn(NeoForgeLaunchProgress) + Send + Sync>,
    stage: NeoForgeLaunchStage,
    progress: Option<f32>,
) {
    callback(NeoForgeLaunchProgress { stage, progress });
}

fn ensure_not_cancelled(
    cancellation: &NeoForgeLaunchCancellation,
) -> Result<(), NeoForgeLaunchError> {
    if cancellation.is_cancelled() {
        Err(NeoForgeLaunchError::Cancelled)
    } else {
        Ok(())
    }
}

async fn cancellable<T, E, Fut>(
    cancellation: &NeoForgeLaunchCancellation,
    future: Fut,
) -> Result<Result<T, E>, NeoForgeLaunchError>
where
    Fut: std::future::Future<Output = Result<T, E>>,
{
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(NeoForgeLaunchError::Cancelled),
        result = future => Ok(result),
    }
}

fn map_acquisition_error(error: AcquisitionError) -> NeoForgeLaunchError {
    if matches!(error, AcquisitionError::Cancelled) {
        NeoForgeLaunchError::Cancelled
    } else {
        NeoForgeLaunchError::Acquisition(error)
    }
}

fn map_installer_error(error: NeoForgeInstallerError) -> NeoForgeLaunchError {
    if matches!(error, NeoForgeInstallerError::Cancelled) {
        NeoForgeLaunchError::Cancelled
    } else {
        NeoForgeLaunchError::Installer(error)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NeoForgeLaunchError {
    #[error("{0}")]
    Authentication(crate::auth::telegram::TelegramAuthError),
    #[error("NeoForge launch was cancelled")]
    Cancelled,
    #[error("failed to fetch Minecraft version catalog: {0}")]
    VersionCatalog(VersionCatalogError),
    #[error("failed to resolve Minecraft version: {0}")]
    VersionResolve(VersionResolveError),
    #[error("version {version_id} has an invalid metadata URL")]
    InvalidMetadataUrl {
        version_id: String,
        #[source]
        source: url::ParseError,
    },
    #[error("failed to fetch Minecraft version metadata: {0}")]
    VersionMetadata(VersionMetadataFetchError),
    #[error("Java discovery failed: {0}")]
    Java(JavaError),
    #[error("vanilla runtime acquisition failed: {0}")]
    Acquisition(AcquisitionError),
    #[error("NeoForge runtime preparation failed: {0}")]
    Installer(NeoForgeInstallerError),
    #[error("NeoForge metadata is invalid: {0}")]
    Metadata(#[from] NeoForgeMetadataError),
    #[error("failed to extract Minecraft native libraries: {0}")]
    NativeExtraction(crate::minecraft::NativeExtractionError),
    #[error("failed to build NeoForge LaunchSpec: {0}")]
    LaunchSpec(crate::minecraft::MinecraftLaunchError),
    #[error("invalid RAM range: minMemory={min_memory}, maxMemory={max_memory}")]
    InvalidMemory { min_memory: u32, max_memory: u32 },
    #[error("{argument_source} contains conflicting memory argument {argument}")]
    ConflictingMemoryArgument {
        argument_source: &'static str,
        argument: String,
    },
    #[error("NeoForge process launch failed: {0}")]
    Process(ProcessError),
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsStr, path::PathBuf};

    use super::*;
    use crate::minecraft::{MinecraftLaunchContext, VersionMetadata};

    #[test]
    fn builds_1211_211248_launch_spec_with_identity_memory_and_direct_connect() {
        let vanilla = vanilla_metadata();
        let neoforge = neoforge_metadata();
        let root = PathBuf::from(r"C:\Launcher Test\.launcher-data");
        let layout = MinecraftRuntimeLayout::new(&root);
        let context = MinecraftLaunchContext {
            username: "OfflinePlayer".to_owned(),
            uuid: "0123456789abcdef0123456789abcdef".to_owned(),
            access_token: "0".to_owned(),
            user_type: "legacy".to_owned(),
            game_directory: root.clone(),
            quick_play_multiplayer: Some("server.example:25565".to_owned()),
            assets_root: layout.assets_dir(),
            natives_directory: layout.natives_dir("1.21.1"),
            launcher_name: "TestLauncher".to_owned(),
            launcher_version: "1".to_owned(),
        };
        let java = crate::JavaRuntime {
            executable_path: PathBuf::from("java.exe"),
            major_version: 21,
        };

        let spec = build_neoforge_launch_spec(
            &vanilla,
            &neoforge,
            &java,
            &layout,
            &context,
            NeoForgeJvmOptions {
                min_memory: 2048,
                max_memory: 6144,
                backend_jvm_args: &["-Dfile.encoding=UTF-8".to_owned()],
            },
        )
        .unwrap();

        assert_eq!(
            spec.main_class,
            "cpw.mods.bootstraplauncher.BootstrapLauncher"
        );
        assert_eq!(spec.working_directory, root);
        assert_eq!(
            &spec.jvm_args[..3],
            [
                OsStr::new("-Xms2048M"),
                OsStr::new("-Xmx6144M"),
                OsStr::new("-Dfile.encoding=UTF-8"),
            ]
        );
        assert_eq!(
            spec.jvm_args
                .iter()
                .filter(|argument| is_heap_argument(&argument.to_string_lossy()))
                .count(),
            2
        );
        assert!(spec.jvm_args.iter().any(|argument| {
            argument
                .to_string_lossy()
                .contains("bootstraplauncher-2.0.2.jar")
        }));
        assert!(
            spec.jvm_args
                .iter()
                .any(|argument| argument == "-DignoreList=client-extra,1.21.1.jar")
        );
        assert!(contains_pair(&spec.game_args, "--version", "1.21.1"));
        assert!(contains_pair(
            &spec.game_args,
            "--username",
            "OfflinePlayer"
        ));
        assert!(contains_pair(
            &spec.game_args,
            "--uuid",
            "0123456789abcdef0123456789abcdef"
        ));
        assert!(contains_pair(&spec.game_args, "--accessToken", "0"));
        assert!(contains_pair(&spec.game_args, "--clientId", ""));
        assert!(contains_pair(&spec.game_args, "--xuid", ""));
        assert!(contains_pair(&spec.game_args, "--userType", "legacy"));
        assert!(contains_pair(
            &spec.game_args,
            "--quickPlayMultiplayer",
            "server.example:25565"
        ));
        assert!(contains_pair(
            &spec.game_args,
            "--fml.neoForgeVersion",
            "21.1.248"
        ));
        assert!(contains_pair(
            &spec.game_args,
            "--launchTarget",
            "forgeclient"
        ));
        assert!(
            spec.classpath
                .iter()
                .any(|path| path.ends_with("bootstraplauncher-2.0.2.jar"))
        );
        assert!(
            spec.classpath
                .iter()
                .any(|path| path.ends_with("vanilla-library-1.jar"))
        );
        assert_eq!(spec.classpath.last(), Some(&layout.client_jar("1.21.1")));
    }

    #[test]
    fn rejects_invalid_memory_range_and_conflicting_backend_heap_args() {
        let vanilla = vanilla_metadata();
        let neoforge = neoforge_metadata();
        let layout = MinecraftRuntimeLayout::new("runtime");
        let context = MinecraftLaunchContext {
            username: "Player".to_owned(),
            uuid: "uuid".to_owned(),
            access_token: "0".to_owned(),
            user_type: "legacy".to_owned(),
            game_directory: PathBuf::from("runtime"),
            quick_play_multiplayer: None,
            assets_root: layout.assets_dir(),
            natives_directory: layout.natives_dir("1.21.1"),
            launcher_name: "test".to_owned(),
            launcher_version: "1".to_owned(),
        };
        let java = crate::JavaRuntime {
            executable_path: PathBuf::from("java"),
            major_version: 21,
        };

        assert!(matches!(
            build_neoforge_launch_spec(
                &vanilla,
                &neoforge,
                &java,
                &layout,
                &context,
                NeoForgeJvmOptions {
                    min_memory: 4096,
                    max_memory: 2048,
                    backend_jvm_args: &[],
                }
            ),
            Err(NeoForgeLaunchError::InvalidMemory { .. })
        ));
        assert!(matches!(
            build_neoforge_launch_spec(
                &vanilla,
                &neoforge,
                &java,
                &layout,
                &context,
                NeoForgeJvmOptions {
                    min_memory: 2048,
                    max_memory: 4096,
                    backend_jvm_args: &["-Xmx8G".to_owned()],
                }
            ),
            Err(NeoForgeLaunchError::ConflictingMemoryArgument { .. })
        ));
        let mut neoforge = neoforge;
        neoforge
            .arguments
            .jvm
            .push(crate::minecraft::Argument::Plain(
                "-XX:MaxHeapSize=123".to_owned(),
            ));
        assert!(matches!(
            build_neoforge_launch_spec(
                &vanilla,
                &neoforge,
                &java,
                &layout,
                &context,
                NeoForgeJvmOptions {
                    min_memory: 2048,
                    max_memory: 6144,
                    backend_jvm_args: &[],
                }
            ),
            Err(NeoForgeLaunchError::ConflictingMemoryArgument {
                argument_source: "distribution metadata",
                ..
            })
        ));
    }

    fn vanilla_metadata() -> VersionMetadata {
        VersionMetadata::from_json(
            r#"{
                "id": "1.21.1",
                "mainClass": "net.minecraft.client.main.Main",
                "assets": "17",
                "assetIndex": {"id": "17"},
                "type": "release",
                "javaVersion": {"majorVersion": 21},
                "downloads": {"client": {}},
                "libraries": [{
                    "name": "org.example:vanilla-library:1",
                    "downloads": {"artifact": {"path": "org/example/vanilla-library/1/vanilla-library-1.jar"}}
                }],
                "arguments": {
                    "jvm": [
                        "-Djava.library.path=${natives_directory}",
                        "-DignoreList=client-extra,${version_name}.jar"
                    ],
                    "game": [
                        "--version", "${version_name}",
                        "--username", "${auth_player_name}",
                        "--uuid", "${auth_uuid}",
                        "--accessToken", "${auth_access_token}",
                        "--clientId", "${clientid}",
                        "--xuid", "${auth_xuid}",
                        "--userType", "${user_type}",
                        {"rules": [{"action": "allow", "features": {"is_quick_play_multiplayer": true}}],
                         "value": ["--quickPlayMultiplayer", "${quickPlayMultiplayer}"]}
                    ]
                }
            }"#,
        )
        .unwrap()
    }

    fn neoforge_metadata() -> NeoForgeVersionMetadata {
        NeoForgeVersionMetadata::from_json(
            r#"{
                "id": "neoforge-21.1.248",
                "inheritsFrom": "1.21.1",
                "mainClass": "cpw.mods.bootstraplauncher.BootstrapLauncher",
                "arguments": {
                    "jvm": [
                        "-DlibraryDirectory=${library_directory}",
                        "-p",
                        "${library_directory}/cpw/mods/bootstraplauncher/2.0.2/bootstraplauncher-2.0.2.jar${classpath_separator}${library_directory}/cpw/mods/securejarhandler/3.0.8/securejarhandler-3.0.8.jar"
                    ],
                    "game": [
                        "--fml.neoForgeVersion", "21.1.248",
                        "--fml.fmlVersion", "4.0.43",
                        "--fml.mcVersion", "1.21.1",
                        "--fml.neoFormVersion", "20240808.144430",
                        "--launchTarget", "forgeclient"
                    ]
                },
                "libraries": [{
                    "name": "cpw.mods:bootstraplauncher:2.0.2",
                    "downloads": {"artifact": {"path": "cpw/mods/bootstraplauncher/2.0.2/bootstraplauncher-2.0.2.jar"}}
                }]
            }"#,
        )
        .unwrap()
    }

    fn contains_pair(arguments: &[OsString], name: &str, value: &str) -> bool {
        arguments
            .windows(2)
            .any(|pair| pair == [OsStr::new(name), OsStr::new(value)])
    }
}
