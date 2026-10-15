use std::{fmt, future::Future, pin::Pin, sync::Arc};

use tokio::sync::{mpsc, watch};

use crate::{
    ClientConfig, HeapMemory, InvalidHeapMemory, LauncherCommand, LauncherDiagnostic,
    LauncherFailure, LauncherStage, LauncherState, LauncherStatus, MinecraftLaunchIdentity,
    ProcessEvent, ProcessHandle, UserLaunchOptions, VanillaLaunchCancellation, VanillaLaunchConfig,
    VanillaLaunchError, VanillaLaunchProgress, VanillaLaunchStage, WorkflowControl,
    diagnostics::LogTail,
    launch_vanilla_with_identity,
    minecraft::neoforge::{
        NeoForgeLaunchCancellation, NeoForgeLaunchConfig, NeoForgeLaunchError,
        NeoForgeLaunchProgress, NeoForgeLaunchStage, launch_neoforge_with_identity,
    },
    publish_state,
};

type LaunchFuture =
    Pin<Box<dyn Future<Output = Result<ProcessHandle, VanillaLaunchError>> + Send + 'static>>;
type ProgressCallback = Arc<dyn Fn(VanillaLaunchProgress) + Send + Sync>;
type NeoForgeLaunchFuture =
    Pin<Box<dyn Future<Output = Result<ProcessHandle, NeoForgeLaunchError>> + Send + 'static>>;
type NeoForgeProgressCallback = Arc<dyn Fn(NeoForgeLaunchProgress) + Send + Sync>;

pub(crate) trait VanillaLauncher: Send + Sync {
    fn launch(
        &self,
        client: reqwest::Client,
        identity: MinecraftLaunchIdentity,
        config: VanillaLaunchConfig,
        cancellation: VanillaLaunchCancellation,
        on_progress: ProgressCallback,
    ) -> LaunchFuture;
}

#[derive(Default)]
struct ProductionVanillaLauncher;

impl VanillaLauncher for ProductionVanillaLauncher {
    fn launch(
        &self,
        client: reqwest::Client,
        identity: MinecraftLaunchIdentity,
        config: VanillaLaunchConfig,
        cancellation: VanillaLaunchCancellation,
        on_progress: ProgressCallback,
    ) -> LaunchFuture {
        Box::pin(async move {
            launch_vanilla_with_identity(
                &client,
                &identity,
                &config,
                &cancellation,
                move |progress| on_progress(progress),
            )
            .await
        })
    }
}

pub(crate) trait NeoForgeLauncher: Send + Sync {
    fn launch(
        &self,
        client: reqwest::Client,
        identity: MinecraftLaunchIdentity,
        config: NeoForgeLaunchConfig,
        cancellation: NeoForgeLaunchCancellation,
        on_progress: NeoForgeProgressCallback,
    ) -> NeoForgeLaunchFuture;
}

#[derive(Default)]
struct ProductionNeoForgeLauncher;

impl NeoForgeLauncher for ProductionNeoForgeLauncher {
    fn launch(
        &self,
        client: reqwest::Client,
        identity: MinecraftLaunchIdentity,
        config: NeoForgeLaunchConfig,
        cancellation: NeoForgeLaunchCancellation,
        on_progress: NeoForgeProgressCallback,
    ) -> NeoForgeLaunchFuture {
        Box::pin(async move {
            launch_neoforge_with_identity(
                &client,
                &identity,
                &config,
                &cancellation,
                move |progress| on_progress(progress),
            )
            .await
        })
    }
}

#[derive(Clone)]
pub struct LauncherGameConfig {
    pub client: reqwest::Client,
    pub vanilla: VanillaLaunchConfig,
    launcher: Arc<dyn VanillaLauncher>,
    neoforge_launcher: Arc<dyn NeoForgeLauncher>,
}

impl LauncherGameConfig {
    pub fn new(client: reqwest::Client, vanilla: VanillaLaunchConfig) -> Self {
        Self {
            client,
            vanilla,
            launcher: Arc::new(ProductionVanillaLauncher),
            neoforge_launcher: Arc::new(ProductionNeoForgeLauncher),
        }
    }

    pub(crate) fn neoforge_for_client_config(&self, config: &ClientConfig) -> NeoForgeLaunchConfig {
        let mut configured = NeoForgeLaunchConfig::from_client_config(self.vanilla.clone(), config);
        if let Some(heap) = self.vanilla.heap_memory {
            configured.min_memory = heap.min_memory_mib;
            configured.max_memory = heap.max_memory_mib;
        }
        configured
    }

    pub(crate) fn for_client_config(
        &self,
        config: &ClientConfig,
        options: UserLaunchOptions,
    ) -> Result<Self, InvalidHeapMemory> {
        let mut configured = self.clone();
        configured.vanilla.heap_memory = Some(HeapMemory::resolve(
            config.min_memory,
            config.max_memory,
            options,
        )?);
        configured.vanilla.jvm_args = config.jvm_args.clone();
        configured.vanilla.version_id = config.version.clone();
        configured.vanilla.java_major_override = Some(config.java_version);
        configured.vanilla.server_address = config.server_address().map(str::to_owned);
        Ok(configured)
    }

    pub(crate) fn with_launch_options(
        &self,
        options: UserLaunchOptions,
    ) -> Result<Self, InvalidHeapMemory> {
        let mut configured = self.clone();
        if let Some(maximum) = options.max_memory_mib {
            let minimum = configured
                .vanilla
                .heap_memory
                .map_or(1024, |heap| heap.min_memory_mib);
            configured.vanilla.heap_memory = Some(HeapMemory::new(minimum.min(maximum), maximum)?);
        }
        Ok(configured)
    }

    #[cfg(test)]
    pub(crate) fn with_launcher(
        client: reqwest::Client,
        vanilla: VanillaLaunchConfig,
        launcher: Arc<dyn VanillaLauncher>,
    ) -> Self {
        Self {
            client,
            vanilla,
            launcher,
            neoforge_launcher: Arc::new(ProductionNeoForgeLauncher),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_neoforge_launcher(
        client: reqwest::Client,
        vanilla: VanillaLaunchConfig,
        neoforge_launcher: Arc<dyn NeoForgeLauncher>,
    ) -> Self {
        Self {
            client,
            vanilla,
            launcher: Arc::new(ProductionVanillaLauncher),
            neoforge_launcher,
        }
    }
}

impl fmt::Debug for LauncherGameConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LauncherGameConfig")
            .field("client", &self.client)
            .field("vanilla", &self.vanilla)
            .field("launcher", &"<internal>")
            .field("neoforge_launcher", &"<internal>")
            .finish()
    }
}

pub(super) async fn run_game_workflow(
    commands: &mut mpsc::Receiver<LauncherCommand>,
    states: &watch::Sender<LauncherState>,
    identity: MinecraftLaunchIdentity,
    config: &LauncherGameConfig,
) -> WorkflowControl {
    let cancellation = VanillaLaunchCancellation::default();
    let progress_states = states.clone();
    let on_progress: ProgressCallback = Arc::new(move |progress| {
        publish_state(
            &progress_states,
            LauncherState::new(
                LauncherStatus::Launching,
                if progress.stage == VanillaLaunchStage::AcquiringRuntime {
                    progress.progress
                } else {
                    None
                },
            )
            .at_stage(map_stage(progress.stage)),
        );
    });

    let client = config.client.clone();
    let vanilla = config.vanilla.clone();
    let launcher = Arc::clone(&config.launcher);
    let task_cancellation = cancellation.clone();
    let launch_task = tokio::spawn(async move {
        launcher
            .launch(client, identity, vanilla, task_cancellation, on_progress)
            .await
            .map_err(GameLaunchError::Vanilla)
    });
    drive_launch_task(commands, states, launch_task, move || cancellation.cancel()).await
}

pub(super) async fn run_neoforge_game_workflow(
    commands: &mut mpsc::Receiver<LauncherCommand>,
    states: &watch::Sender<LauncherState>,
    identity: MinecraftLaunchIdentity,
    game: &LauncherGameConfig,
    config: NeoForgeLaunchConfig,
) -> WorkflowControl {
    let cancellation = NeoForgeLaunchCancellation::default();
    let progress_states = states.clone();
    let on_progress: NeoForgeProgressCallback = Arc::new(move |progress| {
        publish_state(
            &progress_states,
            LauncherState::new(
                LauncherStatus::Launching,
                if matches!(
                    progress.stage,
                    NeoForgeLaunchStage::AcquiringVanilla | NeoForgeLaunchStage::AcquiringNeoForge
                ) {
                    progress.progress
                } else {
                    None
                },
            )
            .at_stage(map_neoforge_stage(progress.stage)),
        );
    });
    let client = game.client.clone();
    let launcher = Arc::clone(&game.neoforge_launcher);
    let task_cancellation = cancellation.clone();
    let launch_task = tokio::spawn(async move {
        launcher
            .launch(client, identity, config, task_cancellation, on_progress)
            .await
            .map_err(GameLaunchError::NeoForge)
    });
    drive_launch_task(commands, states, launch_task, move || cancellation.cancel()).await
}

#[derive(Debug)]
enum GameLaunchError {
    Vanilla(VanillaLaunchError),
    NeoForge(NeoForgeLaunchError),
}

impl GameLaunchError {
    fn is_cancelled(&self) -> bool {
        matches!(
            self,
            Self::Vanilla(VanillaLaunchError::Cancelled)
                | Self::NeoForge(NeoForgeLaunchError::Cancelled)
        )
    }
}

impl GameLaunchError {
    fn failure(&self) -> LauncherFailure {
        match self {
            Self::Vanilla(VanillaLaunchError::Java(
                crate::JavaError::JavaNotFound { required_major }
                | crate::JavaError::VersionMismatch { required_major, .. },
            ))
            | Self::NeoForge(NeoForgeLaunchError::Java(
                crate::JavaError::JavaNotFound { required_major }
                | crate::JavaError::VersionMismatch { required_major, .. },
            )) => LauncherFailure::Java {
                required_major: *required_major,
            },
            _ => LauncherFailure::GameLaunch,
        }
    }
}

impl fmt::Display for GameLaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vanilla(error) => error.fmt(formatter),
            Self::NeoForge(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for GameLaunchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Vanilla(error) => Some(error),
            Self::NeoForge(error) => Some(error),
        }
    }
}

async fn drive_launch_task<C>(
    commands: &mut mpsc::Receiver<LauncherCommand>,
    states: &watch::Sender<LauncherState>,
    mut launch_task: tokio::task::JoinHandle<Result<ProcessHandle, GameLaunchError>>,
    cancel: C,
) -> WorkflowControl
where
    C: Fn(),
{
    loop {
        tokio::select! {
            result = &mut launch_task => {
                return match result {
                    Ok(Ok(process)) => monitor_process(commands, states, process).await,
                    Ok(Err(error)) if error.is_cancelled() => WorkflowControl::Cancel,
                    Ok(Err(error)) => {
                        tracing::error!(%error, "game launch failed");
                        publish_state(states, LauncherState::with_diagnostic(error.failure(), LauncherDiagnostic::from_error(&error)));
                        WorkflowControl::Continue
                    }
                    Err(error) => {
                        tracing::error!(%error, "game launch task failed");
                        publish_state(states, LauncherState::with_diagnostic(LauncherFailure::Internal, LauncherDiagnostic::from_error(&error)));
                        WorkflowControl::Continue
                    }
                };
            }
            command = commands.recv() => match command {
                Some(LauncherCommand::Cancel) => {
                    cancel();
                    if let Ok(Ok(process)) = launch_task.await {
                        cancel_and_reap(process).await;
                    }
                    return WorkflowControl::Cancel;
                }
                Some(LauncherCommand::Play) => {}
                None => {
                    cancel();
                    if let Ok(Ok(process)) = launch_task.await {
                        cancel_and_reap(process).await;
                    }
                    return WorkflowControl::Stop;
                }
            }
        }
    }
}

const fn map_stage(stage: VanillaLaunchStage) -> LauncherStage {
    match stage {
        VanillaLaunchStage::ResolvingVersion => LauncherStage::ResolvingVersion,
        VanillaLaunchStage::CheckingJava => LauncherStage::CheckingJava,
        VanillaLaunchStage::AcquiringRuntime => LauncherStage::AcquiringVanilla,
        VanillaLaunchStage::PreparingRuntime => LauncherStage::PreparingRuntime,
        VanillaLaunchStage::Launching => LauncherStage::StartingProcess,
    }
}

const fn map_neoforge_stage(stage: NeoForgeLaunchStage) -> LauncherStage {
    match stage {
        NeoForgeLaunchStage::ResolvingVersion => LauncherStage::ResolvingVersion,
        NeoForgeLaunchStage::CheckingJava => LauncherStage::CheckingJava,
        NeoForgeLaunchStage::AcquiringVanilla => LauncherStage::AcquiringVanilla,
        NeoForgeLaunchStage::AcquiringNeoForge => LauncherStage::AcquiringNeoForge,
        NeoForgeLaunchStage::PreparingRuntime => LauncherStage::PreparingRuntime,
        NeoForgeLaunchStage::Launching => LauncherStage::StartingProcess,
    }
}

async fn monitor_process(
    commands: &mut mpsc::Receiver<LauncherCommand>,
    states: &watch::Sender<LauncherState>,
    mut process: ProcessHandle,
) -> WorkflowControl {
    tracing::info!("running");
    publish_state(
        states,
        LauncherState::new(LauncherStatus::Running, None).at_stage(LauncherStage::Running),
    );
    let mut cancel_requested = false;
    let mut stop_requested = false;
    let mut log_tail = LogTail::default();

    loop {
        tokio::select! {
            event = process.next_event() => {
                match event {
                    Some(ProcessEvent::Stdout(bytes)) => {
                        log_tail.push("stdout", &bytes);
                        tracing::info!(target: "minecraft_stdout", "{}", String::from_utf8_lossy(&bytes).trim_end());
                    }
                    Some(ProcessEvent::Stderr(bytes)) => {
                        log_tail.push("stderr", &bytes);
                        tracing::warn!(target: "minecraft_stderr", "{}", String::from_utf8_lossy(&bytes).trim_end());
                    }
                    None => break,
                }
            }
            command = commands.recv(), if !stop_requested => match command {
                Some(LauncherCommand::Cancel) => {
                    cancel_requested = true;
                    process.cancel();
                }
                Some(LauncherCommand::Play) => {}
                None => {
                    stop_requested = true;
                    process.cancel();
                }
            }
        }
    }

    let result = process.wait().await;
    tracing::info!("exited");
    if stop_requested {
        return WorkflowControl::Stop;
    }
    if cancel_requested || matches!(&result, Ok(exit) if exit.cancelled) {
        publish_state(states, LauncherState::new(LauncherStatus::Ready, Some(1.0)));
        return WorkflowControl::Continue;
    }

    match result {
        Ok(exit) if exit.status.success() => {
            publish_state(states, LauncherState::new(LauncherStatus::Ready, Some(1.0)));
        }
        Ok(exit) => {
            tracing::error!(status = %exit.status, "Minecraft exited with an error");
            let mut diagnostic = LauncherDiagnostic::message(format!(
                "Minecraft завершился с ошибкой: {}",
                exit.status
            ));
            diagnostic.log_tail = log_tail.into_string();
            publish_state(
                states,
                LauncherState::with_diagnostic(
                    LauncherFailure::GameExit {
                        code: exit.status.code(),
                    },
                    diagnostic,
                ),
            );
        }
        Err(error) => {
            tracing::error!(%error, "failed to monitor Minecraft");
            let mut diagnostic = LauncherDiagnostic::from_error(&error);
            diagnostic.log_tail = log_tail.into_string();
            publish_state(
                states,
                LauncherState::with_diagnostic(LauncherFailure::GameLaunch, diagnostic),
            );
        }
    }
    WorkflowControl::Continue
}

async fn cancel_and_reap(mut process: ProcessHandle) {
    process.cancel();
    while process.next_event().await.is_some() {}
    let _result = process.wait().await;
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::{
        JavaRuntime, LauncherCore, ManifestFile, MinecraftRuntimeLayout, OfflineProfile,
        PackManifest,
        http::test_support::TestServer,
        minecraft::{FixtureMode, SyntheticFixture, launch_vanilla_with_dependencies},
        process::test_support::spawn_test_helper,
    };

    #[derive(Clone, Copy)]
    enum FakeMode {
        Success,
        ExitFailure,
        Running,
        AcquisitionUntilCancelled,
        BeforeSpawnUntilCancelled,
        MissingJava,
    }

    struct FakeLauncher {
        mode: FakeMode,
        working_directory: PathBuf,
        calls: Arc<AtomicUsize>,
        precondition_path: Option<PathBuf>,
        precondition_met: Arc<AtomicBool>,
    }

    struct FullOrchestrationLauncher;

    impl VanillaLauncher for FullOrchestrationLauncher {
        fn launch(
            &self,
            client: reqwest::Client,
            identity: MinecraftLaunchIdentity,
            config: VanillaLaunchConfig,
            cancellation: VanillaLaunchCancellation,
            on_progress: ProgressCallback,
        ) -> LaunchFuture {
            Box::pin(async move {
                let executable = std::env::current_exe().map_err(|source| {
                    VanillaLaunchError::Process(crate::ProcessError::Spawn {
                        executable_path: "current-test-executable".into(),
                        source,
                    })
                })?;
                launch_vanilla_with_dependencies(
                    &client,
                    &identity,
                    &config,
                    &cancellation,
                    move |progress| on_progress(progress),
                    move |major| {
                        let executable = executable.clone();
                        async move {
                            Ok(JavaRuntime {
                                executable_path: executable,
                                major_version: major,
                            })
                        }
                    },
                    move |spec| spawn_test_helper("delayed-capture", &spec.working_directory),
                )
                .await
            })
        }
    }

    impl VanillaLauncher for FakeLauncher {
        fn launch(
            &self,
            _client: reqwest::Client,
            _identity: MinecraftLaunchIdentity,
            _config: VanillaLaunchConfig,
            cancellation: VanillaLaunchCancellation,
            on_progress: ProgressCallback,
        ) -> LaunchFuture {
            self.calls.fetch_add(1, Ordering::AcqRel);
            if let Some(path) = &self.precondition_path {
                self.precondition_met
                    .store(path.is_file(), Ordering::Release);
            }
            let mode = self.mode;
            let working_directory = self.working_directory.clone();
            Box::pin(async move {
                on_progress(VanillaLaunchProgress {
                    stage: VanillaLaunchStage::ResolvingVersion,
                    progress: None,
                });
                match mode {
                    FakeMode::AcquisitionUntilCancelled => {
                        on_progress(VanillaLaunchProgress {
                            stage: VanillaLaunchStage::AcquiringRuntime,
                            progress: Some(0.25),
                        });
                        cancellation.cancelled().await;
                        Err(VanillaLaunchError::Cancelled)
                    }
                    FakeMode::BeforeSpawnUntilCancelled => {
                        on_progress(VanillaLaunchProgress {
                            stage: VanillaLaunchStage::Launching,
                            progress: None,
                        });
                        cancellation.cancelled().await;
                        Err(VanillaLaunchError::Cancelled)
                    }
                    FakeMode::Success => {
                        on_progress(VanillaLaunchProgress {
                            stage: VanillaLaunchStage::Launching,
                            progress: None,
                        });
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        spawn_test_helper("delayed-capture", &working_directory)
                            .map_err(VanillaLaunchError::Process)
                    }
                    FakeMode::ExitFailure => {
                        on_progress(VanillaLaunchProgress {
                            stage: VanillaLaunchStage::Launching,
                            progress: None,
                        });
                        spawn_test_helper("crash-with-output", &working_directory)
                            .map_err(VanillaLaunchError::Process)
                    }
                    FakeMode::MissingJava => {
                        on_progress(VanillaLaunchProgress {
                            stage: VanillaLaunchStage::CheckingJava,
                            progress: None,
                        });
                        Err(VanillaLaunchError::Java(crate::JavaError::JavaNotFound {
                            required_major: 21,
                        }))
                    }
                    FakeMode::Running => {
                        on_progress(VanillaLaunchProgress {
                            stage: VanillaLaunchStage::Launching,
                            progress: None,
                        });
                        spawn_test_helper("sleep", &working_directory)
                            .map_err(VanillaLaunchError::Process)
                    }
                }
            })
        }
    }

    #[tokio::test]
    async fn play_without_profile_requires_profile() {
        let temp = tempfile::tempdir().unwrap();
        let (core, _calls, _) = fake_core(temp.path(), FakeMode::Success, None);
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await.unwrap();

        wait_for_status(&mut states, LauncherStatus::ProfileRequired).await;
    }

    #[tokio::test]
    async fn set_offline_profile_allows_play_and_clean_exit_returns_ready() {
        let temp = tempfile::tempdir().unwrap();
        let (core, calls, _) = fake_core(temp.path(), FakeMode::Success, None);
        core.set_offline_profile(profile());
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await.unwrap();
        wait_for_status(&mut states, LauncherStatus::Launching).await;
        wait_for_status(&mut states, LauncherStatus::Running).await;
        wait_for_status(&mut states, LauncherStatus::Ready).await;

        assert_eq!(calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn custom_pack_repair_precedes_vanilla_launcher() {
        let installation = tempfile::tempdir().unwrap();
        let repository = tempfile::tempdir().unwrap();
        let contents = b"repaired custom pack".to_vec();
        let manifest = manifest_for("mods/custom.jar", &contents);
        let source = crate::LocalFileSource::new(repository.path());
        fs::create_dir_all(repository.path().join("mods")).unwrap();
        fs::write(repository.path().join("mods/custom.jar"), contents).unwrap();
        let repaired_path = installation.path().join("mods/custom.jar");
        let (game, calls, precondition_met) = fake_game_config(
            installation.path(),
            FakeMode::Success,
            Some(repaired_path.clone()),
        );
        let core = LauncherCore::new_with_file_source_and_game(
            &tokio::runtime::Handle::current(),
            installation.path(),
            manifest,
            source,
            game,
        );
        core.set_offline_profile(profile());
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await.unwrap();
        wait_for_status(&mut states, LauncherStatus::Downloading).await;
        wait_for_status(&mut states, LauncherStatus::Ready).await;

        assert_eq!(fs::read(repaired_path).unwrap(), b"repaired custom pack");
        assert!(precondition_met.load(Ordering::Acquire));
        assert_eq!(calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn nonzero_process_exit_becomes_error() {
        let temp = tempfile::tempdir().unwrap();
        let (core, _, _) = fake_core(temp.path(), FakeMode::ExitFailure, None);
        core.set_offline_profile(profile());
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await.unwrap();

        let state = wait_for_status(&mut states, LauncherStatus::Error).await;
        assert_eq!(
            state.failure(),
            Some(&LauncherFailure::GameExit { code: Some(23) })
        );
        assert_eq!(state.stage(), Some(LauncherStage::Running));
        let diagnostic = state.diagnostic().unwrap();
        assert!(diagnostic.log_tail.contains("portable stdout marker"));
        assert!(diagnostic.log_tail.contains("missing mod dependency"));
        assert!(diagnostic.message.contains("23"));
    }

    #[tokio::test]
    async fn repeated_play_while_running_does_not_spawn_twice() {
        let temp = tempfile::tempdir().unwrap();
        let (core, calls, _) = fake_core(temp.path(), FakeMode::Running, None);
        core.set_offline_profile(profile());
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await.unwrap();
        wait_for_status(&mut states, LauncherStatus::Running).await;
        core.send_command(LauncherCommand::Play).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(calls.load(Ordering::Acquire), 1);

        core.send_command(LauncherCommand::Cancel).await.unwrap();
        wait_for_status(&mut states, LauncherStatus::Ready).await;
        assert_eq!(calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn missing_java_keeps_the_stage_cause_and_allows_retry() {
        let temp = tempfile::tempdir().unwrap();
        let (core, calls, _) = fake_core(temp.path(), FakeMode::MissingJava, None);
        core.set_offline_profile(profile());
        let mut states = core.subscribe_state();
        for expected_calls in 1..=2 {
            core.send_command(LauncherCommand::Play).await.unwrap();
            wait_for_status(&mut states, LauncherStatus::Checking).await;
            let state = wait_for_status(&mut states, LauncherStatus::Error).await;
            assert_eq!(
                state.failure(),
                Some(&LauncherFailure::Java { required_major: 21 })
            );
            assert_eq!(state.stage(), Some(LauncherStage::CheckingJava));
            assert!(
                state
                    .diagnostic()
                    .unwrap()
                    .message
                    .contains("Java 21 was not found")
            );
            assert_eq!(calls.load(Ordering::Acquire), expected_calls);
        }
    }

    #[tokio::test]
    async fn cancel_during_acquisition_returns_to_previous_state() {
        let temp = tempfile::tempdir().unwrap();
        let (core, calls, _) = fake_core(temp.path(), FakeMode::AcquisitionUntilCancelled, None);
        core.set_offline_profile(profile());
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await.unwrap();
        let launching = wait_for_status(&mut states, LauncherStatus::Launching).await;
        assert_eq!(launching.progress(), Some(0.25));
        core.send_command(LauncherCommand::Cancel).await.unwrap();

        wait_for_status(&mut states, LauncherStatus::Idle).await;
        assert_eq!(calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn cancel_immediately_before_spawn_does_not_start_process() {
        let temp = tempfile::tempdir().unwrap();
        let (core, calls, _) = fake_core(temp.path(), FakeMode::BeforeSpawnUntilCancelled, None);
        core.set_offline_profile(profile());
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await.unwrap();
        wait_for_status(&mut states, LauncherStatus::Launching).await;
        core.send_command(LauncherCommand::Cancel).await.unwrap();

        wait_for_status(&mut states, LauncherStatus::Idle).await;
        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert_ne!(states.borrow().status(), LauncherStatus::Running);
    }

    #[tokio::test]
    async fn cancel_running_process_kills_reaps_and_returns_ready() {
        let temp = tempfile::tempdir().unwrap();
        let (core, calls, _) = fake_core(temp.path(), FakeMode::Running, None);
        core.set_offline_profile(profile());
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await.unwrap();
        wait_for_status(&mut states, LauncherStatus::Running).await;
        core.send_command(LauncherCommand::Cancel).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_status(&mut states, LauncherStatus::Ready),
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn clear_offline_profile_requires_profile_on_next_play() {
        let temp = tempfile::tempdir().unwrap();
        let (core, _, _) = fake_core(temp.path(), FakeMode::Success, None);
        core.set_offline_profile(profile());
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await.unwrap();
        wait_for_status(&mut states, LauncherStatus::Ready).await;
        core.clear_offline_profile();
        core.send_command(LauncherCommand::Play).await.unwrap();

        wait_for_status(&mut states, LauncherStatus::ProfileRequired).await;
    }

    #[tokio::test]
    async fn full_synthetic_play_runs_orchestrator_and_returns_ready()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = TestServer::start().await?;
        let root = tempfile::tempdir()?;
        let fixture = SyntheticFixture::new(&server, root.path(), FixtureMode::Valid)?;
        let game = LauncherGameConfig::with_launcher(
            fixture.client.clone(),
            fixture.config.clone(),
            Arc::new(FullOrchestrationLauncher),
        );
        let custom_pack_root = root.path().join("custom-pack");
        fs::create_dir_all(&custom_pack_root)?;
        let core = LauncherCore::new_with_game(
            &tokio::runtime::Handle::current(),
            custom_pack_root,
            PackManifest::default(),
            game,
        );
        core.set_offline_profile(profile());
        let mut states = core.subscribe_state();

        core.send_command(LauncherCommand::Play).await?;
        wait_for_status(&mut states, LauncherStatus::Checking).await;
        let acquisition = wait_for_status(&mut states, LauncherStatus::Launching).await;
        assert!(
            acquisition
                .progress()
                .is_none_or(|value| (0.0..=1.0).contains(&value))
        );
        wait_for_status(&mut states, LauncherStatus::Running).await;
        wait_for_status(&mut states, LauncherStatus::Ready).await;

        let layout = MinecraftRuntimeLayout::new(&fixture.config.runtime_root);
        assert!(layout.client_jar(&fixture.config.version_id).is_file());
        assert!(
            layout
                .natives_dir(&fixture.config.version_id)
                .join("native.dll")
                .is_file()
        );
        assert_eq!(server.request_count("/catalog.json"), 1);
        assert_eq!(server.request_count("/runtime/client.jar"), 1);

        core.send_command(LauncherCommand::Play).await?;
        wait_for_status(&mut states, LauncherStatus::Checking).await;
        wait_for_status(&mut states, LauncherStatus::Running).await;
        wait_for_status(&mut states, LauncherStatus::Ready).await;

        assert_eq!(server.request_count("/catalog.json"), 2);
        assert_eq!(server.request_count("/runtime/client.jar"), 1);
        Ok(())
    }

    fn fake_core(
        root: &std::path::Path,
        mode: FakeMode,
        precondition: Option<PathBuf>,
    ) -> (LauncherCore, Arc<AtomicUsize>, Arc<AtomicBool>) {
        let (game, calls, precondition_met) = fake_game_config(root, mode, precondition);
        let core = LauncherCore::new_with_game(
            &tokio::runtime::Handle::current(),
            root,
            PackManifest::default(),
            game,
        );
        (core, calls, precondition_met)
    }

    fn fake_game_config(
        root: &std::path::Path,
        mode: FakeMode,
        precondition_path: Option<PathBuf>,
    ) -> (LauncherGameConfig, Arc<AtomicUsize>, Arc<AtomicBool>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let precondition_met = Arc::new(AtomicBool::new(false));
        let launcher = FakeLauncher {
            mode,
            working_directory: root.to_owned(),
            calls: Arc::clone(&calls),
            precondition_path,
            precondition_met: Arc::clone(&precondition_met),
        };
        let vanilla = VanillaLaunchConfig {
            telegram_auth: None,
            heap_memory: None,
            jvm_args: Vec::new(),
            version_id: "synthetic".to_owned(),
            java_major_override: None,
            server_address: None,
            catalog_url: "http://127.0.0.1/catalog.json".parse().unwrap(),
            assets_base_url: "http://127.0.0.1/assets/".parse().unwrap(),
            runtime_root: root.join("runtime"),
            game_directory: root.to_owned(),
            launcher_name: "launcher-core-test".to_owned(),
            launcher_version: "1".to_owned(),
        };
        (
            LauncherGameConfig::with_launcher(reqwest::Client::new(), vanilla, Arc::new(launcher)),
            calls,
            precondition_met,
        )
    }

    fn profile() -> OfflineProfile {
        OfflineProfile::new("ActorPlayer").unwrap()
    }

    fn manifest_for(path: &str, contents: &[u8]) -> PackManifest {
        let mut hasher = Sha256::new();
        hasher.update(contents);
        PackManifest {
            version: "actor-test".to_owned(),
            minecraft_version: "synthetic".to_owned(),
            files: vec![ManifestFile {
                path: path.to_owned(),
                sha256: format!("{:x}", hasher.finalize()),
                size: contents.len() as u64,
            }],
        }
    }

    async fn wait_for_status(
        states: &mut watch::Receiver<LauncherState>,
        expected: LauncherStatus,
    ) -> LauncherState {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let state = states.borrow_and_update().clone();
                if state.status() == expected {
                    return state;
                }
                states.changed().await.unwrap();
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {expected:?}"))
    }
}
