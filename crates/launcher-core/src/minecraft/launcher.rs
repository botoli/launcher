use std::{error::Error, fmt, future::Future, path::PathBuf, sync::Arc};

use crate::{
    JavaError, JavaRuntime, LaunchSpec, MinecraftLaunchIdentity, MinecraftSession, ProcessError,
    ProcessHandle, find_java, launch_process,
};

use super::{
    acquisition::{
        AcquisitionError, VanillaAcquisitionConfig, VersionMetadataFetchError,
        acquire_vanilla_runtime,
    },
    download::AcquisitionCancellation,
    launch::MinecraftLaunchContext,
    metadata::VersionMetadata,
    rules::OperatingSystem,
    runtime::{MinecraftRuntimeLayout, PrepareVanillaLaunchError, prepare_vanilla_launch_spec},
    version_catalog::{
        VersionCatalogError, VersionResolveError, fetch_version_catalog, resolve_version,
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VanillaLaunchConfig {
    pub telegram_auth: Option<crate::auth::telegram::TelegramAuthGate>,
    pub heap_memory: Option<crate::HeapMemory>,
    pub jvm_args: Vec<String>,
    pub version_id: String,
    pub java_major_override: Option<u32>,
    pub server_address: Option<String>,
    pub catalog_url: reqwest::Url,
    pub assets_base_url: reqwest::Url,
    pub runtime_root: PathBuf,
    pub game_directory: PathBuf,
    pub launcher_name: String,
    pub launcher_version: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VanillaLaunchStage {
    ResolvingVersion,
    CheckingJava,
    AcquiringRuntime,
    PreparingRuntime,
    Launching,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VanillaLaunchProgress {
    pub stage: VanillaLaunchStage,
    pub progress: Option<f32>,
}

#[derive(Clone, Debug, Default)]
pub struct VanillaLaunchCancellation {
    acquisition: AcquisitionCancellation,
}

impl VanillaLaunchCancellation {
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

#[derive(Debug)]
pub enum VanillaLaunchError {
    Authentication(crate::auth::telegram::TelegramAuthError),
    Cancelled,
    Memory(crate::InvalidHeapMemory),
    VersionCatalog(VersionCatalogError),
    VersionResolve(VersionResolveError),
    InvalidMetadataUrl {
        version_id: String,
        value: String,
        source: url::ParseError,
    },
    VersionMetadata(VersionMetadataFetchError),
    MissingJavaVersion {
        version_id: String,
    },
    Java(JavaError),
    Acquisition(AcquisitionError),
    RuntimePreparation(PrepareVanillaLaunchError),
    RuntimePreparationTask {
        source: tokio::task::JoinError,
    },
    Process(ProcessError),
}

impl fmt::Display for VanillaLaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authentication(source) => write!(formatter, "{source}"),
            Self::Cancelled => formatter.write_str("vanilla launch was cancelled"),
            Self::Memory(source) => write!(formatter, "invalid heap settings: {source}"),
            Self::VersionCatalog(source) => {
                write!(
                    formatter,
                    "failed to fetch Minecraft version catalog: {source}"
                )
            }
            Self::VersionResolve(source) => {
                write!(formatter, "failed to resolve Minecraft version: {source}")
            }
            Self::InvalidMetadataUrl { version_id, .. } => {
                write!(
                    formatter,
                    "version {version_id} has an invalid metadata URL"
                )
            }
            Self::VersionMetadata(source) => {
                write!(
                    formatter,
                    "failed to fetch Minecraft version metadata: {source}"
                )
            }
            Self::MissingJavaVersion { version_id } => write!(
                formatter,
                "version {version_id} does not declare a required Java major version"
            ),
            Self::Java(source) => write!(formatter, "Java discovery failed: {source}"),
            Self::Acquisition(source) => {
                write!(formatter, "vanilla runtime acquisition failed: {source}")
            }
            Self::RuntimePreparation(source) => {
                write!(formatter, "vanilla runtime preparation failed: {source}")
            }
            Self::RuntimePreparationTask { source } => {
                write!(formatter, "runtime preparation task failed: {source}")
            }
            Self::Process(source) => write!(formatter, "Minecraft process launch failed: {source}"),
        }
    }
}

impl Error for VanillaLaunchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Authentication(source) => Some(source),
            Self::Memory(source) => Some(source),
            Self::VersionCatalog(source) => Some(source),
            Self::VersionResolve(source) => Some(source),
            Self::InvalidMetadataUrl { source, .. } => Some(source),
            Self::VersionMetadata(source) => Some(source),
            Self::Java(source) => Some(source),
            Self::Acquisition(source) => Some(source),
            Self::RuntimePreparation(source) => Some(source),
            Self::RuntimePreparationTask { source } => Some(source),
            Self::Process(source) => Some(source),
            Self::Cancelled | Self::MissingJavaVersion { .. } => None,
        }
    }
}

pub fn session_to_launch_context(
    session: &MinecraftSession,
    config: &VanillaLaunchConfig,
) -> MinecraftLaunchContext {
    identity_to_launch_context(&MinecraftLaunchIdentity::from(session), config)
}

pub fn identity_to_launch_context(
    identity: &MinecraftLaunchIdentity,
    config: &VanillaLaunchConfig,
) -> MinecraftLaunchContext {
    let layout = MinecraftRuntimeLayout::new(&config.runtime_root);
    MinecraftLaunchContext {
        username: identity.username().to_owned(),
        uuid: identity.uuid().to_owned(),
        access_token: identity.access_token().to_owned(),
        user_type: identity.user_type().to_owned(),
        game_directory: config.game_directory.clone(),
        quick_play_multiplayer: config.server_address.clone(),
        assets_root: layout.assets_dir(),
        natives_directory: layout.natives_dir(&config.version_id),
        launcher_name: config.launcher_name.clone(),
        launcher_version: config.launcher_version.clone(),
    }
}

pub async fn launch_vanilla<F>(
    client: &reqwest::Client,
    session: &MinecraftSession,
    config: &VanillaLaunchConfig,
    cancellation: &VanillaLaunchCancellation,
    on_progress: F,
) -> Result<ProcessHandle, VanillaLaunchError>
where
    F: Fn(VanillaLaunchProgress) + Send + Sync + 'static,
{
    let identity = MinecraftLaunchIdentity::from(session);
    launch_vanilla_with_identity(client, &identity, config, cancellation, on_progress).await
}

pub async fn launch_vanilla_with_identity<F>(
    client: &reqwest::Client,
    identity: &MinecraftLaunchIdentity,
    config: &VanillaLaunchConfig,
    cancellation: &VanillaLaunchCancellation,
    on_progress: F,
) -> Result<ProcessHandle, VanillaLaunchError>
where
    F: Fn(VanillaLaunchProgress) + Send + Sync + 'static,
{
    launch_vanilla_with_dependencies(
        client,
        identity,
        config,
        cancellation,
        on_progress,
        find_java,
        launch_process,
    )
    .await
}

pub(crate) async fn launch_vanilla_with_dependencies<F, R, RFut, P>(
    client: &reqwest::Client,
    identity: &MinecraftLaunchIdentity,
    config: &VanillaLaunchConfig,
    cancellation: &VanillaLaunchCancellation,
    on_progress: F,
    java_resolver: R,
    process_launcher: P,
) -> Result<ProcessHandle, VanillaLaunchError>
where
    F: Fn(VanillaLaunchProgress) + Send + Sync + 'static,
    R: Fn(u32) -> RFut,
    RFut: Future<Output = Result<JavaRuntime, JavaError>>,
    P: FnOnce(LaunchSpec) -> Result<ProcessHandle, ProcessError>,
{
    let on_progress: Arc<dyn Fn(VanillaLaunchProgress) + Send + Sync> = Arc::new(on_progress);
    if let Some(heap) = config.heap_memory {
        crate::HeapMemory::new(heap.min_memory_mib, heap.max_memory_mib)
            .map_err(VanillaLaunchError::Memory)?;
    }
    tracing::info!("resolving Minecraft");
    report(&on_progress, VanillaLaunchStage::ResolvingVersion, None);
    ensure_not_cancelled(cancellation)?;
    let catalog = cancellable(
        cancellation,
        fetch_version_catalog(client, &config.catalog_url),
    )
    .await?
    .map_err(VanillaLaunchError::VersionCatalog)?;
    let entry = resolve_version(&catalog, &config.version_id)
        .map_err(VanillaLaunchError::VersionResolve)?;
    let metadata_url = reqwest::Url::parse(&entry.url).map_err(|source| {
        VanillaLaunchError::InvalidMetadataUrl {
            version_id: entry.id.clone(),
            value: entry.url.clone(),
            source,
        }
    })?;
    let metadata = cancellable(
        cancellation,
        super::acquisition::fetch_version_metadata_verified(client, &metadata_url, &entry.sha1),
    )
    .await?
    .map_err(VanillaLaunchError::VersionMetadata)?;

    let required_java = config
        .java_major_override
        .map(Ok)
        .unwrap_or_else(|| required_java_major(&metadata))?;
    report(&on_progress, VanillaLaunchStage::CheckingJava, None);
    ensure_not_cancelled(cancellation)?;
    let java_runtime = cancellable(cancellation, java_resolver(required_java))
        .await?
        .map_err(VanillaLaunchError::Java)?;

    tracing::info!("acquiring runtime");
    report(
        &on_progress,
        VanillaLaunchStage::AcquiringRuntime,
        Some(0.0),
    );
    ensure_not_cancelled(cancellation)?;
    let layout = MinecraftRuntimeLayout::new(&config.runtime_root);
    let acquisition_config = VanillaAcquisitionConfig::new(config.assets_base_url.clone());
    let acquisition_progress = Arc::clone(&on_progress);
    acquire_vanilla_runtime(
        client,
        &metadata,
        &layout,
        OperatingSystem::Windows,
        &acquisition_config,
        &cancellation.acquisition,
        move |progress| {
            report(
                &acquisition_progress,
                VanillaLaunchStage::AcquiringRuntime,
                Some(progress.fraction()),
            );
        },
    )
    .await
    .map_err(map_acquisition_error)?;

    tracing::info!("preparing runtime");
    report(&on_progress, VanillaLaunchStage::PreparingRuntime, None);
    ensure_not_cancelled(cancellation)?;
    let context = identity_to_launch_context(identity, config);
    let preparation_metadata = metadata.clone();
    let preparation_java = java_runtime.clone();
    let preparation_layout = layout.clone();
    let preparation_context = context.clone();
    let mut launch_spec = tokio::task::spawn_blocking(move || {
        prepare_vanilla_launch_spec(
            &preparation_metadata,
            &preparation_java,
            &preparation_layout,
            &preparation_context,
        )
    })
    .await
    .map_err(|source| VanillaLaunchError::RuntimePreparationTask { source })?
    .map_err(VanillaLaunchError::RuntimePreparation)?;
    launch_spec
        .jvm_args
        .extend(config.jvm_args.iter().map(std::ffi::OsString::from));
    if let Some(heap) = config.heap_memory {
        heap.replace_arguments(&mut launch_spec.jvm_args)
            .map_err(VanillaLaunchError::Memory)?;
    }
    ensure_not_cancelled(cancellation)?;

    tracing::info!("launching");
    report(&on_progress, VanillaLaunchStage::Launching, None);
    ensure_not_cancelled(cancellation)?;
    if let Some(gate) = &config.telegram_auth {
        let user = cancellable(cancellation, gate.verify())
            .await?
            .map_err(VanillaLaunchError::Authentication)?;
        if user.nickname != identity.username() {
            return Err(VanillaLaunchError::Authentication(
                crate::auth::telegram::TelegramAuthError::Unauthorized,
            ));
        }
    }
    ensure_not_cancelled(cancellation)?;
    process_launcher(launch_spec).map_err(VanillaLaunchError::Process)
}

fn required_java_major(metadata: &VersionMetadata) -> Result<u32, VanillaLaunchError> {
    metadata
        .java_version
        .as_ref()
        .map(|requirement| requirement.major_version)
        .ok_or_else(|| VanillaLaunchError::MissingJavaVersion {
            version_id: metadata.id.clone(),
        })
}

fn report(
    callback: &Arc<dyn Fn(VanillaLaunchProgress) + Send + Sync>,
    stage: VanillaLaunchStage,
    progress: Option<f32>,
) {
    callback(VanillaLaunchProgress { stage, progress });
}

fn ensure_not_cancelled(
    cancellation: &VanillaLaunchCancellation,
) -> Result<(), VanillaLaunchError> {
    if cancellation.is_cancelled() {
        Err(VanillaLaunchError::Cancelled)
    } else {
        Ok(())
    }
}

async fn cancellable<T, E, Fut>(
    cancellation: &VanillaLaunchCancellation,
    future: Fut,
) -> Result<Result<T, E>, VanillaLaunchError>
where
    Fut: Future<Output = Result<T, E>>,
{
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(VanillaLaunchError::Cancelled),
        result = future => Ok(result),
    }
}

fn map_acquisition_error(source: AcquisitionError) -> VanillaLaunchError {
    if matches!(source, AcquisitionError::Cancelled) {
        VanillaLaunchError::Cancelled
    } else {
        VanillaLaunchError::Acquisition(source)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        error::Error,
        ffi::OsStr,
        io::{Cursor, Write},
        path::Path,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use serde_json::json;
    use sha1::{Digest, Sha1};
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use super::*;
    use crate::{
        OfflineProfile, build_http_client,
        http::test_support::{TestResponse, TestServer},
        process::test_support::{collect_output, spawn_test_helper},
    };

    const VERSION_ID: &str = "1.21-orchestrated";
    const JAVA_MAJOR: u32 = 21;

    #[test]
    fn converts_session_to_launch_context() {
        let temp = tempfile::tempdir().unwrap();
        let config = config_for_paths(
            temp.path().join("runtime"),
            temp.path().join("Game Directory"),
        );
        let session = session();

        let context = session_to_launch_context(&session, &config);

        assert_eq!(context.username, "TestPlayer");
        assert_eq!(context.uuid, "0123456789abcdef0123456789abcdef");
        assert_eq!(context.access_token, "minecraft-access-secret");
        assert_eq!(context.user_type, "msa");
        assert_eq!(context.game_directory, config.game_directory);
        assert_eq!(
            context.assets_root,
            MinecraftRuntimeLayout::new(&config.runtime_root).assets_dir()
        );
        assert_eq!(
            context.natives_directory,
            MinecraftRuntimeLayout::new(&config.runtime_root).natives_dir(VERSION_ID)
        );
        assert!(!format!("{context:?}").contains("minecraft-access-secret"));
    }

    #[test]
    fn converts_offline_profile_identity_to_launch_context() {
        let temp = tempfile::tempdir().unwrap();
        let config = config_for_paths(
            temp.path().join("runtime"),
            temp.path().join("Game Directory"),
        );
        let profile = OfflineProfile::new("TestPlayer").unwrap();
        let identity = MinecraftLaunchIdentity::from(&profile);

        let context = identity_to_launch_context(&identity, &config);

        assert_eq!(context.username, profile.username());
        assert_eq!(context.uuid, profile.uuid());
        assert_eq!(context.access_token, "0");
        assert_eq!(context.user_type, "legacy");
        assert_eq!(context.game_directory, config.game_directory);
        assert_eq!(
            context.assets_root,
            MinecraftRuntimeLayout::new(&config.runtime_root).assets_dir()
        );
        assert_eq!(
            context.natives_directory,
            MinecraftRuntimeLayout::new(&config.runtime_root).natives_dir(VERSION_ID)
        );
    }

    #[tokio::test]
    async fn full_synthetic_flow_builds_spec_spawns_helper_and_exposes_output()
    -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let temp = tempfile::tempdir()?;
        let fixture = SyntheticFixture::new(&server, temp.path(), FixtureMode::Valid)?;
        let java_calls = Arc::new(Mutex::new(Vec::new()));
        let captured_spec = Arc::new(Mutex::new(None));
        let progress = Arc::new(Mutex::new(Vec::new()));
        let process = launch_with_fixture(
            &fixture,
            &java_calls,
            &captured_spec,
            &progress,
            VanillaLaunchCancellation::default(),
        )
        .await?;

        assert_eq!(*java_calls.lock().unwrap(), vec![JAVA_MAJOR]);
        assert_eq!(server.request_count("/versions/wrong.json"), 0);
        for path in fixture.runtime_routes() {
            assert_eq!(server.request_count(path), 1, "request count for {path}");
        }
        assert_eq!(
            std::fs::read(
                MinecraftRuntimeLayout::new(&fixture.config.runtime_root)
                    .natives_dir(VERSION_ID)
                    .join("native.dll")
            )?,
            b"synthetic native"
        );
        let spec = captured_spec.lock().unwrap().clone().unwrap();
        assert_eq!(spec.main_class, "net.minecraft.client.main.Main");
        assert_eq!(spec.working_directory, fixture.config.game_directory);
        assert_eq!(spec.java_executable, std::env::current_exe()?);
        assert!(spec.classpath.len() >= 3);
        assert!(contains_pair(&spec.game_args, "--username", "TestPlayer"));
        assert!(contains_pair(
            &spec.game_args,
            "--uuid",
            fixture.identity.uuid()
        ));
        assert!(contains_pair(&spec.game_args, "--accessToken", "0"));
        assert!(contains_pair(&spec.game_args, "--userType", "legacy"));

        let stages = distinct_stages(&progress.lock().unwrap());
        assert_eq!(
            stages,
            vec![
                VanillaLaunchStage::ResolvingVersion,
                VanillaLaunchStage::CheckingJava,
                VanillaLaunchStage::AcquiringRuntime,
                VanillaLaunchStage::PreparingRuntime,
                VanillaLaunchStage::Launching,
            ]
        );
        assert!(progress.lock().unwrap().iter().any(|update| {
            update.stage == VanillaLaunchStage::AcquiringRuntime && update.progress == Some(1.0)
        }));

        let (stdout, _stderr, exit) = collect_output(process).await;
        assert!(String::from_utf8(stdout)?.contains("portable stdout marker"));
        assert!(exit.status.success());
        assert!(!exit.cancelled);
        Ok(())
    }

    #[tokio::test]
    async fn valid_runtime_is_reused_without_runtime_downloads() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let temp = tempfile::tempdir()?;
        let fixture = SyntheticFixture::new(&server, temp.path(), FixtureMode::Valid)?;

        for _ in 0..2 {
            let process = launch_with_fixture(
                &fixture,
                &Arc::new(Mutex::new(Vec::new())),
                &Arc::new(Mutex::new(None)),
                &Arc::new(Mutex::new(Vec::new())),
                VanillaLaunchCancellation::default(),
            )
            .await?;
            let (_, _, exit) = collect_output(process).await;
            assert!(exit.status.success());
        }

        assert_eq!(server.request_count("/catalog.json"), 2);
        assert_eq!(server.request_count("/versions/selected.json"), 2);
        for path in fixture.runtime_routes() {
            assert_eq!(server.request_count(path), 1, "re-download for {path}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn selected_memory_reaches_the_vanilla_process_command() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let temp = tempfile::tempdir()?;
        let mut fixture = SyntheticFixture::new(&server, temp.path(), FixtureMode::Valid)?;
        fixture.config.heap_memory = Some(crate::HeapMemory::new(1536, 6143)?);
        fixture.config.jvm_args = vec![
            "-Xmx2G".to_owned(),
            "-XX:InitialHeapSize=123".to_owned(),
            "-Duser.option=kept".to_owned(),
        ];
        let captured_spec = Arc::new(Mutex::new(None));
        let process = launch_with_fixture(
            &fixture,
            &Arc::new(Mutex::new(Vec::new())),
            &captured_spec,
            &Arc::new(Mutex::new(Vec::new())),
            VanillaLaunchCancellation::default(),
        )
        .await?;
        let spec = captured_spec.lock().unwrap().clone().unwrap();
        assert_eq!(
            spec.jvm_args
                .iter()
                .filter(|arg| crate::launch_options::is_heap_argument(arg))
                .count(),
            2
        );
        assert!(spec.jvm_args.contains(&"-Xms1536M".into()));
        assert!(spec.jvm_args.contains(&"-Xmx6143M".into()));
        assert!(spec.jvm_args.contains(&"-Duser.option=kept".into()));
        assert!(collect_output(process).await.2.status.success());
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_before_download_makes_no_request() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let temp = tempfile::tempdir()?;
        let fixture = SyntheticFixture::new(&server, temp.path(), FixtureMode::Valid)?;
        let cancellation = VanillaLaunchCancellation::default();
        cancellation.cancel();

        let error = launch_without_process(&fixture, cancellation, |_| {})
            .await
            .unwrap_err();

        assert!(matches!(error, VanillaLaunchError::Cancelled));
        assert_eq!(server.request_count("/catalog.json"), 0);
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_during_acquisition_stops_before_preparation() -> Result<(), Box<dyn Error>>
    {
        let server = TestServer::start().await?;
        let temp = tempfile::tempdir()?;
        let fixture = SyntheticFixture::new(&server, temp.path(), FixtureMode::SlowClient)?;
        let cancellation = VanillaLaunchCancellation::default();
        let callback_cancellation = cancellation.clone();

        let error = launch_without_process(&fixture, cancellation, move |update| {
            if update.stage == VanillaLaunchStage::AcquiringRuntime
                && update.progress.is_some_and(|progress| progress > 0.0)
            {
                callback_cancellation.cancel();
            }
        })
        .await
        .unwrap_err();

        assert!(matches!(error, VanillaLaunchError::Cancelled));
        assert!(server.request_count("/runtime/client.jar") >= 1);
        assert!(
            !MinecraftRuntimeLayout::new(&fixture.config.runtime_root)
                .natives_dir(VERSION_ID)
                .exists()
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_at_launching_stage_prevents_spawn() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let temp = tempfile::tempdir()?;
        let fixture = SyntheticFixture::new(&server, temp.path(), FixtureMode::Valid)?;
        let cancellation = VanillaLaunchCancellation::default();
        let callback_cancellation = cancellation.clone();
        let process_calls = Arc::new(AtomicUsize::new(0));
        let launcher_calls = Arc::clone(&process_calls);
        let executable = std::env::current_exe()?;

        let error = launch_vanilla_with_dependencies(
            &fixture.client,
            &fixture.identity,
            &fixture.config,
            &cancellation,
            move |update| {
                if update.stage == VanillaLaunchStage::Launching {
                    callback_cancellation.cancel();
                }
            },
            move |_| {
                let executable = executable.clone();
                async move {
                    Ok(JavaRuntime {
                        executable_path: executable,
                        major_version: JAVA_MAJOR,
                    })
                }
            },
            move |_| {
                launcher_calls.fetch_add(1, Ordering::AcqRel);
                unreachable!("process must not spawn after cancellation")
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(error, VanillaLaunchError::Cancelled));
        assert_eq!(process_calls.load(Ordering::Acquire), 0);
        assert!(
            MinecraftRuntimeLayout::new(&fixture.config.runtime_root)
                .natives_dir(VERSION_ID)
                .join("native.dll")
                .is_file()
        );
        Ok(())
    }

    #[tokio::test]
    async fn propagates_version_catalog_error() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let temp = tempfile::tempdir()?;
        let mut config = config_for_paths(temp.path().join("runtime"), temp.path().join("game"));
        config.catalog_url = server.url("missing-catalog.json")?;
        let client = build_http_client()?;

        let error = launch_vanilla_with_dependencies(
            &client,
            &offline_identity(),
            &config,
            &VanillaLaunchCancellation::default(),
            |_| {},
            |_| async { unreachable!("Java resolver must not be called") },
            |_| unreachable!("process must not be launched"),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            VanillaLaunchError::VersionCatalog(VersionCatalogError::HttpStatus { status, .. })
                if status == reqwest::StatusCode::NOT_FOUND
        ));
        Ok(())
    }

    #[tokio::test]
    async fn propagates_java_error_with_metadata_major() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let temp = tempfile::tempdir()?;
        let fixture = SyntheticFixture::new(&server, temp.path(), FixtureMode::Valid)?;
        let java_calls = Arc::new(Mutex::new(Vec::new()));
        let resolver_calls = Arc::clone(&java_calls);

        let error = launch_vanilla_with_dependencies(
            &fixture.client,
            &fixture.identity,
            &fixture.config,
            &VanillaLaunchCancellation::default(),
            |_| {},
            move |major| {
                resolver_calls.lock().unwrap().push(major);
                async move {
                    Err(JavaError::JavaNotFound {
                        required_major: major,
                    })
                }
            },
            |_| unreachable!("process must not be launched"),
        )
        .await
        .unwrap_err();

        assert_eq!(*java_calls.lock().unwrap(), vec![JAVA_MAJOR]);
        assert!(matches!(
            error,
            VanillaLaunchError::Java(JavaError::JavaNotFound {
                required_major: JAVA_MAJOR
            })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn java_override_is_used_for_discovery() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let temp = tempfile::tempdir()?;
        let fixture = SyntheticFixture::new(&server, temp.path(), FixtureMode::Valid)?;
        let java_calls = Arc::new(Mutex::new(Vec::new()));
        let resolver_calls = Arc::clone(&java_calls);
        let mut config = fixture.config.clone();
        config.java_major_override = Some(17);

        let error = launch_vanilla_with_dependencies(
            &fixture.client,
            &fixture.identity,
            &config,
            &VanillaLaunchCancellation::default(),
            |_| {},
            move |major| {
                resolver_calls.lock().unwrap().push(major);
                async move {
                    Err(JavaError::JavaNotFound {
                        required_major: major,
                    })
                }
            },
            |_| unreachable!("process must not be launched"),
        )
        .await
        .unwrap_err();

        assert_eq!(*java_calls.lock().unwrap(), vec![17]);
        assert!(matches!(
            error,
            VanillaLaunchError::Java(JavaError::JavaNotFound { required_major: 17 })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn propagates_runtime_preparation_error() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let temp = tempfile::tempdir()?;
        let fixture = SyntheticFixture::new(&server, temp.path(), FixtureMode::InvalidNative)?;

        let error = launch_without_process(&fixture, VanillaLaunchCancellation::default(), |_| {})
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            VanillaLaunchError::RuntimePreparation(PrepareVanillaLaunchError::NativeExtraction(_))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn propagates_process_spawn_error() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let temp = tempfile::tempdir()?;
        let fixture = SyntheticFixture::new(&server, temp.path(), FixtureMode::Valid)?;
        let executable = std::env::current_exe()?;

        let error = launch_vanilla_with_dependencies(
            &fixture.client,
            &fixture.identity,
            &fixture.config,
            &VanillaLaunchCancellation::default(),
            |_| {},
            move |_| {
                let executable = executable.clone();
                async move {
                    Ok(JavaRuntime {
                        executable_path: executable,
                        major_version: JAVA_MAJOR,
                    })
                }
            },
            |_| {
                Err(ProcessError::Spawn {
                    executable_path: "synthetic-helper".into(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "synthetic spawn failure",
                    ),
                })
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            VanillaLaunchError::Process(ProcessError::Spawn { .. })
        ));
        Ok(())
    }

    async fn launch_with_fixture(
        fixture: &SyntheticFixture,
        java_calls: &Arc<Mutex<Vec<u32>>>,
        captured_spec: &Arc<Mutex<Option<LaunchSpec>>>,
        progress: &Arc<Mutex<Vec<VanillaLaunchProgress>>>,
        cancellation: VanillaLaunchCancellation,
    ) -> Result<ProcessHandle, VanillaLaunchError> {
        let resolver_calls = Arc::clone(java_calls);
        let executable = std::env::current_exe().unwrap();
        let spec_capture = Arc::clone(captured_spec);
        let updates = Arc::clone(progress);
        launch_vanilla_with_dependencies(
            &fixture.client,
            &fixture.identity,
            &fixture.config,
            &cancellation,
            move |update| updates.lock().unwrap().push(update),
            move |major| {
                resolver_calls.lock().unwrap().push(major);
                let executable = executable.clone();
                async move {
                    Ok(JavaRuntime {
                        executable_path: executable,
                        major_version: major,
                    })
                }
            },
            move |spec| {
                *spec_capture.lock().unwrap() = Some(spec.clone());
                spawn_test_helper("capture", &spec.working_directory)
            },
        )
        .await
    }

    async fn launch_without_process<F>(
        fixture: &SyntheticFixture,
        cancellation: VanillaLaunchCancellation,
        on_progress: F,
    ) -> Result<ProcessHandle, VanillaLaunchError>
    where
        F: Fn(VanillaLaunchProgress) + Send + Sync + 'static,
    {
        let executable = std::env::current_exe().unwrap();
        launch_vanilla_with_dependencies(
            &fixture.client,
            &fixture.identity,
            &fixture.config,
            &cancellation,
            on_progress,
            move |_| {
                let executable = executable.clone();
                async move {
                    Ok(JavaRuntime {
                        executable_path: executable,
                        major_version: JAVA_MAJOR,
                    })
                }
            },
            |_| unreachable!("process must not be launched"),
        )
        .await
    }

    fn distinct_stages(progress: &[VanillaLaunchProgress]) -> Vec<VanillaLaunchStage> {
        let mut stages = Vec::new();
        for update in progress {
            if stages.last() != Some(&update.stage) {
                stages.push(update.stage);
            }
        }
        stages
    }

    #[derive(Clone, Copy)]
    pub(crate) enum FixtureMode {
        Valid,
        SlowClient,
        InvalidNative,
    }

    pub(crate) struct SyntheticFixture {
        pub(crate) client: reqwest::Client,
        pub(crate) identity: MinecraftLaunchIdentity,
        pub(crate) config: VanillaLaunchConfig,
        asset_route: String,
    }

    impl SyntheticFixture {
        pub(crate) fn new(
            server: &TestServer,
            root: &Path,
            mode: FixtureMode,
        ) -> Result<Self, Box<dyn Error>> {
            let client_jar = match mode {
                FixtureMode::SlowClient => vec![7_u8; 128 * 1024],
                FixtureMode::Valid | FixtureMode::InvalidNative => b"synthetic client".to_vec(),
            };
            let library = b"synthetic library".to_vec();
            let native_classpath = b"native classpath".to_vec();
            let native_archive = match mode {
                FixtureMode::InvalidNative => b"not a zip archive".to_vec(),
                FixtureMode::Valid | FixtureMode::SlowClient => native_archive(),
            };
            let asset = b"synthetic asset".to_vec();
            let asset_hash = sha1_hex(&asset);
            let asset_index = json!({
                "objects": {
                    "minecraft/test.bin": {
                        "hash": &asset_hash,
                        "size": asset.len()
                    }
                }
            })
            .to_string()
            .into_bytes();

            let client_response = match mode {
                FixtureMode::SlowClient => {
                    TestResponse::slow(client_jar.clone(), 1024, Duration::from_millis(2))
                }
                FixtureMode::Valid | FixtureMode::InvalidNative => {
                    TestResponse::ok(client_jar.clone())
                }
            };
            server.route("/runtime/client.jar", client_response);
            server.route("/runtime/library.jar", TestResponse::ok(library.clone()));
            server.route(
                "/runtime/native-classpath.jar",
                TestResponse::ok(native_classpath.clone()),
            );
            server.route(
                "/runtime/native-classifier.jar",
                TestResponse::ok(native_archive.clone()),
            );
            server.route("/runtime/index.json", TestResponse::ok(asset_index.clone()));
            server.route(
                &format!("/assets/objects/{}/{}", &asset_hash[..2], asset_hash),
                TestResponse::ok(asset),
            );

            let classifier = if cfg!(target_pointer_width = "64") {
                "natives-windows-64"
            } else {
                "natives-windows-32"
            };
            let mut classifiers = serde_json::Map::new();
            classifiers.insert(
                classifier.to_owned(),
                artifact(
                    &format!("org/test/native/1/native-1-{classifier}.jar"),
                    server.url("runtime/native-classifier.jar")?.as_str(),
                    &native_archive,
                ),
            );
            let metadata = json!({
                "id": VERSION_ID,
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assets": "test-assets",
                "assetIndex": {
                    "id": "test-assets",
                    "url": server.url("runtime/index.json")?.as_str(),
                    "sha1": sha1_hex(&asset_index),
                    "size": asset_index.len()
                },
                "downloads": {
                    "client": download(server.url("runtime/client.jar")?.as_str(), &client_jar)
                },
                "libraries": [
                    {
                        "name": "org.test:library:1",
                        "downloads": {
                            "artifact": artifact(
                                "org/test/library/1/library.jar",
                                server.url("runtime/library.jar")?.as_str(),
                                &library
                            )
                        }
                    },
                    {
                        "name": "org.test:native:1",
                        "natives": { "windows": "natives-windows-${arch}" },
                        "downloads": {
                            "artifact": artifact(
                                "org/test/native/1/native.jar",
                                server.url("runtime/native-classpath.jar")?.as_str(),
                                &native_classpath
                            ),
                            "classifiers": classifiers
                        }
                    }
                ],
                "arguments": {
                    "jvm": ["-Djava.library.path=${natives_directory}"],
                    "game": [
                        "--username", "${auth_player_name}",
                        "--uuid", "${auth_uuid}",
                        "--accessToken", "${auth_access_token}",
                        "--userType", "${user_type}"
                    ]
                },
                "javaVersion": { "majorVersion": JAVA_MAJOR }
            })
            .to_string();
            let metadata_sha1 = sha1_hex(metadata.as_bytes());
            server.route(
                "/versions/selected.json",
                TestResponse::ok(metadata.clone()),
            );
            server.route(
                "/catalog.json",
                TestResponse::ok(
                    json!({
                        "latest": { "release": VERSION_ID, "snapshot": "wrong-version" },
                        "versions": [
                            {
                                "id": "wrong-version",
                                "type": "snapshot",
                                "url": server.url("versions/wrong.json")?.as_str(),
                                "sha1": "0".repeat(40)
                            },
                            {
                                "id": VERSION_ID,
                                "type": "release",
                                "url": server.url("versions/selected.json")?.as_str(),
                                "sha1": metadata_sha1
                            }
                        ]
                    })
                    .to_string(),
                ),
            );

            let runtime_root = root.join("runtime");
            let game_directory = root.join("Game Directory");
            std::fs::create_dir_all(&game_directory)?;
            let mut config = config_for_paths(runtime_root, game_directory);
            config.catalog_url = server.url("catalog.json")?;
            config.assets_base_url = server.url("assets/objects/")?;
            let asset_route = format!("/assets/objects/{}/{}", &asset_hash[..2], asset_hash);
            Ok(Self {
                client: build_http_client()?,
                identity: offline_identity(),
                config,
                asset_route,
            })
        }

        fn runtime_routes(&self) -> Vec<&str> {
            vec![
                "/runtime/client.jar",
                "/runtime/library.jar",
                "/runtime/native-classpath.jar",
                "/runtime/native-classifier.jar",
                "/runtime/index.json",
                self.asset_route.as_str(),
            ]
        }
    }

    fn config_for_paths(runtime_root: PathBuf, game_directory: PathBuf) -> VanillaLaunchConfig {
        VanillaLaunchConfig {
            telegram_auth: None,
            heap_memory: None,
            jvm_args: Vec::new(),
            version_id: VERSION_ID.to_owned(),
            java_major_override: None,
            server_address: None,
            catalog_url: reqwest::Url::parse("https://example.invalid/catalog.json").unwrap(),
            assets_base_url: reqwest::Url::parse("https://example.invalid/assets/").unwrap(),
            runtime_root,
            game_directory,
            launcher_name: "Launcher Core Tests".to_owned(),
            launcher_version: "0.1.0-test".to_owned(),
        }
    }

    fn session() -> MinecraftSession {
        MinecraftSession::new(
            "TestPlayer",
            "0123456789abcdef0123456789abcdef",
            "minecraft-access-secret",
        )
    }

    fn offline_identity() -> MinecraftLaunchIdentity {
        MinecraftLaunchIdentity::from(&OfflineProfile::new("TestPlayer").unwrap())
    }

    fn contains_pair(arguments: &[std::ffi::OsString], name: &str, value: &str) -> bool {
        arguments
            .windows(2)
            .any(|pair| pair == [OsStr::new(name), OsStr::new(value)])
    }

    fn download(url: &str, contents: &[u8]) -> serde_json::Value {
        json!({ "url": url, "sha1": sha1_hex(contents), "size": contents.len() })
    }

    fn artifact(path: &str, url: &str, contents: &[u8]) -> serde_json::Value {
        json!({
            "path": path,
            "url": url,
            "sha1": sha1_hex(contents),
            "size": contents.len()
        })
    }

    fn sha1_hex(contents: &[u8]) -> String {
        format!("{:x}", Sha1::digest(contents))
    }

    fn native_archive() -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        writer.start_file("native.dll", options).unwrap();
        writer.write_all(b"synthetic native").unwrap();
        writer.finish().unwrap().into_inner()
    }
}
