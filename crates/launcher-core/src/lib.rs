pub mod auth;
mod client_config;
mod diagnostics;
mod game_actor;
pub mod http;
pub mod identity;
pub mod installer;
pub mod java;
pub mod java_install;
mod launch_options;
pub mod launcher_update;
pub mod manifest;
pub mod minecraft;
mod mods;
pub mod process;
mod process_command;
mod server_status;
pub mod source;
pub mod verification;

use std::{error::Error, fmt, path::PathBuf, sync::Arc, time::Duration};

pub use auth::{
    AuthCancellation, AuthHttpError, DeviceLogin, EntitlementError, MAX_AUTH_RESPONSE_SIZE,
    MicrosoftAuthConfig, MicrosoftAuthError, MicrosoftTokenSet, MinecraftAccessToken,
    MinecraftEntitlement, MinecraftEntitlements, MinecraftLoginError, MinecraftProfile,
    MinecraftProfileError, MinecraftServicesConfig, MinecraftSession, MinecraftSessionError,
    XboxAuthConfig, XboxUserAuthError, XboxUserToken, XstsAuthError, XstsToken,
    authenticate_minecraft, authenticate_xbox_user, authenticate_xsts, begin_device_login,
    check_minecraft_entitlement, complete_device_login, create_minecraft_session,
    fetch_minecraft_profile, refresh_microsoft_token,
};
pub use client_config::{
    ClientConfig, ClientConfigFetchError, ClientServer, LauncherBackendConfig,
    LauncherBackendConfigError, MAX_CLIENT_CONFIG_SIZE, fetch_client_config,
};
pub use diagnostics::LauncherDiagnostic;
pub use game_actor::LauncherGameConfig;
pub use http::{
    HttpClientBuildError, HttpFileSource, HttpFileSourceConfigError, HttpUrlError,
    MAX_MANIFEST_SIZE, ManifestFetchError, RemotePackConfig, RemotePackConfigError,
    build_http_client, fetch_manifest,
};
pub use identity::{
    MAX_OFFLINE_USERNAME_LENGTH, MIN_OFFLINE_USERNAME_LENGTH, MinecraftLaunchIdentity,
    OfflineProfile, OfflineProfileError,
};
pub use installer::{
    InstallCancellation, InstallError, InstallProgress, InstallSummary, install_files,
};
pub use java::{JavaError, JavaRuntime, find_java};
pub use launch_options::{HeapMemory, InvalidHeapMemory, UserLaunchOptions};
pub use manifest::{ManifestFile, PackManifest};
pub use minecraft::neoforge::{
    NeoForgeDistribution, NeoForgeInstallProfile, NeoForgeInstallerError, NeoForgeJvmOptions,
    NeoForgeLaunchCancellation, NeoForgeLaunchConfig, NeoForgeLaunchError, NeoForgeLaunchProgress,
    NeoForgeLaunchStage, NeoForgeMetadataError, NeoForgePreparationRequest,
    NeoForgePreparationSummary, NeoForgeProcessor, NeoForgeSidedValue, NeoForgeVersionMetadata,
    PreparedNeoForgeRuntime, ProfileLibrary, build_neoforge_launch_spec,
    launch_neoforge_with_identity, neoforge_version_id, prepare_neoforge_runtime,
};
pub use minecraft::{
    AcquisitionCancellation, AcquisitionError, AcquisitionProgress, AcquisitionSummary, Argument,
    ArgumentValue, AssetIndex, AssetIndexFile, AssetObject, AssetPathError, DownloadInfo,
    FetchVersionByIdError, JavaVersionRequirement, Library, LibraryArtifact, LibraryDownloads,
    LibraryExtract, MAX_VERSION_CATALOG_SIZE, MinecraftArguments, MinecraftLaunchContext,
    MinecraftLaunchError, MinecraftPlaceholderValues, MinecraftRuntimeLayout,
    MissingRuntimeComponent, NativeArchive, NativeExtractionError, NativeExtractionSummary,
    OperatingSystem, PlaceholderError, PrepareVanillaLaunchError, Rule, RuleAction, RuleContext,
    RuleOs, RuntimeDownloadError, RuntimeValidationError, RuntimeValidationResult,
    VanillaAcquisitionConfig, VanillaLaunchCancellation, VanillaLaunchConfig, VanillaLaunchError,
    VanillaLaunchProgress, VanillaLaunchStage, VerifiedDownload, VersionCatalog,
    VersionCatalogEntry, VersionCatalogError, VersionCatalogLatest, VersionDownloads,
    VersionMetadata, VersionMetadataFetchError, VersionResolveError, acquire_vanilla_runtime,
    asset_object_path, asset_object_url, build_vanilla_launch_spec, download_verified_file,
    extract_native_libraries, fetch_version_by_id, fetch_version_catalog, fetch_version_metadata,
    fetch_version_metadata_verified, identity_to_launch_context, launch_vanilla,
    launch_vanilla_with_identity, prepare_vanilla_launch_spec, resolve_native_archives,
    resolve_version, rules_allow, session_to_launch_context, substitute_placeholders,
    validate_vanilla_runtime,
};
pub use mods::{
    MAX_EXTRACTED_MODS_SIZE, MAX_MODS_ZIP_ENTRIES, MAX_MODS_ZIP_SIZE, ModsSyncCancellation,
    ModsSyncError, ModsSyncOutcome, ModsSyncProgress, ModsSyncStage, sync_mods,
};
pub use process::{
    LaunchSpec, LaunchSpecError, ProcessCancellation, ProcessError, ProcessEvent, ProcessExit,
    ProcessHandle, build_command, launch_process,
};
pub use server_status::{ServerStatusError, ping_minecraft_server};
pub use source::{FileChunkStream, FileSource, FileSourceError, LocalFileSource, SourceFuture};
use tokio::{
    runtime::Handle,
    sync::{mpsc, watch},
};
pub use verification::{
    CorruptFile, InvalidManifestPathReason, VerificationError, VerificationResult,
    verify_installation,
};

const COMMAND_CHANNEL_CAPACITY: usize = 8;
const CHECKING_DELAY: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LauncherCommand {
    Play,
    Cancel,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LauncherStatus {
    #[default]
    Idle,
    Checking,
    Downloading,
    Ready,
    Launching,
    Running,
    AuthenticationRequired,
    ProfileRequired,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LauncherStage {
    FetchingConfig,
    CheckingFiles,
    DownloadingPack,
    SynchronizingMods,
    ResolvingVersion,
    CheckingJava,
    AcquiringVanilla,
    AcquiringNeoForge,
    PreparingRuntime,
    StartingProcess,
    Running,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LauncherState {
    status: LauncherStatus,
    progress: Option<f32>,
    client_config: Option<ClientConfig>,
    failure: Option<LauncherFailure>,
    stage: Option<LauncherStage>,
    diagnostic: Option<LauncherDiagnostic>,
    mods_progress: Option<ModsSyncProgress>,
}

impl LauncherState {
    pub const fn status(&self) -> LauncherStatus {
        self.status
    }

    pub const fn progress(&self) -> Option<f32> {
        self.progress
    }

    pub const fn client_config(&self) -> Option<&ClientConfig> {
        self.client_config.as_ref()
    }

    pub const fn failure(&self) -> Option<&LauncherFailure> {
        self.failure.as_ref()
    }

    pub const fn stage(&self) -> Option<LauncherStage> {
        self.stage
    }
    pub const fn diagnostic(&self) -> Option<&LauncherDiagnostic> {
        self.diagnostic.as_ref()
    }
    pub const fn mods_progress(&self) -> Option<&ModsSyncProgress> {
        self.mods_progress.as_ref()
    }

    const fn new(status: LauncherStatus, progress: Option<f32>) -> Self {
        Self {
            status,
            progress,
            client_config: None,
            failure: None,
            stage: None,
            diagnostic: None,
            mods_progress: None,
        }
    }

    fn with_failure(failure: LauncherFailure) -> Self {
        let mut state = Self::new(LauncherStatus::Error, None);
        state.failure = Some(failure);
        state
    }

    fn at_stage(mut self, stage: LauncherStage) -> Self {
        self.stage = Some(stage);
        self
    }

    fn with_mods_progress(progress: ModsSyncProgress) -> Self {
        let mut state = Self::new(LauncherStatus::Downloading, progress.fraction())
            .at_stage(LauncherStage::SynchronizingMods);
        state.mods_progress = Some(progress);
        state
    }

    fn with_diagnostic(failure: LauncherFailure, diagnostic: LauncherDiagnostic) -> Self {
        let mut state = Self::with_failure(failure);
        state.diagnostic = Some(diagnostic);
        state
    }

    const fn stable(status: LauncherStatus) -> Self {
        match status {
            LauncherStatus::Ready => Self::new(LauncherStatus::Ready, Some(1.0)),
            LauncherStatus::AuthenticationRequired => {
                Self::new(LauncherStatus::AuthenticationRequired, None)
            }
            LauncherStatus::ProfileRequired => Self::new(LauncherStatus::ProfileRequired, None),
            _ => Self::new(LauncherStatus::Idle, None),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LauncherFailure {
    AuthenticationVerification,
    ClientConfig,
    ModsSync,
    LaunchOptions,
    UnsupportedLoader(String),
    Installation,
    GameLaunch,
    Java { required_major: u32 },
    GameExit { code: Option<i32> },
    Internal,
}

impl Default for LauncherState {
    fn default() -> Self {
        Self::stable(LauncherStatus::Idle)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendCommandError;

impl fmt::Display for SendCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("launcher core command processor is not running")
    }
}

impl Error for SendCommandError {}

#[derive(Clone, Debug)]
pub struct LauncherCore {
    command_tx: mpsc::Sender<LauncherCommand>,
    state_rx: watch::Receiver<LauncherState>,
    offline_profile_tx: watch::Sender<Option<OfflineProfile>>,
    launch_options_tx: watch::Sender<UserLaunchOptions>,
}

impl LauncherCore {
    pub fn new(runtime: &Handle) -> Self {
        Self::new_with_installation(runtime, ".", PackManifest::default())
    }

    pub fn new_with_installation(
        runtime: &Handle,
        root_dir: impl Into<PathBuf>,
        manifest: PackManifest,
    ) -> Self {
        let root_dir = root_dir.into();
        let source = LocalFileSource::new(root_dir.clone());
        Self::new_with_file_source(runtime, root_dir, manifest, source)
    }

    pub fn new_with_file_source(
        runtime: &Handle,
        root_dir: impl Into<PathBuf>,
        manifest: PackManifest,
        source: impl FileSource + 'static,
    ) -> Self {
        Self::new_configured(
            runtime,
            root_dir.into(),
            ManifestProvider::Local(manifest),
            Arc::new(source),
            None,
        )
    }

    pub fn new_with_game(
        runtime: &Handle,
        root_dir: impl Into<PathBuf>,
        manifest: PackManifest,
        game: LauncherGameConfig,
    ) -> Self {
        let root_dir = root_dir.into();
        let source = LocalFileSource::new(root_dir.clone());
        Self::new_with_file_source_and_game(runtime, root_dir, manifest, source, game)
    }

    pub fn new_with_file_source_and_game(
        runtime: &Handle,
        root_dir: impl Into<PathBuf>,
        manifest: PackManifest,
        source: impl FileSource + 'static,
        game: LauncherGameConfig,
    ) -> Self {
        Self::new_configured(
            runtime,
            root_dir.into(),
            ManifestProvider::Local(manifest),
            Arc::new(source),
            Some(game),
        )
    }

    pub fn new_remote(
        runtime: &Handle,
        root_dir: impl Into<PathBuf>,
        client: reqwest::Client,
        config: RemotePackConfig,
    ) -> Result<Self, HttpFileSourceConfigError> {
        let source = HttpFileSource::new(client.clone(), config.files_base_url().clone())?;
        Ok(Self::new_configured(
            runtime,
            root_dir.into(),
            ManifestProvider::Remote {
                client,
                manifest_url: config.manifest_url().clone(),
            },
            Arc::new(source),
            None,
        ))
    }

    pub fn new_remote_with_game(
        runtime: &Handle,
        root_dir: impl Into<PathBuf>,
        client: reqwest::Client,
        config: RemotePackConfig,
        game: LauncherGameConfig,
    ) -> Result<Self, HttpFileSourceConfigError> {
        let source = HttpFileSource::new(client.clone(), config.files_base_url().clone())?;
        Ok(Self::new_configured(
            runtime,
            root_dir.into(),
            ManifestProvider::Remote {
                client,
                manifest_url: config.manifest_url().clone(),
            },
            Arc::new(source),
            Some(game),
        ))
    }

    pub fn new_backend_with_game(
        runtime: &Handle,
        root_dir: impl Into<PathBuf>,
        client: reqwest::Client,
        config: LauncherBackendConfig,
        game: LauncherGameConfig,
    ) -> Self {
        let root_dir = root_dir.into();
        let installation = Installation {
            manifest: ManifestProvider::Local(PackManifest::default()),
            source: Arc::new(LocalFileSource::new(root_dir.clone())),
            root_dir,
            game: Some(game),
            backend: Some(BackendInstallation { client, config }),
        };
        Self::spawn(runtime, installation)
    }

    fn new_configured(
        runtime: &Handle,
        root_dir: PathBuf,
        manifest: ManifestProvider,
        source: Arc<dyn FileSource>,
        game: Option<LauncherGameConfig>,
    ) -> Self {
        let installation = Installation {
            root_dir,
            manifest,
            source,
            game,
            backend: None,
        };

        Self::spawn(runtime, installation)
    }

    fn spawn(runtime: &Handle, installation: Installation) -> Self {
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        let (state_tx, state_rx) = watch::channel(LauncherState::default());
        let (offline_profile_tx, offline_profile_rx) = watch::channel(None);
        let (launch_options_tx, launch_options_rx) = watch::channel(UserLaunchOptions::default());

        let actor = runtime.spawn(run_actor(
            command_rx,
            state_tx,
            offline_profile_rx,
            launch_options_rx,
            installation,
        ));
        drop(actor);

        Self {
            command_tx,
            state_rx,
            offline_profile_tx,
            launch_options_tx,
        }
    }

    pub async fn send_command(&self, command: LauncherCommand) -> Result<(), SendCommandError> {
        self.command_tx
            .send(command)
            .await
            .map_err(|_| SendCommandError)
    }

    pub fn subscribe_state(&self) -> watch::Receiver<LauncherState> {
        self.state_rx.clone()
    }

    pub fn set_offline_profile(&self, profile: OfflineProfile) {
        self.offline_profile_tx.send_replace(Some(profile));
    }

    pub fn clear_offline_profile(&self) {
        self.offline_profile_tx.send_replace(None);
    }

    pub fn set_launch_options(&self, options: UserLaunchOptions) {
        self.launch_options_tx.send_replace(options);
    }
}

#[derive(Clone)]
struct Installation {
    root_dir: PathBuf,
    manifest: ManifestProvider,
    source: Arc<dyn FileSource>,
    game: Option<LauncherGameConfig>,
    backend: Option<BackendInstallation>,
}

#[derive(Clone)]
struct BackendInstallation {
    client: reqwest::Client,
    config: LauncherBackendConfig,
}

#[derive(Clone)]
enum ManifestProvider {
    Local(PackManifest),
    Remote {
        client: reqwest::Client,
        manifest_url: reqwest::Url,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkflowControl {
    Continue,
    Cancel,
    Stop,
}

enum VerificationStep {
    Finished(Result<VerificationResult, VerificationError>),
    TaskFailed(tokio::task::JoinError),
    Cancel,
    Stop,
}

enum ManifestStep {
    Finished(Result<PackManifest, ManifestFetchError>),
    Cancel,
    Stop,
}

enum InstallStep {
    Finished(Result<InstallSummary, InstallError>),
    TaskFailed(tokio::task::JoinError),
    Cancel,
    Stop,
}

enum ClientConfigStep {
    Finished(Box<Result<ClientConfig, ClientConfigFetchError>>),
    Cancel,
    Stop,
}

enum ModsSyncStep {
    Finished(Result<ModsSyncOutcome, ModsSyncError>),
    TaskFailed(tokio::task::JoinError),
    Cancel,
    Stop,
}

async fn run_actor(
    mut commands: mpsc::Receiver<LauncherCommand>,
    states: watch::Sender<LauncherState>,
    offline_profiles: watch::Receiver<Option<OfflineProfile>>,
    launch_options: watch::Receiver<UserLaunchOptions>,
    installation: Installation,
) {
    while let Some(command) = commands.recv().await {
        match command {
            LauncherCommand::Play if is_playable(states.borrow().status()) => {
                let resume_status = states.borrow().status();

                let options = *launch_options.borrow();
                match run_play_workflow(
                    &mut commands,
                    &states,
                    &offline_profiles,
                    &installation,
                    options,
                )
                .await
                {
                    WorkflowControl::Continue => {}
                    WorkflowControl::Cancel => {
                        publish_state(&states, LauncherState::stable(resume_status));
                    }
                    WorkflowControl::Stop => return,
                }
            }
            LauncherCommand::Play | LauncherCommand::Cancel => {}
        }
    }
}

async fn run_play_workflow(
    commands: &mut mpsc::Receiver<LauncherCommand>,
    states: &watch::Sender<LauncherState>,
    offline_profiles: &watch::Receiver<Option<OfflineProfile>>,
    installation: &Installation,
    options: UserLaunchOptions,
) -> WorkflowControl {
    tracing::info!("checking pack");
    publish_state(
        states,
        LauncherState::new(LauncherStatus::Checking, None).at_stage(
            if installation.backend.is_some() {
                LauncherStage::FetchingConfig
            } else {
                LauncherStage::CheckingFiles
            },
        ),
    );

    if let control @ (WorkflowControl::Cancel | WorkflowControl::Stop) =
        wait_or_receive_control(commands, CHECKING_DELAY).await
    {
        return control;
    }

    if let Some(backend) = &installation.backend {
        return run_backend_play_workflow(
            commands,
            states,
            offline_profiles,
            installation,
            backend,
            options,
        )
        .await;
    }

    let manifest =
        match fetch_pack_manifest_or_receive_control(commands, &installation.manifest).await {
            ManifestStep::Finished(Ok(manifest)) => manifest,
            ManifestStep::Finished(Err(error)) => {
                publish_state(
                    states,
                    LauncherState::with_diagnostic(
                        LauncherFailure::Installation,
                        LauncherDiagnostic::from_error(&error),
                    ),
                );
                return WorkflowControl::Continue;
            }
            ManifestStep::Cancel => return WorkflowControl::Cancel,
            ManifestStep::Stop => return WorkflowControl::Stop,
        };

    let verification =
        match verify_manifest_or_receive_control(commands, &installation.root_dir, &manifest).await
        {
            VerificationStep::Finished(Ok(result)) => result,
            VerificationStep::Finished(Err(error)) => {
                publish_state(
                    states,
                    LauncherState::with_diagnostic(
                        LauncherFailure::Installation,
                        LauncherDiagnostic::from_error(&error),
                    ),
                );
                return WorkflowControl::Continue;
            }
            VerificationStep::TaskFailed(error) => {
                publish_state(
                    states,
                    LauncherState::with_diagnostic(
                        LauncherFailure::Internal,
                        LauncherDiagnostic::from_error(&error),
                    ),
                );
                return WorkflowControl::Continue;
            }
            VerificationStep::Cancel => return WorkflowControl::Cancel,
            VerificationStep::Stop => return WorkflowControl::Stop,
        };

    if !verification.is_valid() {
        tracing::info!("downloading pack");
        publish_state(
            states,
            LauncherState::new(LauncherStatus::Downloading, Some(0.0))
                .at_stage(LauncherStage::DownloadingPack),
        );
        match install_or_receive_control(
            commands,
            states,
            installation,
            manifest.clone(),
            verification,
        )
        .await
        {
            InstallStep::Finished(Ok(_)) => {}
            InstallStep::Finished(Err(error)) => {
                publish_state(
                    states,
                    LauncherState::with_diagnostic(
                        LauncherFailure::Installation,
                        LauncherDiagnostic::from_error(&error),
                    ),
                );
                return WorkflowControl::Continue;
            }
            InstallStep::TaskFailed(error) => {
                publish_state(
                    states,
                    LauncherState::with_diagnostic(
                        LauncherFailure::Internal,
                        LauncherDiagnostic::from_error(&error),
                    ),
                );
                return WorkflowControl::Continue;
            }
            InstallStep::Cancel => return WorkflowControl::Cancel,
            InstallStep::Stop => return WorkflowControl::Stop,
        }

        publish_state(
            states,
            LauncherState::new(LauncherStatus::Checking, None)
                .at_stage(LauncherStage::CheckingFiles),
        );
        let verification =
            match verify_manifest_or_receive_control(commands, &installation.root_dir, &manifest)
                .await
            {
                VerificationStep::Finished(Ok(result)) => result,
                VerificationStep::Finished(Err(error)) => {
                    publish_state(
                        states,
                        LauncherState::with_diagnostic(
                            LauncherFailure::Installation,
                            LauncherDiagnostic::from_error(&error),
                        ),
                    );
                    return WorkflowControl::Continue;
                }
                VerificationStep::TaskFailed(error) => {
                    publish_state(
                        states,
                        LauncherState::with_diagnostic(
                            LauncherFailure::Internal,
                            LauncherDiagnostic::from_error(&error),
                        ),
                    );
                    return WorkflowControl::Continue;
                }
                VerificationStep::Cancel => return WorkflowControl::Cancel,
                VerificationStep::Stop => return WorkflowControl::Stop,
            };

        if !verification.is_valid() {
            publish_state(
                states,
                LauncherState::with_diagnostic(
                    LauncherFailure::Installation,
                    LauncherDiagnostic::message("Проверка файлов после установки не пройдена"),
                ),
            );
            return WorkflowControl::Continue;
        }
    }

    let Some(game) = &installation.game else {
        publish_state(states, LauncherState::new(LauncherStatus::Ready, Some(1.0)));
        return WorkflowControl::Continue;
    };
    let Some(profile) = offline_profiles.borrow().clone() else {
        publish_state(
            states,
            LauncherState::new(LauncherStatus::ProfileRequired, None),
        );
        return WorkflowControl::Continue;
    };
    let identity = MinecraftLaunchIdentity::from(&profile);

    let game = match game.with_launch_options(options) {
        Ok(game) => game,
        Err(error) => {
            tracing::error!(%error, "invalid launch options");
            publish_state(
                states,
                LauncherState::with_diagnostic(
                    LauncherFailure::LaunchOptions,
                    LauncherDiagnostic::from_error(&error),
                ),
            );
            return WorkflowControl::Continue;
        }
    };
    game_actor::run_game_workflow(commands, states, identity, &game).await
}

async fn run_backend_play_workflow(
    commands: &mut mpsc::Receiver<LauncherCommand>,
    states: &watch::Sender<LauncherState>,
    offline_profiles: &watch::Receiver<Option<OfflineProfile>>,
    installation: &Installation,
    backend: &BackendInstallation,
    options: UserLaunchOptions,
) -> WorkflowControl {
    if let Some(gate) = installation
        .game
        .as_ref()
        .and_then(|g| g.vanilla.telegram_auth.as_ref())
    {
        match gate.verify().await {
            Ok(user) => {
                if offline_profiles
                    .borrow()
                    .as_ref()
                    .map(OfflineProfile::username)
                    != Some(user.nickname.as_str())
                {
                    publish_state(
                        states,
                        LauncherState::new(LauncherStatus::AuthenticationRequired, None),
                    );
                    return WorkflowControl::Continue;
                }
            }
            Err(auth::telegram::TelegramAuthError::Unauthorized) => {
                publish_state(
                    states,
                    LauncherState::new(LauncherStatus::AuthenticationRequired, None),
                );
                return WorkflowControl::Continue;
            }
            Err(error) => {
                publish_state(
                    states,
                    LauncherState::with_diagnostic(
                        LauncherFailure::AuthenticationVerification,
                        LauncherDiagnostic::from_error(&error),
                    ),
                );
                return WorkflowControl::Continue;
            }
        }
    }
    let config = match fetch_client_config_or_receive_control(commands, backend).await {
        ClientConfigStep::Finished(result) => match *result {
            Ok(config) => config,
            Err(error) => {
                tracing::error!(%error, "failed to fetch Minecraft client config");
                publish_state(
                    states,
                    LauncherState::with_diagnostic(
                        LauncherFailure::ClientConfig,
                        LauncherDiagnostic::from_error(&error),
                    ),
                );
                return WorkflowControl::Continue;
            }
        },
        ClientConfigStep::Cancel => return WorkflowControl::Cancel,
        ClientConfigStep::Stop => return WorkflowControl::Stop,
    };
    publish_client_config(states, config.clone());

    let configured_game = match installation
        .game
        .as_ref()
        .map(|game| game.for_client_config(&config, options))
        .transpose()
    {
        Ok(game) => game,
        Err(error) => {
            tracing::error!(%error, "invalid backend or user heap settings");
            publish_state(
                states,
                LauncherState::with_diagnostic(
                    LauncherFailure::LaunchOptions,
                    LauncherDiagnostic::from_error(&error),
                ),
            );
            return WorkflowControl::Continue;
        }
    };

    tracing::info!(mods_hash = %config.mods_hash, "synchronizing mods archive");
    publish_state(
        states,
        LauncherState::new(LauncherStatus::Downloading, Some(0.0))
            .at_stage(LauncherStage::SynchronizingMods),
    );
    match sync_mods_or_receive_control(
        commands,
        states,
        &backend.client,
        &config,
        &installation.root_dir,
    )
    .await
    {
        ModsSyncStep::Finished(Ok(_)) => {}
        ModsSyncStep::Finished(Err(ModsSyncError::Cancelled)) | ModsSyncStep::Cancel => {
            return WorkflowControl::Cancel;
        }
        ModsSyncStep::Finished(Err(error)) => {
            tracing::error!(%error, "failed to synchronize mods archive");
            publish_state(
                states,
                LauncherState::with_diagnostic(
                    LauncherFailure::ModsSync,
                    LauncherDiagnostic::from_error(&error),
                ),
            );
            return WorkflowControl::Continue;
        }
        ModsSyncStep::TaskFailed(error) => {
            tracing::error!(%error, "mods synchronization task failed");
            publish_state(
                states,
                LauncherState::with_diagnostic(
                    LauncherFailure::ModsSync,
                    LauncherDiagnostic::from_error(&error),
                ),
            );
            return WorkflowControl::Continue;
        }
        ModsSyncStep::Stop => return WorkflowControl::Stop,
    }

    let loader = if let Some(loader) = BackendLoader::parse(&config.loader) {
        loader
    } else {
        tracing::warn!(loader = %config.loader, "unsupported Minecraft loader");
        publish_state(
            states,
            LauncherState::with_failure(LauncherFailure::UnsupportedLoader(config.loader)),
        );
        return WorkflowControl::Continue;
    };

    let Some(game) = &configured_game else {
        publish_state(states, LauncherState::new(LauncherStatus::Ready, Some(1.0)));
        return WorkflowControl::Continue;
    };
    let Some(profile) = offline_profiles.borrow().clone() else {
        publish_state(
            states,
            LauncherState::new(LauncherStatus::ProfileRequired, None),
        );
        return WorkflowControl::Continue;
    };

    let identity = MinecraftLaunchIdentity::from(&profile);
    if let Some(control) =
        game_actor::run_java_setup(commands, states, game, config.java_version).await
    {
        return control;
    }
    match loader {
        BackendLoader::Vanilla => {
            game_actor::run_game_workflow(commands, states, identity, game).await
        }
        BackendLoader::NeoForge => {
            let neoforge = game.neoforge_for_client_config(&config);
            game_actor::run_neoforge_game_workflow(commands, states, identity, game, neoforge).await
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackendLoader {
    Vanilla,
    NeoForge,
}

impl BackendLoader {
    fn parse(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("vanilla") {
            Some(Self::Vanilla)
        } else if value.eq_ignore_ascii_case("neoforge") {
            Some(Self::NeoForge)
        } else {
            None
        }
    }
}

async fn fetch_client_config_or_receive_control(
    commands: &mut mpsc::Receiver<LauncherCommand>,
    backend: &BackendInstallation,
) -> ClientConfigStep {
    let fetch = fetch_client_config(&backend.client, backend.config.client_config_url());
    tokio::pin!(fetch);
    loop {
        tokio::select! {
            result = &mut fetch => return ClientConfigStep::Finished(Box::new(result)),
            command = commands.recv() => match command {
                Some(LauncherCommand::Cancel) => return ClientConfigStep::Cancel,
                Some(LauncherCommand::Play) => {}
                None => return ClientConfigStep::Stop,
            }
        }
    }
}

async fn sync_mods_or_receive_control(
    commands: &mut mpsc::Receiver<LauncherCommand>,
    states: &watch::Sender<LauncherState>,
    client: &reqwest::Client,
    config: &ClientConfig,
    root_dir: &std::path::Path,
) -> ModsSyncStep {
    let client = client.clone();
    let config = config.clone();
    let root_dir = root_dir.to_owned();
    let cancellation = ModsSyncCancellation::default();
    let task_cancellation = cancellation.clone();
    let progress_states = states.clone();
    let mut task = tokio::spawn(async move {
        sync_mods(
            &client,
            &config,
            &root_dir,
            &task_cancellation,
            move |progress| {
                publish_state(
                    &progress_states,
                    LauncherState::with_mods_progress(progress),
                );
            },
        )
        .await
    });

    loop {
        tokio::select! {
            result = &mut task => {
                return match result {
                    Ok(result) => ModsSyncStep::Finished(result),
                    Err(error) => ModsSyncStep::TaskFailed(error),
                };
            }
            command = commands.recv() => match command {
                Some(LauncherCommand::Cancel) => {
                    cancellation.cancel();
                    let _task_result = task.await;
                    return ModsSyncStep::Cancel;
                }
                Some(LauncherCommand::Play) => {}
                None => {
                    cancellation.cancel();
                    let _task_result = task.await;
                    return ModsSyncStep::Stop;
                }
            }
        }
    }
}

async fn fetch_pack_manifest_or_receive_control(
    commands: &mut mpsc::Receiver<LauncherCommand>,
    provider: &ManifestProvider,
) -> ManifestStep {
    match provider {
        ManifestProvider::Local(manifest) => ManifestStep::Finished(Ok(manifest.clone())),
        ManifestProvider::Remote {
            client,
            manifest_url,
        } => {
            let fetch = fetch_manifest(client, manifest_url);
            tokio::pin!(fetch);
            loop {
                tokio::select! {
                    result = &mut fetch => return ManifestStep::Finished(result),
                    command = commands.recv() => match command {
                        Some(LauncherCommand::Cancel) => return ManifestStep::Cancel,
                        Some(LauncherCommand::Play) => {}
                        None => return ManifestStep::Stop,
                    }
                }
            }
        }
    }
}

async fn verify_manifest_or_receive_control(
    commands: &mut mpsc::Receiver<LauncherCommand>,
    root_dir: &std::path::Path,
    manifest: &PackManifest,
) -> VerificationStep {
    let root_dir = root_dir.to_owned();
    let manifest = manifest.clone();
    let mut verification_task =
        tokio::task::spawn_blocking(move || verify_installation(root_dir, &manifest));

    loop {
        tokio::select! {
            result = &mut verification_task => {
                return match result {
                    Ok(result) => VerificationStep::Finished(result),
                    Err(error) => VerificationStep::TaskFailed(error),
                };
            }
            command = commands.recv() => match command {
                Some(LauncherCommand::Cancel) => {
                    verification_task.abort();
                    return VerificationStep::Cancel;
                }
                Some(LauncherCommand::Play) => {}
                None => {
                    verification_task.abort();
                    return VerificationStep::Stop;
                }
            }
        }
    }
}

async fn install_or_receive_control(
    commands: &mut mpsc::Receiver<LauncherCommand>,
    states: &watch::Sender<LauncherState>,
    installation: &Installation,
    manifest: PackManifest,
    verification: VerificationResult,
) -> InstallStep {
    let root_dir = installation.root_dir.clone();
    let source = Arc::clone(&installation.source);
    let cancellation = InstallCancellation::default();
    let task_cancellation = cancellation.clone();
    let progress_states = states.clone();
    let mut install_task = tokio::spawn(async move {
        install_files(
            root_dir,
            &manifest,
            &verification,
            source.as_ref(),
            &task_cancellation,
            |progress| {
                publish_state(
                    &progress_states,
                    LauncherState::new(LauncherStatus::Downloading, Some(progress.fraction())),
                );
            },
        )
        .await
    });

    loop {
        tokio::select! {
            result = &mut install_task => {
                return match result {
                    Ok(result) => InstallStep::Finished(result),
                    Err(error) => InstallStep::TaskFailed(error),
                };
            }
            command = commands.recv() => match command {
                Some(LauncherCommand::Cancel) => {
                    cancellation.cancel();
                    let _task_result = install_task.await;
                    return InstallStep::Cancel;
                }
                Some(LauncherCommand::Play) => {}
                None => {
                    cancellation.cancel();
                    let _task_result = install_task.await;
                    return InstallStep::Stop;
                }
            }
        }
    }
}

async fn wait_or_receive_control(
    commands: &mut mpsc::Receiver<LauncherCommand>,
    duration: Duration,
) -> WorkflowControl {
    let delay = tokio::time::sleep(duration);
    tokio::pin!(delay);

    loop {
        tokio::select! {
            () = &mut delay => return WorkflowControl::Continue,
            command = commands.recv() => match command {
                Some(LauncherCommand::Cancel) => return WorkflowControl::Cancel,
                Some(LauncherCommand::Play) => {}
                None => return WorkflowControl::Stop,
            }
        }
    }
}

fn is_playable(status: LauncherStatus) -> bool {
    matches!(
        status,
        LauncherStatus::Idle
            | LauncherStatus::Ready
            | LauncherStatus::AuthenticationRequired
            | LauncherStatus::ProfileRequired
            | LauncherStatus::Error
    )
}

fn publish_state(states: &watch::Sender<LauncherState>, mut state: LauncherState) {
    if state.client_config.is_none() {
        state.client_config = states.borrow().client_config.clone();
    }
    if state.stage.is_none()
        && matches!(
            state.status,
            LauncherStatus::Error | LauncherStatus::Downloading
        )
    {
        state.stage = states.borrow().stage;
    }
    if let Some(diagnostic) = &state.diagnostic {
        tracing::error!(reason = %diagnostic.message, "launcher workflow failed");
    }
    if state.status == LauncherStatus::Error && state.mods_progress.is_none() {
        state.mods_progress = states.borrow().mods_progress.clone();
    }
    states.send_replace(state);
}

fn publish_client_config(states: &watch::Sender<LauncherState>, config: ClientConfig) {
    let mut state = states.borrow().clone();
    state.client_config = Some(config);
    state.failure = None;
    state.diagnostic = None;
    states.send_replace(state);
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Write,
        pin::Pin,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::*;
    use crate::http::test_support::{TestResponse, TestServer};
    use crate::{
        game_actor::NeoForgeLauncher,
        minecraft::neoforge::{
            NeoForgeLaunchCancellation, NeoForgeLaunchConfig, NeoForgeLaunchError,
            NeoForgeLaunchProgress, NeoForgeLaunchStage,
        },
        process::test_support::spawn_test_helper,
    };
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    #[tokio::test(start_paused = true)]
    async fn new_core_returns_idle_state_without_progress() {
        let core = LauncherCore::new(&Handle::current());
        let state = core.subscribe_state().borrow().clone();

        assert_eq!(state.status(), LauncherStatus::Idle);
        assert_eq!(state.progress(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn valid_installation_transitions_from_checking_directly_to_ready() {
        let core = LauncherCore::new(&Handle::current());
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await.unwrap();
        wait_for_status(&mut states, LauncherStatus::Checking).await;
        advance_and_yield(CHECKING_DELAY).await;

        let state = wait_for_status(&mut states, LauncherStatus::Ready).await;
        assert_eq!(state.progress(), Some(1.0));
    }

    #[tokio::test(start_paused = true)]
    async fn play_workflow_installs_missing_file_and_reaches_ready() {
        let (installation, _repository, manifest, core) = core_with_missing_manifest();
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await.unwrap();
        wait_for_status(&mut states, LauncherStatus::Checking).await;
        advance_and_yield(CHECKING_DELAY).await;

        assert_eq!(
            wait_for_status(&mut states, LauncherStatus::Downloading)
                .await
                .status(),
            LauncherStatus::Downloading
        );
        let ready = wait_for_status(&mut states, LauncherStatus::Ready).await;

        assert_eq!(ready.progress(), Some(1.0));
        assert!(
            verify_installation(installation.path(), &manifest)
                .unwrap()
                .is_valid()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn download_progress_is_monotonic_and_reaches_one() {
        let (_installation, _repository, _manifest, core) = core_with_missing_manifest();
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await.unwrap();
        wait_for_status(&mut states, LauncherStatus::Checking).await;
        advance_and_yield(CHECKING_DELAY).await;
        wait_for_status(&mut states, LauncherStatus::Downloading).await;

        let mut previous_progress = 0.0_f32;
        loop {
            states.changed().await.unwrap();
            let state = states.borrow_and_update().clone();
            if state.status() == LauncherStatus::Ready {
                assert_eq!(state.progress(), Some(1.0));
                break;
            }
            if state.status() == LauncherStatus::Downloading {
                let progress = state.progress().unwrap();
                assert!((0.0..=1.0).contains(&progress));
                assert!(progress >= previous_progress);
                previous_progress = progress;
            }
        }

        assert!(previous_progress > 0.0);
    }

    #[tokio::test(start_paused = true)]
    async fn installation_finishes_ready_with_complete_progress() {
        let (_installation, _repository, _manifest, core) = core_with_missing_manifest();
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await.unwrap();
        wait_for_status(&mut states, LauncherStatus::Checking).await;
        advance_and_yield(CHECKING_DELAY).await;
        wait_for_status(&mut states, LauncherStatus::Downloading).await;

        let state = wait_for_status(&mut states, LauncherStatus::Ready).await;
        assert_eq!(state.progress(), Some(1.0));
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_restores_state_from_before_workflow() {
        let (_installation, _repository, _manifest, core) = core_with_missing_manifest();
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await.unwrap();
        wait_for_status(&mut states, LauncherStatus::Checking).await;
        core.send_command(LauncherCommand::Cancel).await.unwrap();

        let state = wait_for_status(&mut states, LauncherStatus::Idle).await;
        assert_eq!(state.progress(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_during_installation_removes_partial_file_and_restores_idle() {
        let (installation, _repository, _manifest, core) = core_with_missing_manifest();
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await.unwrap();
        wait_for_status(&mut states, LauncherStatus::Checking).await;
        advance_and_yield(CHECKING_DELAY).await;
        wait_for_status(&mut states, LauncherStatus::Downloading).await;
        core.send_command(LauncherCommand::Cancel).await.unwrap();

        let state = wait_for_status(&mut states, LauncherStatus::Idle).await;
        assert_eq!(state.progress(), None);
        assert!(!installation.path().join("missing.bin").exists());
        assert_eq!(fs::read_dir(installation.path()).unwrap().count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_play_does_not_start_parallel_workflow() {
        let core = LauncherCore::new(&Handle::current());
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await.unwrap();
        wait_for_status(&mut states, LauncherStatus::Checking).await;
        core.send_command(LauncherCommand::Play).await.unwrap();
        advance_and_yield(CHECKING_DELAY).await;
        wait_for_status(&mut states, LauncherStatus::Ready).await;

        advance_and_yield(CHECKING_DELAY).await;
        assert_eq!(states.borrow().status(), LauncherStatus::Ready);
    }

    #[tokio::test]
    async fn remote_play_workflow_fetches_installs_verifies_and_reaches_ready()
    -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let installation = tempfile::tempdir()?;
        let contents = vec![6_u8; 128 * 1024];
        let file = manifest_file("mods/remote.jar", &contents);
        let manifest = PackManifest {
            version: "remote".to_owned(),
            minecraft_version: "1.21.1".to_owned(),
            files: vec![file],
        };
        server.route(
            "/manifest.json",
            TestResponse::ok(manifest.to_json_pretty()?),
        );
        server.route(
            "/files/mods/remote.jar",
            TestResponse::slow(contents, 16 * 1024, Duration::from_millis(10)),
        );
        let config = RemotePackConfig::new(server.url("manifest.json")?, server.url("files/")?)?;
        let core = LauncherCore::new_remote(
            &Handle::current(),
            installation.path(),
            build_http_client()?,
            config,
        )?;
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await?;
        wait_for_status(&mut states, LauncherStatus::Checking).await;
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_status(&mut states, LauncherStatus::Downloading),
        )
        .await?;
        let ready = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_status(&mut states, LauncherStatus::Ready),
        )
        .await?;

        assert_eq!(ready.progress(), Some(1.0));
        assert!(verify_installation(installation.path(), &manifest)?.is_valid());
        Ok(())
    }

    #[tokio::test]
    async fn remote_play_cancel_preserves_target_and_cleans_temporary_file()
    -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let installation = tempfile::tempdir()?;
        let contents = vec![3_u8; 512 * 1024];
        let file = manifest_file("client.jar", &contents);
        let manifest = PackManifest {
            version: "remote".to_owned(),
            minecraft_version: "1.21.1".to_owned(),
            files: vec![file],
        };
        fs::write(installation.path().join("client.jar"), b"old target")?;
        server.route(
            "/manifest.json",
            TestResponse::ok(manifest.to_json_pretty()?),
        );
        server.route(
            "/files/client.jar",
            TestResponse::slow(contents, 8 * 1024, Duration::from_millis(20)),
        );
        let config = RemotePackConfig::new(server.url("manifest.json")?, server.url("files/")?)?;
        let core = LauncherCore::new_remote(
            &Handle::current(),
            installation.path(),
            build_http_client()?,
            config,
        )?;
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await?;
        wait_for_status(&mut states, LauncherStatus::Checking).await;
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_status(&mut states, LauncherStatus::Downloading),
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        core.send_command(LauncherCommand::Cancel).await?;
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_status(&mut states, LauncherStatus::Idle),
        )
        .await?;

        assert_eq!(
            fs::read(installation.path().join("client.jar"))?,
            b"old target"
        );
        assert_eq!(fs::read_dir(installation.path())?.count(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn backend_syncs_mods_then_reports_unsupported_loader() -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let installation = tempfile::tempdir()?;
        server.route(
            "/mods.zip",
            TestResponse::ok(test_zip("mods/backend.jar", b"backend mod")?),
        );
        server.route(
            "/api/client-config",
            TestResponse::ok(
                serde_json::json!({
                    "version": "1.21.1",
                    "loader": "quilt",
                    "loaderVersion": "21.1.248",
                    "javaVersion": 21,
                    "minMemory": 2048,
                    "maxMemory": 4096,
                    "jvmArgs": ["-Dfile.encoding=UTF-8"],
                    "servers": [{"name": "Test", "address": "fallback:25565"}],
                    "modsHash": "backend-hash",
                    "modsURL": server.url("mods.zip")?.as_str(),
                    "serverIP": "server.example:25565"
                })
                .to_string(),
            ),
        );
        let backend = LauncherBackendConfig::new(server.url("")?)?;
        let game = LauncherGameConfig::new(
            build_http_client()?,
            VanillaLaunchConfig {
                telegram_auth: None,
                heap_memory: None,
                jvm_args: Vec::new(),
                version_id: "startup".to_owned(),
                java_major_override: None,
                server_address: None,
                catalog_url: server.url("catalog.json")?,
                assets_base_url: server.url("assets/")?,
                runtime_root: installation.path().to_owned(),
                game_directory: installation.path().to_owned(),
                launcher_name: "test".to_owned(),
                launcher_version: "1".to_owned(),
            },
        );
        let core = LauncherCore::new_backend_with_game(
            &Handle::current(),
            installation.path(),
            build_http_client()?,
            backend,
            game,
        );
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await?;
        let state = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_status(&mut states, LauncherStatus::Error),
        )
        .await?;

        assert_eq!(
            state.failure(),
            Some(&LauncherFailure::UnsupportedLoader("quilt".to_owned()))
        );
        let mods = state
            .mods_progress()
            .expect("mods status reaches launcher snapshots");
        assert_eq!(mods.stage, ModsSyncStage::Complete);
        assert_eq!(mods.files_completed, 1);
        assert_eq!(mods.files_total, Some(1));
        assert_eq!(
            state.client_config().map(|config| config.version.as_str()),
            Some("1.21.1")
        );
        assert_eq!(server.request_count("/api/client-config"), 1);
        assert_eq!(server.request_count("/mods.zip"), 1);
        assert_eq!(
            fs::read(installation.path().join("mods/backend.jar"))?,
            b"backend mod"
        );
        let state_json: serde_json::Value =
            serde_json::from_slice(&fs::read(installation.path().join("state.json"))?)?;
        assert_eq!(state_json["modsHash"], "backend-hash");
        Ok(())
    }

    #[test]
    fn backend_loader_selection_preserves_vanilla_adds_neoforge_and_rejects_unknown() {
        assert_eq!(
            BackendLoader::parse("vanilla"),
            Some(BackendLoader::Vanilla)
        );
        assert_eq!(
            BackendLoader::parse("NeoForge"),
            Some(BackendLoader::NeoForge)
        );
        assert_eq!(BackendLoader::parse("fabric"), None);
    }

    struct SyntheticNeoForgeLauncher {
        calls: Arc<AtomicUsize>,
        heaps: Arc<Mutex<Vec<HeapMemory>>>,
    }

    impl NeoForgeLauncher for SyntheticNeoForgeLauncher {
        fn launch(
            &self,
            _client: reqwest::Client,
            identity: MinecraftLaunchIdentity,
            config: NeoForgeLaunchConfig,
            _cancellation: NeoForgeLaunchCancellation,
            on_progress: Arc<dyn Fn(NeoForgeLaunchProgress) + Send + Sync>,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<ProcessHandle, NeoForgeLaunchError>>
                    + Send
                    + 'static,
            >,
        > {
            self.calls.fetch_add(1, Ordering::AcqRel);
            self.heaps
                .lock()
                .unwrap()
                .push(HeapMemory::new(config.min_memory, config.max_memory).unwrap());
            Box::pin(async move {
                assert_eq!(config.minecraft_version, "1.21.1");
                assert_eq!(config.loader_version, "21.1.248");
                assert_eq!(config.java_major, 21);
                assert_eq!(config.jvm_args, ["-Dfile.encoding=UTF-8"]);
                assert_eq!(
                    config.vanilla.server_address.as_deref(),
                    Some("server.example:25565")
                );
                assert_eq!(identity.username(), "NeoPlayer");
                assert_eq!(identity.access_token(), "0");
                assert_eq!(identity.user_type(), "legacy");
                assert!(
                    config
                        .vanilla
                        .game_directory
                        .join("mods/existing.jar")
                        .is_file()
                );
                on_progress(NeoForgeLaunchProgress {
                    stage: NeoForgeLaunchStage::PreparingRuntime,
                    progress: None,
                });
                on_progress(NeoForgeLaunchProgress {
                    stage: NeoForgeLaunchStage::Launching,
                    progress: None,
                });
                spawn_test_helper("delayed-capture", &config.vanilla.game_directory)
                    .map_err(NeoForgeLaunchError::Process)
            })
        }
    }

    #[tokio::test]
    async fn full_synthetic_backend_neoforge_play_reuses_mods_and_runs_helper()
    -> Result<(), Box<dyn Error>> {
        let server = TestServer::start().await?;
        let installation = tempfile::tempdir()?;
        fs::create_dir_all(installation.path().join("mods"))?;
        fs::write(
            installation.path().join("mods/existing.jar"),
            b"already synchronized",
        )?;
        fs::write(
            installation.path().join("state.json"),
            r#"{"modsHash":"backend-hash"}"#,
        )?;
        server.route(
            "/api/client-config",
            TestResponse::ok(
                serde_json::json!({
                    "version": "1.21.1",
                    "loader": "neoforge",
                    "loaderVersion": "21.1.248",
                    "javaVersion": 21,
                    "minMemory": 2048,
                    "maxMemory": 4096,
                    "jvmArgs": ["-Dfile.encoding=UTF-8"],
                    "servers": [],
                    "modsHash": "backend-hash",
                    "modsURL": server.url("mods.zip")?.as_str(),
                    "serverIP": "server.example:25565"
                })
                .to_string(),
            ),
        );
        let backend = LauncherBackendConfig::new(server.url("")?)?;
        let calls = Arc::new(AtomicUsize::new(0));
        let heaps = Arc::new(Mutex::new(Vec::new()));
        let vanilla = VanillaLaunchConfig {
            telegram_auth: None,
            heap_memory: None,
            jvm_args: Vec::new(),
            version_id: "startup".to_owned(),
            java_major_override: None,
            server_address: None,
            catalog_url: server.url("catalog.json")?,
            assets_base_url: server.url("assets/")?,
            runtime_root: installation.path().to_owned(),
            game_directory: installation.path().to_owned(),
            launcher_name: "test".to_owned(),
            launcher_version: "1".to_owned(),
        };
        let (java_setup, mut java_requests) = crate::java_install::JavaSetup::channel();
        let game = LauncherGameConfig::with_neoforge_launcher(
            build_http_client()?,
            vanilla,
            Arc::new(SyntheticNeoForgeLauncher {
                calls: Arc::clone(&calls),
                heaps: Arc::clone(&heaps),
            }),
        );
        let game = game.with_java_setup(java_setup);
        let java_root = installation.path().to_owned();
        let java_calls = Arc::clone(&calls);
        let setup_task = tokio::spawn(async move {
            for index in 0..2 {
                let request = java_requests.recv().await.unwrap();
                assert_eq!(request.required_major, 21);
                assert!(java_root.join("state.json").exists());
                assert_eq!(
                    fs::read(java_root.join("mods/existing.jar")).unwrap(),
                    b"already synchronized"
                );
                assert_eq!(
                    java_calls.load(Ordering::Acquire),
                    index,
                    "Minecraft must wait for Java readiness"
                );
                request
                    .reply
                    .send(Ok(crate::java_install::JavaSetupOutcome::Ready))
                    .unwrap();
            }
        });
        let core = LauncherCore::new_backend_with_game(
            &Handle::current(),
            installation.path(),
            build_http_client()?,
            backend,
            game,
        );
        core.set_offline_profile(OfflineProfile::new("NeoPlayer")?);
        core.set_launch_options(UserLaunchOptions {
            max_memory_mib: Some(6144),
        });
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await?;
        wait_for_status(&mut states, LauncherStatus::Checking).await;
        // A change while the workflow is running belongs to the following Play.
        core.set_launch_options(UserLaunchOptions {
            max_memory_mib: Some(1536),
        });
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_status(&mut states, LauncherStatus::Running),
        )
        .await?;
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_status(&mut states, LauncherStatus::Ready),
        )
        .await?;

        server.route("/api/client-config", TestResponse::ok(serde_json::json!({
            "version":"1.21.1", "loader":"neoforge", "loaderVersion":"21.1.248", "javaVersion":21,
            "minMemory":4096, "maxMemory":8192, "jvmArgs":["-Dfile.encoding=UTF-8"], "servers":[],
            "modsHash":"backend-hash", "modsURL":server.url("mods.zip")?.as_str(), "serverIP":"server.example:25565"
        }).to_string()));
        core.send_command(LauncherCommand::Play).await?;
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_status(&mut states, LauncherStatus::Running),
        )
        .await?;
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_status(&mut states, LauncherStatus::Ready),
        )
        .await?;
        assert_eq!(
            *heaps.lock().unwrap(),
            [HeapMemory::new(2048, 6144)?, HeapMemory::new(1536, 1536)?]
        );
        setup_task.await?;
        assert_eq!(calls.load(Ordering::Acquire), 2);
        assert_eq!(server.request_count("/api/client-config"), 2);
        assert_eq!(server.request_count("/mods.zip"), 0);
        assert_eq!(
            fs::read(installation.path().join("mods/existing.jar"))?,
            b"already synchronized"
        );
        Ok(())
    }

    fn core_with_missing_manifest() -> (TempDir, TempDir, PackManifest, LauncherCore) {
        let installation = tempfile::tempdir().unwrap();
        let repository = tempfile::tempdir().unwrap();
        let contents = vec![7_u8; 128 * 1024 + 17];
        let mut hasher = Sha256::new();
        hasher.update(&contents);
        let manifest = PackManifest {
            version: "test".to_owned(),
            minecraft_version: "test".to_owned(),
            files: vec![ManifestFile {
                path: "missing.bin".to_owned(),
                sha256: format!("{:x}", hasher.finalize()),
                size: contents.len() as u64,
            }],
        };
        fs::write(repository.path().join("missing.bin"), contents).unwrap();
        let source = DelayedLocalSource(LocalFileSource::new(repository.path()));
        let core = LauncherCore::new_with_file_source(
            &Handle::current(),
            installation.path().to_owned(),
            manifest.clone(),
            source,
        );

        (installation, repository, manifest, core)
    }

    fn manifest_file(path: &str, contents: &[u8]) -> ManifestFile {
        let mut hasher = Sha256::new();
        hasher.update(contents);
        ManifestFile {
            path: path.to_owned(),
            sha256: format!("{:x}", hasher.finalize()),
            size: contents.len() as u64,
        }
    }

    fn test_zip(path: &str, contents: &[u8]) -> Result<Vec<u8>, zip::result::ZipError> {
        let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer.start_file(
            path,
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )?;
        writer.write_all(contents)?;
        Ok(writer.finish()?.into_inner())
    }

    struct DelayedLocalSource(LocalFileSource);

    impl FileSource for DelayedLocalSource {
        fn fetch<'a>(
            &'a self,
            file: &'a ManifestFile,
        ) -> SourceFuture<'a, Result<Box<dyn FileChunkStream>, FileSourceError>> {
            Box::pin(async move {
                Ok(Box::new(DelayedReader(self.0.fetch(file).await?)) as Box<dyn FileChunkStream>)
            })
        }
    }

    struct DelayedReader(Box<dyn FileChunkStream>);

    impl FileChunkStream for DelayedReader {
        fn next_chunk(&mut self) -> SourceFuture<'_, Result<Option<Vec<u8>>, FileSourceError>> {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(25)).await;
                self.0.next_chunk().await
            })
        }
    }

    async fn wait_for_status(
        states: &mut watch::Receiver<LauncherState>,
        expected: LauncherStatus,
    ) -> LauncherState {
        loop {
            let state = states.borrow_and_update().clone();
            if state.status() == expected {
                return state;
            }

            states.changed().await.unwrap();
        }
    }

    async fn advance_and_yield(duration: Duration) {
        tokio::time::advance(duration).await;
        tokio::task::yield_now().await;
    }
}
