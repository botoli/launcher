#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app_config;
mod diagnostics;
mod logging;
mod memory_settings;
mod platform;
mod server_monitor;
mod settings;
mod system_memory;

use std::{process::ExitCode, rc::Rc};

use app_config::{AppConfigError, DesktopAppConfig};
use diagnostics::DiagnosticsController;
use launcher_core::{
    ClientConfig, ClientConfigFetchError, HttpClientBuildError, LauncherCommand, LauncherCore,
    LauncherFailure, LauncherGameConfig, LauncherState, LauncherStatus, OfflineProfile,
    OfflineProfileError, VanillaLaunchConfig, build_http_client, fetch_client_config,
};
use launcher_core::{LauncherDiagnostic, LauncherStage};
use launcher_core::{ModsSyncProgress, ModsSyncStage};
use logging::LoggingError;
use memory_settings::MemorySettingsController;
use platform::SingleInstanceState;
use server_monitor::{ServerMonitor, ServerView, apply_server_view};
use settings::{ProfileController, SettingsStore};
use slint::{ComponentHandle, LogicalPosition};
use thiserror::Error;
use tokio::{runtime::Handle, sync::watch};

slint::include_modules!();

const MINECRAFT_VERSION_CATALOG_URL: &str =
    "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
const MINECRAFT_ASSETS_BASE_URL: &str = "https://resources.download.minecraft.net/";

#[derive(Debug, Error)]
enum DesktopError {
    #[error("failed to load desktop application configuration: {0}")]
    Configuration(#[from] AppConfigError),
    #[error("failed to initialize logging: {0}")]
    Logging(#[from] LoggingError),
    #[error("failed to acquire the Windows single-instance guard: {source}")]
    SingleInstance {
        #[source]
        source: std::io::Error,
    },
    #[error("failed to initialize the async runtime: {0}")]
    AsyncRuntime(#[from] std::io::Error),
    #[error("desktop UI error: {0}")]
    Ui(#[from] slint::PlatformError),
}

#[derive(Debug, Error)]
enum DesktopConfigError {
    #[error("built-in URL {name} is invalid")]
    InvalidUrl { name: &'static str },
    #[error("failed to build the launcher HTTP client")]
    HttpClient {
        #[source]
        source: HttpClientBuildError,
    },
    #[error("failed to fetch the initial client config")]
    ClientConfig {
        #[source]
        source: ClientConfigFetchError,
    },
}

impl DesktopConfigError {
    fn user_message(&self) -> String {
        match self {
            Self::InvalidUrl { .. } | Self::HttpClient { .. } => {
                "Не удалось настроить сетевое подключение launcher".to_owned()
            }
            Self::ClientConfig { .. } => {
                "Не удалось получить конфигурацию Minecraft с launcher server".to_owned()
            }
        }
    }
}

struct DesktopDisplayState {
    version: String,
    server_address: String,
}

impl Default for DesktopDisplayState {
    fn default() -> Self {
        Self {
            version: "—".to_owned(),
            server_address: "—".to_owned(),
        }
    }
}

impl DesktopDisplayState {
    fn from_client_config(config: &ClientConfig) -> Self {
        Self {
            version: config.version.clone(),
            server_address: config.server_address().unwrap_or("—").to_owned(),
        }
    }
}

struct ConfiguredLauncher {
    core: LauncherCore,
    profile: Option<OfflineProfile>,
    profile_controller: ProfileController,
    display: DesktopDisplayState,
    client_config: ClientConfig,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "launcher terminated with a startup error");

            #[cfg(debug_assertions)]
            eprintln!("{error}");

            #[cfg(not(debug_assertions))]
            platform::show_error_message(
                "Equestrian Dream Launcher",
                "Не удалось запустить launcher. Подробности сохранены в launcher.log.",
            );

            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), DesktopError> {
    let app_config = DesktopAppConfig::load()?;
    app_config.paths.prepare()?;
    let _logging_guard = logging::initialize(app_config.paths.logs())?;
    install_production_panic_hook();

    let _single_instance_guard = match platform::acquire_single_instance()
        .map_err(|source| DesktopError::SingleInstance { source })?
    {
        SingleInstanceState::Acquired(guard) => guard,
        SingleInstanceState::AlreadyRunning => {
            tracing::warn!("launcher startup stopped because another instance is already running");
            platform::show_error_message(
                "Equestrian Dream Launcher",
                "Equestrian Dream Launcher уже запущен.",
            );
            return Ok(());
        }
    };

    let runtime = tokio::runtime::Runtime::new()?;
    let window = AppWindow::new()?;
    let diagnostics = DiagnosticsController::new(runtime.handle().clone(), app_config.paths.logs());
    diagnostics.bind(&window);
    apply_static_display_state(&window, &DesktopDisplayState::default());
    bind_window_callbacks(&window);
    window.set_installation_path(app_config.paths.game().display().to_string().into());

    match runtime.block_on(configure_launcher(runtime.handle(), app_config)) {
        Ok(configured) => {
            apply_static_display_state(&window, &configured.display);
            apply_initial_profile(&window, &configured.core, configured.profile.as_ref());
            let server_monitor = ServerMonitor::start(
                &window,
                runtime.handle(),
                configured.client_config.server_address().unwrap_or(""),
            );

            let mut state_updates = configured.core.subscribe_state();
            let initial_state = state_updates.borrow_and_update().clone();
            apply_launcher_state(&window, &initial_state);

            let profile_controller = Rc::new(configured.profile_controller);
            let memory_controller = MemorySettingsController::new(
                profile_controller.clone(),
                configured.core.clone(),
                &configured.client_config,
            );
            memory_controller.bind(&window);

            bind_play_command(
                &window,
                configured.core.clone(),
                runtime.handle().clone(),
                profile_controller,
                memory_controller.clone(),
                diagnostics.clone(),
            );
            listen_for_state_updates(
                &window,
                state_updates,
                runtime.handle(),
                memory_controller,
                diagnostics,
                server_monitor,
            );
        }
        Err(error) => {
            tracing::error!(%error, "launcher configuration error");
            apply_configuration_error(&window, &error.user_message());
            diagnostics.show(
                &window,
                "Ошибка конфигурации",
                &LauncherDiagnostic::from_error(&error),
                "Получение конфигурации сборки при открытии лаунчера",
            );
        }
    }

    window.run()?;
    Ok(())
}

fn install_production_panic_hook() {
    #[cfg(not(debug_assertions))]
    std::panic::set_hook(Box::new(|panic_info| {
        let location = panic_info.location().map(|location| {
            let file_name = std::path::Path::new(location.file())
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("unknown");
            format!("{file_name}:{}:{}", location.line(), location.column())
        });

        tracing::error!(
            panic_location = location.as_deref().unwrap_or("unknown"),
            "launcher terminated because of an unexpected panic"
        );
        platform::show_error_message(
            "Equestrian Dream Launcher",
            "Произошла непредвиденная ошибка. Подробности сохранены в launcher.log.",
        );
    }));
}

async fn configure_launcher(
    runtime: &Handle,
    config: DesktopAppConfig,
) -> Result<ConfiguredLauncher, DesktopConfigError> {
    let (profile_controller, profile) =
        ProfileController::load(SettingsStore::new(config.paths.settings_file()));
    let client = build_http_client().map_err(|source| DesktopConfigError::HttpClient { source })?;
    let client_config = fetch_client_config(&client, config.backend.client_config_url())
        .await
        .map_err(|source| DesktopConfigError::ClientConfig { source })?;
    let vanilla = vanilla_launch_config(
        config.paths.game(),
        client_config.version.clone(),
        Some(client_config.java_version),
        client_config.server_address().map(str::to_owned),
    )?;
    let game = LauncherGameConfig::new(client.clone(), vanilla);
    let core = LauncherCore::new_backend_with_game(
        runtime,
        config.paths.game(),
        client,
        config.backend,
        game,
    );
    Ok(ConfiguredLauncher {
        core,
        profile,
        profile_controller,
        display: DesktopDisplayState::from_client_config(&client_config),
        client_config,
    })
}

fn vanilla_launch_config(
    data_directory: &std::path::Path,
    version_id: String,
    java_major_override: Option<u32>,
    server_address: Option<String>,
) -> Result<VanillaLaunchConfig, DesktopConfigError> {
    Ok(VanillaLaunchConfig {
        heap_memory: None,
        jvm_args: Vec::new(),
        version_id,
        java_major_override,
        server_address,
        catalog_url: MINECRAFT_VERSION_CATALOG_URL.parse().map_err(|_| {
            DesktopConfigError::InvalidUrl {
                name: "built-in Minecraft version catalog",
            }
        })?,
        assets_base_url: MINECRAFT_ASSETS_BASE_URL.parse().map_err(|_| {
            DesktopConfigError::InvalidUrl {
                name: "built-in Minecraft assets URL",
            }
        })?,
        runtime_root: data_directory.to_owned(),
        game_directory: data_directory.to_owned(),
        launcher_name: "EquestrianDreamLauncher".to_owned(),
        launcher_version: env!("CARGO_PKG_VERSION").to_owned(),
    })
}

fn apply_static_display_state(window: &AppWindow, state: &DesktopDisplayState) {
    window.set_nickname_input("".into());
    window.set_nickname_error("".into());
    window.set_version(state.version.as_str().into());
    window.set_server_address(state.server_address.as_str().into());
    apply_server_view(
        window,
        &if state.server_address == "—" {
            ServerView::Unconfigured
        } else {
            ServerView::Checking
        },
    );
}

fn apply_initial_profile(
    window: &AppWindow,
    core: &LauncherCore,
    profile: Option<&OfflineProfile>,
) {
    let Some(profile) = profile else {
        return;
    };

    core.set_offline_profile(profile.clone());
    window.set_nickname_input(profile.username().into());
    tracing::info!("profile configured");
}

fn apply_launcher_state(window: &AppWindow, state: &LauncherState) {
    if let Some(config) = state.client_config() {
        apply_client_config(window, config);
    }

    let (mut status_text, mut detail_text) = match state.status() {
        LauncherStatus::Idle => ("Ожидание".to_owned(), "Лаунчер готов к запуску".to_owned()),
        LauncherStatus::Checking => (
            "Проверка".to_owned(),
            "Получение конфигурации Minecraft".to_owned(),
        ),
        LauncherStatus::Downloading => (
            "Загрузка".to_owned(),
            "Синхронизация файлов сборки".to_owned(),
        ),
        LauncherStatus::Ready => ("Готово".to_owned(), "Все файлы проверены".to_owned()),
        LauncherStatus::Launching => (
            "Запуск".to_owned(),
            "Подготовка Minecraft к запуску".to_owned(),
        ),
        LauncherStatus::Running => ("Запущено".to_owned(), "Minecraft запущен".to_owned()),
        LauncherStatus::AuthenticationRequired => (
            "Требуется авторизация".to_owned(),
            "Требуется авторизация".to_owned(),
        ),
        LauncherStatus::ProfileRequired => (
            "Требуется профиль".to_owned(),
            "Укажите ник для запуска игры".to_owned(),
        ),
        LauncherStatus::Error => match state.failure() {
            Some(LauncherFailure::ClientConfig) => (
                "Ошибка конфигурации".to_owned(),
                "Не удалось получить client-config с launcher server".to_owned(),
            ),
            Some(LauncherFailure::ModsSync) => (
                "Ошибка модов".to_owned(),
                "Не удалось безопасно обновить mods.zip".to_owned(),
            ),
            Some(LauncherFailure::LaunchOptions) => (
                "Ошибка параметров ОЗУ".to_owned(),
                "Проверьте настройки памяти и параметры сборки".to_owned(),
            ),
            Some(LauncherFailure::UnsupportedLoader(loader)) => (
                "Неподдерживаемый загрузчик".to_owned(),
                format!("Загрузчик {loader} пока не поддерживается"),
            ),
            Some(LauncherFailure::Installation) => (
                "Ошибка файлов сборки".to_owned(),
                "Не удалось загрузить или проверить файлы Minecraft".to_owned(),
            ),
            Some(LauncherFailure::GameLaunch) => (
                "Не удалось запустить Minecraft".to_owned(),
                "Откройте логи: там указана причина сбоя Java или подготовки игры".to_owned(),
            ),
            Some(LauncherFailure::Java { required_major }) => (
                format!("Не найдена Java {required_major}"),
                format!("Установите Java {required_major} x64 или проверьте JAVA_HOME и PATH"),
            ),
            Some(LauncherFailure::GameExit { code }) => (
                "Minecraft завершился с ошибкой".to_owned(),
                code.map_or_else(
                    || "Процесс игры аварийно завершился. Подробности в логах".to_owned(),
                    |code| format!("Код завершения: {code}. Подробности в логах"),
                ),
            ),
            Some(LauncherFailure::Internal) => (
                "Ошибка лаунчера".to_owned(),
                "Операция прервана. Подробности доступны в логах".to_owned(),
            ),
            None => (
                "Ошибка".to_owned(),
                "Не удалось выполнить запуск".to_owned(),
            ),
        },
    };
    if state.status() != LauncherStatus::Error
        && let Some(stage) = state.stage()
    {
        let (title, detail) = launch_stage_text(stage);
        status_text = title.to_owned();
        detail_text = detail.to_owned();
        if let Some(config) = state.client_config() {
            match stage {
                LauncherStage::CheckingJava => {
                    detail_text = format!("Поиск установленной Java {} x64", config.java_version)
                }
                LauncherStage::AcquiringVanilla => {
                    detail_text = format!(
                        "Minecraft {}: загрузка клиента, библиотек и ресурсов",
                        config.version
                    )
                }
                LauncherStage::AcquiringNeoForge => {
                    detail_text = format!(
                        "NeoForge {}: загрузка загрузчика и его библиотек",
                        config.loader_version
                    )
                }
                _ => {}
            }
        }
    }
    let progress = state.progress().map(|value| value.clamp(0.0, 1.0));
    let progress_value = progress.unwrap_or_default();
    let progress_percent = (progress_value * 100.0).round() as i32;
    let loading = matches!(
        state.status(),
        LauncherStatus::Checking
            | LauncherStatus::Downloading
            | LauncherStatus::Launching
            | LauncherStatus::Running
    );
    let play_enabled = matches!(
        state.status(),
        LauncherStatus::Idle
            | LauncherStatus::Ready
            | LauncherStatus::ProfileRequired
            | LauncherStatus::Error
    );

    window.set_launcher_status(status_text.into());
    window.set_launcher_detail(detail_text.into());
    window.set_progress_metrics("".into());
    window.set_progress(progress_value);
    window.set_progress_percent(progress_percent);
    window.set_has_progress(progress.is_some());
    window.set_loading(loading);
    window.set_play_enabled(play_enabled);
    window.set_play_label(
        match state.status() {
            LauncherStatus::Checking => "Проверка...",
            LauncherStatus::Downloading => "Загрузка...",
            LauncherStatus::Launching => "Запуск...",
            LauncherStatus::Running => "Игра запущена",
            _ => "Играть",
        }
        .into(),
    );
    window.set_horseshoe_angle((progress_value * 1_800.0) % 360.0);
    if state.status() != LauncherStatus::Error
        && let Some(progress) = state.mods_progress()
    {
        apply_mods_progress(window, progress);
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} МБ", bytes as f64 / (1024.0 * 1024.0)).replace('.', ",")
    } else if bytes >= 1024 {
        format!("{:.1} КБ", bytes as f64 / 1024.0).replace('.', ",")
    } else {
        format!("{bytes} Б")
    }
}

fn mods_progress_text(progress: &ModsSyncProgress) -> (String, String, String) {
    let seconds = progress.elapsed.as_secs();
    let elapsed = format!("{:02}:{:02}", seconds / 60, seconds % 60);
    let file = progress
        .current_file
        .as_deref()
        .unwrap_or("mods.zip")
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let count = progress.files_total.map_or_else(
        || progress.files_completed.to_string(),
        |total| format!("{} / {total}", progress.files_completed),
    );
    match progress.stage {
        ModsSyncStage::Checking => (
            "Проверка модов".to_owned(),
            "Сравнение сохранённой версии модов с версией сервера".to_owned(),
            format!("Прошло {elapsed}"),
        ),
        ModsSyncStage::Requesting => (
            "Запрос архива модов".to_owned(),
            format!(
                "{file} — ожидаем ответ сервера уже {} с",
                progress.idle_for.as_secs()
            ),
            format!("Прошло {elapsed}"),
        ),
        ModsSyncStage::Downloading => {
            let amount = progress.total.map_or_else(
                || format_bytes(progress.downloaded),
                |total| {
                    format!(
                        "{} / {}",
                        format_bytes(progress.downloaded),
                        format_bytes(total)
                    )
                },
            );
            let detail = if progress.idle_for >= std::time::Duration::from_secs(1) {
                format!(
                    "{file} — ожидаем новые данные уже {} с",
                    progress.idle_for.as_secs()
                )
            } else if progress.total.is_none() {
                format!("{file} — общий ZIP с модами; сервер не сообщил размер архива")
            } else {
                format!("{file} — скачивается общий ZIP; затем начнётся распаковка модов")
            };
            (
                "Скачивание модов".to_owned(),
                detail,
                format!(
                    "Получено {amount} • {}/с • {elapsed}",
                    format_bytes(progress.bytes_per_second)
                ),
            )
        }
        ModsSyncStage::Inspecting => (
            "Проверка архива модов".to_owned(),
            format!("Проверяется запись: {file}"),
            format!("Проверено {count} записей • {elapsed}"),
        ),
        ModsSyncStage::Extracting => {
            let current = if progress.current_file.is_some() {
                format!("Распаковка: {file}")
            } else {
                "Подготовка файлов из архива".to_owned()
            };
            let size = progress.file_total.map_or_else(String::new, |total| {
                format!(
                    " • файл {} / {}",
                    format_bytes(progress.file_bytes),
                    format_bytes(total)
                )
            });
            (
                "Установка модов".to_owned(),
                current,
                format!("Готово {count} файлов{size} • {elapsed}"),
            )
        }
        ModsSyncStage::Committing => (
            "Сохранение модов".to_owned(),
            "Обновление папки mods и сохранение версии набора".to_owned(),
            format!(
                "Подготовлено {} файлов • {elapsed}",
                progress.files_completed
            ),
        ),
        ModsSyncStage::Complete => (
            "Моды обновлены".to_owned(),
            "Набор модов установлен и сохранён".to_owned(),
            format!(
                "Установлено {} файлов • {elapsed}",
                progress.files_completed
            ),
        ),
        ModsSyncStage::Unchanged => (
            "Моды актуальны".to_owned(),
            "Версия набора совпадает; повторное скачивание не требуется".to_owned(),
            format!("Прошло {elapsed}"),
        ),
    }
}

fn apply_mods_progress(window: &AppWindow, progress: &ModsSyncProgress) {
    let (status, detail, metrics) = mods_progress_text(progress);
    window.set_launcher_status(status.into());
    window.set_launcher_detail(detail.into());
    window.set_progress_metrics(metrics.into());
    let fraction = progress.fraction();
    window.set_has_progress(fraction.is_some());
    window.set_progress(fraction.unwrap_or(0.0));
    window.set_progress_percent((fraction.unwrap_or(0.0) * 100.0).round() as i32);
    window.set_horseshoe_angle((progress.elapsed.as_secs_f32() * 45.0) % 360.0);
}

fn launch_stage_text(stage: LauncherStage) -> (&'static str, &'static str) {
    match stage {
        LauncherStage::FetchingConfig => (
            "Получение конфигурации",
            "Запрос версии игры и параметров сборки с сервера",
        ),
        LauncherStage::CheckingFiles => (
            "Проверка файлов",
            "Проверка наличия, размеров и контрольных сумм файлов сборки",
        ),
        LauncherStage::DownloadingPack => (
            "Загрузка сборки",
            "Загрузка отсутствующих и повреждённых файлов",
        ),
        LauncherStage::SynchronizingMods => (
            "Обновление модов",
            "Проверка версии модов, загрузка и распаковка архива",
        ),
        LauncherStage::ResolvingVersion => (
            "Выбор версии Minecraft",
            "Получение метаданных выбранной версии игры",
        ),
        LauncherStage::CheckingJava => ("Проверка Java", "Поиск установленной Java нужной версии"),
        LauncherStage::AcquiringVanilla => (
            "Загрузка Minecraft",
            "Загрузка клиента, библиотек и игровых ресурсов",
        ),
        LauncherStage::AcquiringNeoForge => (
            "Подготовка NeoForge",
            "Загрузка загрузчика и выполнение установочных операций",
        ),
        LauncherStage::PreparingRuntime => (
            "Подготовка к запуску",
            "Подготовка библиотек и параметров запуска игры",
        ),
        LauncherStage::StartingProcess => (
            "Запуск Minecraft",
            "Создание процесса Java и передача параметров игры",
        ),
        LauncherStage::Running => (
            "Игра запущена",
            "Minecraft работает. После закрытия игры кнопки станут доступны",
        ),
    }
}

fn apply_launch_pending(window: &AppWindow) {
    window.set_progress_metrics("".into());
    window.set_loading(true);
    window.set_play_enabled(false);
    window.set_play_label("Проверка...".into());
    window.set_settings_visible(false);
    window.set_launcher_status("Начало запуска".into());
    window.set_launcher_detail("Проверка профиля и параметров сборки".into());
    window.set_has_progress(false);
    window.set_progress(0.0);
    window.set_progress_percent(0);
}

fn apply_client_config(window: &AppWindow, config: &ClientConfig) {
    window.set_version(config.version.as_str().into());
    window.set_server_address(config.server_address().unwrap_or("—").into());
}

fn apply_configuration_error(window: &AppWindow, message: &str) {
    window.set_progress_metrics("".into());
    window.set_launcher_status("Ошибка конфигурации".into());
    window.set_launcher_detail(message.into());
    window.set_progress(0.0);
    window.set_progress_percent(0);
    window.set_has_progress(false);
    window.set_loading(false);
    window.set_play_enabled(false);
    window.set_horseshoe_angle(0.0);
}

fn apply_profile_required(window: &AppWindow, message: &str) {
    window.set_progress_metrics("".into());
    window.set_nickname_error(message.into());
    window.set_launcher_status("Требуется профиль".into());
    window.set_launcher_detail(message.into());
    window.set_progress(0.0);
    window.set_progress_percent(0);
    window.set_has_progress(false);
    window.set_loading(false);
    window.set_play_enabled(true);
    window.set_horseshoe_angle(0.0);
}

fn bind_window_callbacks(window: &AppWindow) {
    window.on_mods_requested(|| {
        tracing::debug!("Mods clicked; no mods action is configured");
    });

    let window_weak = window.as_weak();
    window.on_move_window(move |delta_x, delta_y| {
        if let Some(window) = window_weak.upgrade() {
            let platform_window = window.window();
            let scale_factor = platform_window.scale_factor();
            let current_position = platform_window.position().to_logical(scale_factor);

            platform_window.set_position(LogicalPosition::new(
                current_position.x + delta_x,
                current_position.y + delta_y,
            ));
        }
    });
}

fn bind_play_command(
    window: &AppWindow,
    core: LauncherCore,
    runtime: Handle,
    profile_controller: Rc<ProfileController>,
    memory_controller: Rc<MemorySettingsController>,
    diagnostics: Rc<DiagnosticsController>,
) {
    let window_weak = window.as_weak();
    window.on_play_requested(move |nickname| {
        let Some(window) = window_weak.upgrade() else {
            return;
        };
        if window.get_loading() || !window.get_play_enabled() {
            return;
        }
        if !memory_controller.prepare_play(&window) {
            return;
        }
        let profile = match profile_controller.accept_username(nickname.to_string()) {
            Ok(profile) => profile,
            Err(error) => {
                if let Some(window) = window_weak.upgrade() {
                    apply_profile_required(&window, offline_profile_error_message(&error));
                }
                return;
            }
        };

        core.set_offline_profile(profile.clone());
        if let Some(window) = window_weak.upgrade() {
            window.set_nickname_input(profile.username().into());
            window.set_nickname_error("".into());
        }
        tracing::info!("profile configured");

        diagnostics.clear(&window);
        apply_launch_pending(&window);

        let core = core.clone();
        let command_task =
            runtime.spawn(async move { core.send_command(LauncherCommand::Play).await });
        let weak = window_weak.clone();
        let diagnostics = diagnostics.clone();
        let _ = slint::spawn_local(async move {
            let failure = match command_task.await {
                Ok(Ok(())) => return,
                Ok(Err(error)) => LauncherDiagnostic::from_error(&error),
                Err(error) => LauncherDiagnostic::from_error(&error),
            };
            tracing::error!(reason = %failure.message, "failed to send PLAY command");
            if let Some(window) = weak.upgrade() {
                apply_configuration_error(
                    &window,
                    "Обработчик запуска недоступен. Перезапустите лаунчер.",
                );
                diagnostics.show(
                    &window,
                    "Ошибка запуска",
                    &failure,
                    "Отправка команды запуска",
                );
            }
        });
    });
}

fn offline_profile_error_message(error: &OfflineProfileError) -> &'static str {
    match error {
        OfflineProfileError::UsernameTooShort { .. }
        | OfflineProfileError::UsernameTooLong { .. } => "Ник должен содержать от 3 до 16 символов",
        OfflineProfileError::InvalidUsernameCharacter { .. } => {
            "Используйте латинские буквы, цифры и _"
        }
        OfflineProfileError::InvalidUuid { .. } => "Не удалось настроить offline-профиль",
    }
}

fn listen_for_state_updates(
    window: &AppWindow,
    mut states: watch::Receiver<LauncherState>,
    runtime: &Handle,
    memory_controller: Rc<MemorySettingsController>,
    diagnostics: Rc<DiagnosticsController>,
    server_monitor: Rc<ServerMonitor>,
) {
    let window_weak = window.as_weak();
    let (ui_tx, mut ui_rx) = watch::channel::<Option<LauncherState>>(None);
    // Keep only the latest snapshot while the UI is busy drawing a frame.
    slint::spawn_local(async move {
        while ui_rx.changed().await.is_ok() {
            let Some(state) = ui_rx.borrow_and_update().clone() else {
                continue;
            };
            if let Some(window) = window_weak.upgrade() {
                if let Some(config) = state.client_config() {
                    memory_controller.observe_config(&window, config);
                    server_monitor.observe_address(&window, config.server_address());
                }
                apply_launcher_state(&window, &state);
                diagnostics.observe_state(&window, &state);
            } else {
                break;
            }
        }
    })
    .expect("UI event loop is available");
    let listener_task = runtime.spawn(async move {
        while states.changed().await.is_ok() {
            let state = states.borrow_and_update().clone();
            if ui_tx.send(Some(state)).is_err() {
                break;
            }
        }
    });
    drop(listener_task);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_state_uses_client_version_and_server() {
        let config = ClientConfig {
            version: "1.21.1".to_owned(),
            loader: "neoforge".to_owned(),
            loader_version: "21.1.248".to_owned(),
            java_version: 21,
            min_memory: 2048,
            max_memory: 4096,
            jvm_args: vec!["-Dfile.encoding=UTF-8".to_owned()],
            servers: Vec::new(),
            mods_hash: "hash".to_owned(),
            mods_url: "https://launcher.example/mods.zip".parse().unwrap(),
            server_ip: "server.example:25565".to_owned(),
        };

        let display = DesktopDisplayState::from_client_config(&config);

        assert_eq!(display.version, "1.21.1");
        assert_eq!(display.server_address, "server.example:25565");
    }

    #[test]
    fn maps_profile_validation_errors_to_safe_text() {
        let short = OfflineProfile::new("ab").unwrap_err();
        let invalid_character = OfflineProfile::new("bad-name").unwrap_err();

        assert_eq!(
            offline_profile_error_message(&short),
            "Ник должен содержать от 3 до 16 символов"
        );
        assert_eq!(
            offline_profile_error_message(&invalid_character),
            "Используйте латинские буквы, цифры и _"
        );
    }
}
