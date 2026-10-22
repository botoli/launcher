use std::{cell::Cell, rc::Rc};

use launcher_core::{ClientConfig, HeapMemory, LauncherCore, UserLaunchOptions};
use slint::ComponentHandle;

use crate::{
    AppWindow,
    settings::ProfileController,
    system_memory::{
        MEMORY_STEP_MIB, MIN_MEMORY_MIB, MemoryBudget, SystemMemory, query_system_memory,
    },
};

pub struct MemorySettingsController {
    settings: Rc<ProfileController>,
    core: LauncherCore,
    backend_memory: Cell<(u32, u32)>,
    budget: Cell<Option<MemoryBudget>>,
    query_memory: fn() -> std::io::Result<SystemMemory>,
}

impl MemorySettingsController {
    pub fn new(
        settings: Rc<ProfileController>,
        core: LauncherCore,
        config: &ClientConfig,
    ) -> Rc<Self> {
        Rc::new(Self {
            settings,
            core,
            backend_memory: Cell::new((config.min_memory, config.max_memory)),
            budget: Cell::new(None),
            query_memory: query_system_memory,
        })
    }

    pub fn observe_config(&self, window: &AppWindow, config: &ClientConfig) {
        self.backend_memory
            .set((config.min_memory, config.max_memory));
        self.update_launch_readiness(window);
    }

    fn detect(&self, window: &AppWindow) {
        self.budget.set(None);
        window.set_memory_detected(false);
        window.set_memory_maximum(MIN_MEMORY_MIB as i32);
        match (self.query_memory)() {
            Ok(memory) => match MemoryBudget::for_system(memory) {
                Ok(budget) => {
                    tracing::debug!(
                        total_mib = memory.total_mib,
                        available_mib = memory.available_mib,
                        maximum_mib = budget.maximum_mib,
                        "detected memory budget"
                    );
                    self.budget.set(Some(budget));
                    window.set_memory_maximum(budget.maximum_mib as i32);
                    window.set_memory_detected(true);
                }
                Err(message) => {
                    window.set_memory_problem(message.into());
                }
            },
            Err(error) => {
                tracing::warn!(%error, "failed to detect physical memory");
                window.set_memory_problem(
                    "Не удалось определить ОЗУ. Откройте настройки и нажмите «Определить память снова»."
                        .into(),
                );
            }
        }
        self.update_launch_readiness(window);
    }

    fn effective_memory(&self) -> Result<u32, String> {
        let budget = self
            .budget
            .get()
            .ok_or_else(|| "Не удалось определить допустимый объём ОЗУ".to_owned())?;
        let (minimum, maximum) = self.backend_memory.get();
        HeapMemory::new(minimum, maximum)
            .map_err(|_| "Сервер передал некорректные параметры ОЗУ".to_owned())?;
        match self.settings.memory_mib() {
            Some(saved) => budget.validate(saved).map_err(|_| format!("Сохранено {saved} МБ. Допустимо {} МБ. Исправьте и сохраните ОЗУ в настройках.", budget.suggested(saved))),
            None => Ok(budget.suggested(maximum)),
        }
    }

    fn update_launch_readiness(&self, window: &AppWindow) {
        match self.effective_memory() {
            Ok(value) => {
                self.core.set_launch_options(UserLaunchOptions {
                    max_memory_mib: Some(value),
                });
                window.set_memory_ready(true);
                window.set_memory_problem("".into());
            }
            Err(message) => {
                window.set_memory_ready(false);
                // Keep the more specific memory-detection error if there is no budget.
                if self.budget.get().is_some() || window.get_memory_problem().is_empty() {
                    window.set_memory_problem(message.into());
                }
            }
        }
    }

    fn edit(&self, window: &AppWindow, input: &str) {
        window.set_memory_input(input.into());
        window.set_memory_save_enabled(false);
        window.set_memory_gib("—".into());
        let Some(budget) = self.budget.get() else {
            window.set_memory_error(window.get_memory_problem());
            return;
        };
        match budget.parse(input) {
            Ok(value) => {
                window.set_memory_value(value as i32);
                window.set_memory_gib(
                    format!("{:.2}", f64::from(value) / 1024.0)
                        .replace('.', ",")
                        .into(),
                );
                window.set_memory_error("".into());
                window.set_memory_save_enabled(true);
            }
            Err(message) => window.set_memory_error(message.into()),
        }
    }

    fn open(&self, window: &AppWindow) {
        self.detect(window);
        let value = self.settings.memory_mib().unwrap_or_else(|| {
            self.budget
                .get()
                .map_or(self.backend_memory.get().1, |budget| {
                    budget.suggested(self.backend_memory.get().1)
                })
        });
        window.set_memory_suggestion("".into());
        if let Some(budget) = self.budget.get() {
            window.set_memory_value(budget.suggested(value) as i32);
            if budget.validate(value).is_err() {
                window.set_memory_suggestion(
                    format!(
                        "Сохранено {value} МБ; допустимо {} МБ.",
                        budget.suggested(value)
                    )
                    .into(),
                );
            }
        }
        self.edit(window, &value.to_string());
        window.set_settings_visible(true);
    }

    fn slider(&self, window: &AppWindow, value: f32) {
        let Some(budget) = self.budget.get() else {
            return;
        };
        if !value.is_finite() {
            return;
        }
        let value = value.clamp(MIN_MEMORY_MIB as f32, budget.maximum_mib as f32);
        let value = (value / MEMORY_STEP_MIB as f32).round() as u32 * MEMORY_STEP_MIB;
        self.edit(window, &budget.suggested(value).to_string());
    }

    fn step(&self, window: &AppWindow, direction: i32) {
        let Some(budget) = self.budget.get() else {
            return;
        };
        let current = budget
            .parse(window.get_memory_input().as_str())
            .unwrap_or(window.get_memory_value().max(MIN_MEMORY_MIB as i32) as u32);
        let next = if direction > 0 {
            current.saturating_add(MEMORY_STEP_MIB)
        } else {
            current.saturating_sub(MEMORY_STEP_MIB)
        };
        self.edit(window, &budget.suggested(next).to_string());
    }

    fn save(&self, window: &AppWindow) {
        // Check the current machine again before committing the user's choice.
        self.detect(window);
        let input = window.get_memory_input();
        self.edit(window, input.as_str());
        let Some(budget) = self.budget.get() else {
            return;
        };
        let Ok(value) = budget.parse(input.as_str()) else {
            return;
        };
        match self.settings.save_memory(value) {
            Ok(()) => {
                self.update_launch_readiness(window);
                window.set_settings_visible(false);
            }
            Err(error) => {
                tracing::warn!(%error, "failed to save memory settings");
                window.set_memory_error(
                    "Не удалось сохранить настройки. Проверьте доступ к папке данных и повторите."
                        .into(),
                );
            }
        }
    }

    pub fn prepare_play(&self, window: &AppWindow) -> bool {
        self.detect(window);
        window.get_memory_ready()
    }

    pub fn bind(self: &Rc<Self>, window: &AppWindow) {
        self.detect(window);
        let controller = self.clone();
        let weak = window.as_weak();
        window.on_settings_requested(move || {
            if let Some(window) = weak.upgrade() {
                if window.get_loading() {
                    return;
                }
                controller.open(&window);
            }
        });
        let controller = self.clone();
        let weak = window.as_weak();
        window.on_memory_input_edited(move |input| {
            if let Some(window) = weak.upgrade() {
                controller.edit(&window, input.as_str());
            }
        });
        let controller = self.clone();
        let weak = window.as_weak();
        window.on_memory_slider_changed(move |value| {
            if let Some(window) = weak.upgrade() {
                controller.slider(&window, value);
            }
        });
        let controller = self.clone();
        let weak = window.as_weak();
        window.on_memory_stepped(move |direction| {
            if let Some(window) = weak.upgrade() {
                controller.step(&window, direction);
            }
        });
        let controller = self.clone();
        let weak = window.as_weak();
        window.on_memory_save_requested(move || {
            if let Some(window) = weak.upgrade() {
                controller.save(&window);
            }
        });
        let controller = self.clone();
        let weak = window.as_weak();
        window.on_memory_retry_requested(move || {
            if let Some(window) = weak.upgrade() {
                controller.detect(&window);
                controller.edit(&window, window.get_memory_input().as_str());
            }
        });
        let controller = self.clone();
        let weak = window.as_weak();
        window.on_memory_suggestion_requested(move || {
            if let Some(window) = weak.upgrade()
                && let Some(budget) = controller.budget.get()
            {
                let saved = controller
                    .settings
                    .memory_mib()
                    .unwrap_or(controller.backend_memory.get().1);
                controller.edit(&window, &budget.suggested(saved).to_string());
            }
        });
        let weak = window.as_weak();
        window.on_memory_back_requested(move || {
            if let Some(window) = weak.upgrade() {
                window.set_settings_visible(false);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        path::Path,
        rc::Rc,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use slint::{
        PhysicalSize, Rgb8Pixel,
        platform::{
            Platform, PlatformError, WindowAdapter, WindowEvent,
            software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
        },
    };

    use super::*;
    use crate::settings::SettingsStore;

    type EventQueue = Arc<Mutex<VecDeque<Box<dyn FnOnce() + Send>>>>;
    struct TestPlatform(Rc<MinimalSoftwareWindow>, EventQueue);
    struct TestProxy(EventQueue);
    impl slint::platform::EventLoopProxy for TestProxy {
        fn quit_event_loop(&self) -> Result<(), slint::EventLoopError> {
            Ok(())
        }
        fn invoke_from_event_loop(
            &self,
            event: Box<dyn FnOnce() + Send>,
        ) -> Result<(), slint::EventLoopError> {
            self.0.lock().unwrap().push_back(event);
            Ok(())
        }
    }
    impl Platform for TestPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
            Ok(self.0.clone())
        }
        fn new_event_loop_proxy(&self) -> Option<Box<dyn slint::platform::EventLoopProxy>> {
            Some(Box::new(TestProxy(self.1.clone())))
        }
    }

    fn pump_until(events: &EventQueue, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !condition() {
            slint::platform::update_timers_and_animations();
            let event = events.lock().unwrap().pop_front();
            if let Some(event) = event {
                event();
            } else {
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(Instant::now() < deadline, "UI update timed out");
        }
    }

    fn click(window: &AppWindow, x: f32, y: f32) {
        let position = slint::LogicalPosition::new(x, y);
        window
            .window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        window.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: slint::platform::PointerEventButton::Left,
        });
        window
            .window()
            .dispatch_event(WindowEvent::PointerReleased {
                position,
                button: slint::platform::PointerEventButton::Left,
            });
    }

    fn eight_gib() -> std::io::Result<SystemMemory> {
        Ok(SystemMemory {
            total_mib: 8192,
            available_mib: 3072,
        })
    }

    fn unavailable() -> std::io::Result<SystemMemory> {
        Err(std::io::Error::other("test memory API failure"))
    }

    fn key(window: &AppWindow, text: impl Into<slint::SharedString>) {
        let text = text.into();
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        window
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text });
    }

    fn render(window: &AppWindow, adapter: &MinimalSoftwareWindow, name: &str) {
        let mut sizes = vec![
            (1180., 750., 1.0, "100"),
            (1180., 750., 1.25, "125"),
            (1180., 750., 1.5, "150"),
        ];
        if name == "main-initial"
            || name == "mods-downloading"
            || name.starts_with("news")
            || name.starts_with("settings")
        {
            sizes.extend([
                (1536., 1024., 1.0, "reference"),
                (1000., 720., 1.5, "minimum-150"),
                (1800., 800., 1.0, "wide"),
            ]);
        }
        for (logical_width, logical_height, scale, suffix) in sizes {
            window
                .window()
                .dispatch_event(WindowEvent::ScaleFactorChanged {
                    scale_factor: scale,
                });
            let width = (logical_width * scale) as u32;
            let height = (logical_height * scale) as u32;
            adapter.set_size(PhysicalSize::new(width, height));
            window.window().request_redraw();
            let mut pixels = vec![Rgb8Pixel::default(); (width * height) as usize];
            assert!(adapter.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, width as usize);
            }));
            if let Some(directory) = std::env::var_os("LAUNCHER_UI_PREVIEW_DIR") {
                if std::env::var("LAUNCHER_UI_PREVIEW_FILTER")
                    .is_ok_and(|filter| !name.contains(&filter))
                {
                    continue;
                }
                fs::create_dir_all(&directory).unwrap();
                let bytes: Vec<u8> = pixels
                    .into_iter()
                    .flat_map(|pixel| [pixel.r, pixel.g, pixel.b])
                    .collect();
                image::save_buffer(
                    Path::new(&directory).join(format!("{name}-{suffix}.png")),
                    &bytes,
                    width,
                    height,
                    image::ColorType::Rgb8,
                )
                .unwrap();
            }
        }
        window
            .window()
            .dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 1.0 });
        adapter.set_size(PhysicalSize::new(1180, 750));
    }

    #[test]
    fn memory_screen_syncs_validates_saves_and_renders() {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        let events = EventQueue::default();
        slint::platform::set_platform(Box::new(TestPlatform(adapter.clone(), events.clone())))
            .unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state/settings.json");
        let (profile_controller, _) = ProfileController::load(SettingsStore::new(&path));
        let settings = Rc::new(profile_controller);
        settings.accept_username("TestPlayer").unwrap();
        let config: ClientConfig = serde_json::from_value(serde_json::json!({
            "version":"1.21.1", "loader":"vanilla", "loaderVersion":"", "javaVersion":21,
            "minMemory":2048, "maxMemory":8192, "jvmArgs":[], "servers":[],
            "modsHash":"test", "modsURL":"https://example.test/mods.zip", "serverIP":""
        }))
        .unwrap();
        // Initial configuration fails twice, then succeeds in the same window.
        // Duplicate Retry clicks must not create parallel bootstrap operations.
        let bootstrap_window = AppWindow::new().unwrap();
        let bootstrap_diagnostics =
            crate::diagnostics::DiagnosticsController::new(runtime.handle().clone(), root.path());
        bootstrap_diagnostics.bind(&bootstrap_window);
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let backend = format!("http://{}/", listener.local_addr().unwrap());
        let payload = serde_json::json!({
            "version":"1.21.1", "loader":"vanilla", "loaderVersion":"", "javaVersion":21,
            "minMemory":2048, "maxMemory":8192, "jvmArgs":[], "servers":[],
            "modsHash":"test", "modsURL":"https://example.test/mods.zip", "serverIP":""
        })
        .to_string();
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = requests.clone();
        let server = runtime.spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            for attempt in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                let count = stream.read(&mut request).await.unwrap();
                assert!(
                    String::from_utf8_lossy(&request[..count])
                        .starts_with("GET /api/client-config")
                );
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let (status, body) = if attempt < 2 {
                    ("503 Service Unavailable", "unavailable")
                } else {
                    ("200 OK", payload.as_str())
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let bootstrap_config =
            crate::app_config::DesktopAppConfig::for_test(&root.path().join("bootstrap"), &backend);
        bootstrap_config.paths.prepare().unwrap();
        crate::bind_launcher_startup(
            &bootstrap_window,
            runtime.handle().clone(),
            bootstrap_config,
            launcher_core::java_install::JavaSetup::channel().0,
            bootstrap_diagnostics.clone(),
        );
        assert!(bootstrap_window.get_loading());
        bootstrap_window.invoke_retry_requested();
        pump_until(&events, || bootstrap_window.get_retry_available());
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!bootstrap_window.get_play_enabled());
        assert!(
            bootstrap_window
                .get_diagnostics_advice()
                .contains("запрошена заново")
        );
        bootstrap_window.invoke_retry_requested();
        bootstrap_window.invoke_retry_requested();
        pump_until(&events, || bootstrap_window.get_retry_available());
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 2);
        bootstrap_window.invoke_retry_requested();
        pump_until(&events, || bootstrap_window.get_repair_available());
        assert!(bootstrap_window.get_play_enabled());
        assert_eq!(bootstrap_window.get_version(), "1.21.1");
        assert!(!bootstrap_window.get_diagnostics_visible());
        assert!(!bootstrap_window.get_retry_available());
        runtime.block_on(server).unwrap();
        drop(bootstrap_window);

        // Completing Telegram login resumes a pending Play only once, after
        // installing the session; visiting the account page does not launch.
        let login_window = AppWindow::new().unwrap();
        let login_gate = launcher_core::auth::telegram::TelegramAuthGate::new(
            launcher_core::auth::telegram::TelegramAuthClient::new(
                launcher_core::build_http_client().unwrap(),
                "http://127.0.0.1:1/".parse().unwrap(),
            ),
        );
        let login = crate::telegram_login::TelegramLogin::bind(
            &login_window,
            runtime.handle().clone(),
            login_gate.clone(),
            LauncherCore::new(runtime.handle()),
            crate::telegram_session::SessionStore::new(
                root.path().join("login.dat"),
                "test".into(),
            ),
        );
        let login_plays = Rc::new(Cell::new(0));
        let count = login_plays.clone();
        let gate_for_callback = login_gate.clone();
        login_window.on_play_requested(move |nickname| {
            assert!(gate_for_callback.session().is_some());
            assert_eq!(nickname, "TestPlayer");
            count.set(count.get() + 1);
        });
        let login_session = launcher_core::auth::telegram::TelegramSession {
            token: "test-only-session".into(),
            expires_at: 9999999999999,
            user: launcher_core::auth::telegram::TelegramUser {
                login: "tg_test".into(),
                nickname: "TestPlayer".into(),
                registration_status: "pending".into(),
            },
        };
        login_window.set_auth_play_pending(true);
        login.accept(login_session.clone()).unwrap();
        assert_eq!(login_plays.get(), 1);
        assert!(login_window.get_play_enabled());
        assert!(!login_window.get_auth_visible());
        login.accept(login_session.clone()).unwrap();
        assert_eq!(login_plays.get(), 1);
        login_window.set_auth_play_pending(true);
        login_window.invoke_auth_back_requested();
        login.accept(login_session).unwrap();
        assert_eq!(login_plays.get(), 1);
        drop(login);
        drop(login_window);

        let core = LauncherCore::new(runtime.handle());
        let mut controller = MemorySettingsController::new(settings.clone(), core, &config);
        Rc::get_mut(&mut controller).unwrap().query_memory = eight_gib;
        let window = AppWindow::new().unwrap();
        window.set_auth_verified(true);
        let diagnostics =
            crate::diagnostics::DiagnosticsController::new(runtime.handle().clone(), root.path());
        diagnostics.bind(&window);
        crate::bind_repair_command(
            &window,
            controller.core.clone(),
            runtime.handle().clone(),
            diagnostics.clone(),
        );
        crate::bind_preparation_commands(
            &window,
            controller.core.clone(),
            runtime.handle().clone(),
            diagnostics.clone(),
        );
        crate::apply_launcher_state(&window, &controller.core.subscribe_state().borrow());
        window.set_version("1.21.1".into());
        window.set_server_address("server.example:25565".into());
        window.set_nickname_input("TestPlayer".into());
        window.set_installation_path(
            r"C:\Users\Player\AppData\Local\EquestrianDreamLauncher\game".into(),
        );
        controller.bind(&window);
        let login_gate = launcher_core::auth::telegram::TelegramAuthGate::new(
            launcher_core::auth::telegram::TelegramAuthClient::new(
                launcher_core::build_http_client().unwrap(),
                "http://127.0.0.1".parse().unwrap(),
            ),
        );
        let _login = crate::telegram_login::TelegramLogin::bind(
            &window,
            runtime.handle().clone(),
            login_gate,
            controller.core.clone(),
            crate::telegram_session::SessionStore::new(
                root.path().join("state/telegram-session.dat"),
                "http://127.0.0.1".into(),
            ),
        );
        window.show().unwrap();
        // The launcher starts on the home screen; the avatar opens the account page.
        window.set_auth_verified(false);
        assert!(!window.get_auth_visible());
        render(&window, &adapter, "main-signed-out");
        click(&window, 900.0, 30.0);
        assert!(window.get_auth_visible());
        assert_eq!(window.get_auth_provider(), 0);
        render(&window, &adapter, "auth-providers");
        click(&window, 824.0, 397.0);
        assert_eq!(window.get_auth_provider(), 2);
        assert!(!window.get_auth_busy());
        render(&window, &adapter, "discord-login");
        click(&window, 720.0, 540.0);
        assert_eq!(window.get_auth_provider(), 0);
        let requests = Rc::new(std::cell::Cell::new(0));
        let requested = requests.clone();
        window.on_auth_requested(move || requested.set(requested.get() + 1));
        click(&window, 550.0, 397.0);
        assert_eq!(window.get_auth_provider(), 1);
        assert_eq!(requests.get(), 1, "Telegram opens on the first click");
        window.set_auth_busy(true);
        window.set_auth_message("Создаём запрос входа…".into());
        render(&window, &adapter, "telegram-connecting");
        click(&window, 700.0, 490.0);
        assert_eq!(requests.get(), 1, "pending requests cannot be duplicated");
        window.set_auth_code("A1B2C3".into());
        window.set_auth_message("Откройте бота и подтвердите вход с этим кодом. При первом входе бот поможет выбрать ник. Ссылка действует 5 минут.".into());
        window.set_auth_link("https://t.me/TestAuthBot?start=test".into());
        render(&window, &adapter, "telegram-login");
        window.set_auth_busy(false);
        window.set_auth_error(true);
        window.set_auth_message(
            "Не удалось подключиться к серверу входа. Проверьте интернет и повторите попытку."
                .into(),
        );
        render(&window, &adapter, "telegram-error");
        window.set_auth_link("".into());
        window.set_auth_code("".into());
        render(&window, &adapter, "telegram-error-no-link");
        window.set_auth_error(false);
        window.set_auth_verified(true);
        render(&window, &adapter, "auth-account");
        click(&window, 220.0, 150.0);
        assert!(!window.get_auth_visible());
        assert!(!window.get_auth_busy());
        assert_eq!(window.get_auth_provider(), 0);
        render(&window, &adapter, "main-initial");
        // News is a real route: verify mouse/keyboard access, history scrolling,
        // return navigation, and the launch-time navigation lock.
        assert!(!window.get_news_visible());
        click(&window, 190.0, 80.0);
        assert!(window.get_news_visible());
        key(&window, slint::platform::Key::Return);
        assert!(window.get_news_visible());
        render(&window, &adapter, "news");
        window
            .window()
            .dispatch_event(WindowEvent::PointerScrolled {
                position: slint::LogicalPosition::new(650.0, 500.0),
                delta_x: 0.0,
                delta_y: -650.0,
            });
        render(&window, &adapter, "news-history");
        click(&window, 80.0, 80.0);
        assert!(!window.get_news_visible());
        key(&window, slint::platform::Key::Tab);
        key(&window, slint::platform::Key::Return);
        assert!(window.get_news_visible());
        click(&window, 80.0, 80.0);
        window.set_loading(true);
        click(&window, 190.0, 80.0);
        assert!(!window.get_news_visible());
        window.set_loading(false);

        // The Java offer blocks navigation, and its buttons stay disabled while
        // the external Windows Installer owns the installation operation.
        let java = crate::java_install::JavaInstallController::bind(
            &window,
            runtime.handle().clone(),
            root.path().join("logs"),
        );
        window.set_java_visible(true);
        window.set_java_message("Конфигурация и моды проверены. Для запуска нужна Java 21 x64. Скачать и установить Eclipse Temurin OpenJDK 21.0.3+9? После загрузки откроется мастер Windows; может потребоваться разрешение администратора. Оставьте стандартную папку установки. Затем запуск Minecraft продолжится автоматически.".into());
        render(&window, &adapter, "java-offer");
        click(&window, 900.0, 30.0);
        assert!(!window.get_auth_visible());
        click(&window, 1042.0, 30.0);
        assert!(!window.get_settings_visible());
        window.set_java_busy(true);
        window.set_java_progress(0.45);
        window.set_java_message(
            "Скачивание Java: 77,1 из 171,4 МБ. Файл будет проверен перед установкой.".into(),
        );
        render(&window, &adapter, "java-downloading");
        window.set_java_can_cancel(false);
        window.set_java_message("Ожидаем завершения мастера установки Java 21. Отменить установку можно в мастере Windows.".into());
        render(&window, &adapter, "java-installing");
        window.invoke_java_dismiss_requested();
        assert!(window.get_java_visible());
        window.set_java_busy(false);
        window.set_java_error(true);
        window.set_java_can_cancel(true);
        window.set_java_message(
            "Установка Java отменена. Можно повторить или вернуться в лаунчер.".into(),
        );
        render(&window, &adapter, "java-cancelled");
        click(&window, 800.0, 535.0);
        assert!(!window.get_java_visible());
        drop(java);
        // Render every connection icon and verify that failed probes clear the old ping.
        for (ping_ms, signal, preview) in [
            (48, 3, "server-good"),
            (190, 2, "server-fair"),
            (420, 1, "server-poor"),
        ] {
            crate::server_monitor::apply_server_view(
                &window,
                &crate::server_monitor::ServerView::Online { ping_ms },
            );
            assert_eq!(window.get_server_signal(), signal);
            assert_eq!(window.get_ping_ms(), ping_ms);
            assert!(window.get_server_online());
            render(&window, &adapter, preview);
        }
        crate::server_monitor::apply_server_view(
            &window,
            &crate::server_monitor::ServerView::Unavailable,
        );
        assert_eq!(window.get_ping_ms(), -1);
        assert_eq!(window.get_server_signal(), 0);
        assert!(!window.get_server_online());
        render(&window, &adapter, "server-unavailable");
        window.set_server_address("a-very-long-minecraft-server-address.example.org:25565".into());
        render(&window, &adapter, "server-long-address");
        window.set_server_address("server.example:25565".into());
        let position = slint::LogicalPosition::new(1042.0, 30.0);
        window
            .window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        window.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: slint::platform::PointerEventButton::Left,
        });
        window
            .window()
            .dispatch_event(WindowEvent::PointerReleased {
                position,
                button: slint::platform::PointerEventButton::Left,
            });
        assert!(
            window.get_settings_visible(),
            "settings must open after the first mouse click"
        );
        assert_eq!(window.get_memory_input(), "6144");
        assert!(window.get_memory_ready());
        render(&window, &adapter, "settings");

        // Repair is independent of login and memory validation, and does not
        // commit the settings form when the user leaves it to reinstall mods.
        let saved_settings = fs::read(&path).unwrap();
        window.set_auth_verified(false);
        window.set_memory_ready(false);
        window.invoke_memory_input_edited("invalid".into());
        window.set_update_visible(true);
        window.invoke_repair_requested();
        assert!(!window.get_loading(), "updates block repair");
        window.set_update_visible(false);
        window.invoke_repair_requested();
        assert!(window.get_loading());
        assert!(window.get_repairing());
        assert!(!window.get_settings_visible());
        assert!(!window.get_auth_visible(), "repair needs no login");
        assert_eq!(fs::read(&path).unwrap(), saved_settings);
        window.invoke_repair_requested();
        assert!(window.get_repairing(), "duplicate repair stays locked");
        render(&window, &adapter, "repair-started");
        // The test core simulates preparation without a Minecraft process.
        let states = controller.core.subscribe_state();
        pump_until(&events, || {
            states.borrow().status() == launcher_core::LauncherStatus::Ready
        });
        crate::apply_launcher_state(&window, &states.borrow());
        assert!(!window.get_loading());
        assert!(!window.get_repairing());
        assert_eq!(window.get_launcher_status(), "Восстановлено");
        assert_eq!(fs::read(&path).unwrap(), saved_settings);
        render(&window, &adapter, "repair-complete");

        // Cancellation locks immediately, survives progress updates, and is
        // released only after the core acknowledges the operation has ended.
        window.invoke_repair_requested();
        pump_until(&events, || {
            states.borrow().status() == launcher_core::LauncherStatus::Checking
        });
        crate::apply_launcher_state(&window, &states.borrow());
        assert!(window.get_cancel_allowed());
        let checking = states.borrow().clone();
        window.invoke_cancel_requested();
        assert!(window.get_cancel_pending());
        window.invoke_cancel_requested();
        crate::apply_launcher_state(&window, &checking);
        assert!(window.get_cancel_pending());
        render(&window, &adapter, "preparation-cancelling");
        pump_until(&events, || {
            states.borrow().status() == launcher_core::LauncherStatus::Ready
        });
        crate::apply_launcher_state(&window, &states.borrow());
        assert!(!window.get_cancel_pending());
        assert!(!window.get_loading());
        assert_eq!(window.get_launcher_status(), "Отменено");
        assert_eq!(fs::read(&path).unwrap(), saved_settings);

        // Retrying preserves which operation failed, including after the
        // transient `repairing` flag has been cleared by an error state.
        let repair_retries = Rc::new(Cell::new(0));
        let play_retries = Rc::new(Cell::new(0));
        let count = repair_retries.clone();
        window.on_repair_requested(move || count.set(count.get() + 1));
        let count = play_retries.clone();
        window.on_play_requested(move |_| count.set(count.get() + 1));
        window.set_retry_available(true);
        window.set_retry_repair(true);
        window.set_repairing(false);
        window.invoke_retry_requested();
        assert_eq!(repair_retries.get(), 1);
        assert_eq!(play_retries.get(), 0);
        window.set_retry_repair(false);
        window.invoke_retry_requested();
        assert_eq!(play_retries.get(), 1);
        window.set_loading(true);
        window.invoke_retry_requested();
        assert_eq!(play_retries.get(), 1, "retry is blocked during preparation");
        crate::apply_launcher_state(&window, &states.borrow());
        crate::bind_repair_command(
            &window,
            controller.core.clone(),
            runtime.handle().clone(),
            diagnostics.clone(),
        );
        window.set_auth_verified(true);
        window.invoke_settings_requested();

        // Exercise the real TextInput binding, including replacing selected text
        // and arrow keys, rather than only calling the Rust callbacks.
        let position = slint::LogicalPosition::new(850.0, 225.0);
        window.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: slint::platform::PointerEventButton::Left,
        });
        window
            .window()
            .dispatch_event(WindowEvent::PointerReleased {
                position,
                button: slint::platform::PointerEventButton::Left,
            });
        window.window().dispatch_event(WindowEvent::KeyPressed {
            text: slint::platform::Key::Control.into(),
        });
        key(&window, "a");
        window.window().dispatch_event(WindowEvent::KeyReleased {
            text: slint::platform::Key::Control.into(),
        });
        key(&window, "4097");
        assert_eq!(window.get_memory_input(), "4097");
        assert_eq!(window.get_memory_value(), 4097);
        key(&window, slint::platform::Key::UpArrow);
        assert_eq!(window.get_memory_input(), "4353");
        key(&window, slint::platform::Key::Escape);
        assert!(!window.get_settings_visible());
        render(&window, &adapter, "main");
        let position = slint::LogicalPosition::new(900.0, 20.0);
        window.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: slint::platform::PointerEventButton::Left,
        });
        window
            .window()
            .dispatch_event(WindowEvent::PointerReleased {
                position,
                button: slint::platform::PointerEventButton::Left,
            });
        assert!(window.get_auth_visible());
        window.set_auth_visible(false);
        key(&window, slint::platform::Key::Tab);
        key(&window, slint::platform::Key::Return);
        assert!(window.get_settings_visible());
        assert_eq!(window.get_memory_input(), "6144");

        window.invoke_memory_input_edited("6143".into());
        assert_eq!(window.get_memory_value(), 6143);
        window.invoke_memory_slider_changed(4000.0);
        assert_eq!(window.get_memory_input(), "4096");
        window.invoke_memory_stepped(1);
        assert_eq!(window.get_memory_input(), "4352");
        for input in ["", "-2048", "2048.5", "letters", "999999999999", "6145"] {
            window.invoke_memory_input_edited(input.into());
            assert!(!window.get_memory_save_enabled());
            assert!(!window.get_memory_error().is_empty());
            window.invoke_memory_save_requested();
            assert!(window.get_settings_visible());
            assert_eq!(settings.memory_mib(), None);
        }
        window.invoke_memory_input_edited("6143".into());
        window.invoke_memory_back_requested();
        window.invoke_settings_requested();
        assert_eq!(window.get_memory_input(), "6144");
        window.invoke_memory_input_edited("6143".into());
        window.invoke_memory_save_requested();
        assert!(!window.get_settings_visible());
        let (restored, profile) = ProfileController::load(SettingsStore::new(&path));
        assert_eq!(restored.memory_mib(), Some(6143));
        assert_eq!(profile.unwrap().username(), "TestPlayer");

        settings.save_memory(8192).unwrap();
        window.invoke_settings_requested();
        assert!(!window.get_memory_ready());
        assert_eq!(window.get_memory_input(), "8192");
        assert!(!window.get_memory_suggestion().is_empty());
        render(&window, &adapter, "settings-over-limit");
        window.invoke_memory_suggestion_requested();
        assert_eq!(window.get_memory_input(), "6144");
        assert!(!window.get_memory_ready());
        window.invoke_memory_save_requested();
        assert!(window.get_memory_ready());

        window.invoke_settings_requested();
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        window.invoke_memory_input_edited("4096".into());
        window.invoke_memory_save_requested();
        assert!(window.get_settings_visible());
        assert!(window.get_memory_error().contains("Не удалось сохранить"));
        assert_eq!(settings.memory_mib(), Some(6144));
        render(&window, &adapter, "settings-save-error");

        let mut failed = MemorySettingsController::new(settings, controller.core.clone(), &config);
        Rc::get_mut(&mut failed).unwrap().query_memory = unavailable;
        failed.bind(&window);
        window.invoke_settings_requested();
        assert!(!window.get_memory_ready());
        assert!(!window.get_memory_detected());
        assert!(!window.get_memory_save_enabled());
        assert!(!failed.prepare_play(&window));
        render(&window, &adapter, "settings-detection-error");

        // Verify immediate locking and unlocking through the real Play callback
        // and the UI event queue, using a core without an actual game process.
        controller.bind(&window);
        window.set_settings_visible(false);
        crate::apply_launcher_state(&window, &controller.core.subscribe_state().borrow());
        let gate = launcher_core::auth::telegram::TelegramAuthGate::new(
            launcher_core::auth::telegram::TelegramAuthClient::new(
                launcher_core::build_http_client().unwrap(),
                "http://127.0.0.1".parse().unwrap(),
            ),
        );
        gate.set_session(Some(launcher_core::auth::telegram::TelegramSession {
            token: "test-only-session".into(),
            expires_at: 9999999999999,
            user: launcher_core::auth::telegram::TelegramUser {
                login: "tg_test".into(),
                nickname: "TestPlayer".into(),
                registration_status: "pending".into(),
            },
        }));
        window.set_auth_verified(true);
        crate::bind_play_command(
            &window,
            controller.core.clone(),
            runtime.handle().clone(),
            controller.settings.clone(),
            controller.clone(),
            diagnostics.clone(),
            gate,
        );
        window.set_auth_verified(false);
        window.invoke_play_requested("OtherPlayer".into());
        assert!(window.get_auth_visible());
        assert!(
            !window.get_loading(),
            "Play must refuse an unauthenticated user"
        );
        window.set_auth_visible(false);
        window.set_auth_verified(true);
        crate::listen_for_state_updates(
            &window,
            controller.core.subscribe_state(),
            runtime.handle(),
            controller.clone(),
            diagnostics.clone(),
            crate::server_monitor::ServerMonitor::start(&window, runtime.handle(), ""),
        );
        render(&window, &adapter, "main-ready");
        click(&window, 590.0, 545.0);
        assert!(window.get_loading());
        assert!(!window.get_play_enabled());
        click(&window, 1042.0, 30.0);
        assert!(!window.get_settings_visible());
        assert!(window.get_preparing());
        window.invoke_play_requested("OtherPlayer".into());
        assert_eq!(window.get_nickname_input(), "TestPlayer");
        click(&window, 900.0, 20.0);
        window.window().dispatch_event(WindowEvent::KeyPressed {
            text: slint::platform::Key::Control.into(),
        });
        key(&window, "a");
        window.window().dispatch_event(WindowEvent::KeyReleased {
            text: slint::platform::Key::Control.into(),
        });
        key(&window, "ChangedPlayer");
        assert_eq!(window.get_nickname_input(), "TestPlayer");
        render(&window, &adapter, "main-launching");
        pump_until(&events, || !window.get_loading());
        assert!(window.get_play_enabled());
        assert!(!window.get_preparing());
        click(&window, 1042.0, 30.0);
        assert!(window.get_settings_visible());
        window.invoke_memory_back_requested();

        // Preview the actual UI bindings for unknown-size ZIP downloads,
        // paused transfers, and a named mod being extracted.
        window.set_loading(true);
        window.set_preparing(true);
        window.set_play_enabled(false);
        let download = launcher_core::ModsSyncProgress {
            stage: launcher_core::ModsSyncStage::Downloading,
            current_file: Some("mods.zip".to_owned()),
            downloaded: 48 * 1024 * 1024,
            bytes_per_second: 512 * 1024,
            elapsed: Duration::from_secs(90),
            ..Default::default()
        };
        crate::apply_mods_progress(&window, &download);
        assert!(
            window
                .get_launcher_detail()
                .contains("сервер не сообщил размер")
        );
        assert!(window.get_progress_metrics().contains("48,0 МБ"));
        assert!(!window.get_has_progress());
        render(&window, &adapter, "mods-downloading");
        let waiting = launcher_core::ModsSyncProgress {
            idle_for: Duration::from_secs(17),
            bytes_per_second: 0,
            ..download
        };
        crate::apply_mods_progress(&window, &waiting);
        assert!(window.get_launcher_detail().contains("17 с"));
        render(&window, &adapter, "mods-waiting");
        let extracting = launcher_core::ModsSyncProgress {
            stage: launcher_core::ModsSyncStage::Extracting,
            current_file: Some("example-mod-neoforge-1.21.1.jar".to_owned()),
            files_completed: 37,
            files_total: Some(112),
            file_bytes: 2 * 1024 * 1024,
            file_total: Some(8 * 1024 * 1024),
            elapsed: Duration::from_secs(128),
            ..Default::default()
        };
        crate::apply_mods_progress(&window, &extracting);
        assert!(
            window
                .get_launcher_detail()
                .contains("example-mod-neoforge-1.21.1.jar")
        );
        assert!(window.get_progress_metrics().contains("37 / 112"));
        assert!(window.get_has_progress());
        render(&window, &adapter, "mods-installing");
        let committing = launcher_core::ModsSyncProgress {
            stage: launcher_core::ModsSyncStage::Committing,
            ..extracting
        };
        crate::apply_mods_progress(&window, &committing);
        assert!(!window.get_cancel_allowed());
        assert!(
            window
                .get_launcher_detail()
                .contains("отмена временно недоступна")
        );
        render(&window, &adapter, "preparation-committing");
        crate::apply_launcher_state(&window, &controller.core.subscribe_state().borrow());

        // The error dialog includes process output immediately, then appends
        // the launcher file asynchronously without overwriting a newer launch.
        let sample_log = "2026-10-14 ERROR Minecraft exited with an error: 23\n".repeat(100);
        fs::write(root.path().join("launcher.log"), &sample_log).unwrap();
        let diagnostic = launcher_core::LauncherDiagnostic {
            message: "Minecraft завершился с ошибкой: код 23".to_owned(),
            log_tail: "[stdout] Starting Minecraft\n[stderr] Missing mod dependency\n".to_owned(),
        };
        diagnostics.show(
            &window,
            "Minecraft завершился с ошибкой",
            &diagnostic,
            "Этап: Игра запущена\nMinecraft 1.21.1 • NeoForge 21.1.248 • Java 21",
        );
        diagnostics.set_guidance(
            &window,
            Some(&launcher_core::LauncherFailure::GameExit { code: Some(23) }),
            &diagnostic,
        );
        assert!(window.get_diagnostics_repair_action());
        window.set_retry_available(true);
        assert!(window.get_diagnostics_visible());
        assert!(
            window
                .get_diagnostics_text()
                .contains("Missing mod dependency")
        );
        pump_until(&events, || !window.get_diagnostics_refreshing());
        assert!(window.get_diagnostics_text().contains(&sample_log));
        assert!(!window.get_diagnostics_details_visible());
        render(&window, &adapter, "diagnostics-summary");
        click(&window, 220.0, 266.0);
        assert!(window.get_diagnostics_details_visible());
        render(&window, &adapter, "diagnostics-crash");
        diagnostics.set_guidance(
            &window,
            Some(&launcher_core::LauncherFailure::LaunchOptions),
            &diagnostic,
        );
        assert!(window.get_diagnostics_memory_action());
        assert!(!window.get_diagnostics_repair_action());
        window.set_diagnostics_details_visible(false);
        render(&window, &adapter, "diagnostics-memory");
        click(&window, 440.0, 266.0);
        assert!(window.get_settings_visible());
        assert!(!window.get_diagnostics_visible());
        window.invoke_memory_back_requested();
        window.invoke_diagnostics_requested();
        click(&window, 992.0, 673.0);
        assert!(!window.get_diagnostics_visible());
        assert!(window.get_diagnostics_available());
        window.invoke_diagnostics_requested();
        assert!(window.get_diagnostics_visible());
        diagnostics.clear(&window);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let queued = !events.lock().unwrap().is_empty();
            if queued {
                break;
            }
            assert!(Instant::now() < deadline, "old log read did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
        let event = events.lock().unwrap().pop_front().unwrap();
        event();
        assert!(!window.get_diagnostics_visible());
        assert!(window.get_diagnostics_text().is_empty());

        // The real update controller checks in the background, defers its offer
        // while a game is active, and presents working mouse/keyboard controls.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let update_base = format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let response_sent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sent = response_sent.clone();
        runtime.spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            let count = stream.read(&mut request).await.unwrap();
            assert!(count > 0);
            let body = serde_json::json!({"version":"9.0.0","url":"/download/launcher/9.0.0.zip","sha256":"0".repeat(64),"size":10485760}).to_string();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
            sent.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        window.set_loading(true);
        let updates = crate::update_controller::UpdateController::bind(
            &window,
            runtime.handle().clone(),
            launcher_core::launcher_update::UpdateClient::new(update_base).unwrap(),
        );
        pump_until(&events, || {
            response_sent.load(std::sync::atomic::Ordering::SeqCst)
        });
        assert!(!window.get_update_visible());
        window.set_loading(false);
        pump_until(&events, || window.get_update_visible());
        assert_eq!(window.get_update_version(), "9.0.0");
        render(&window, &adapter, "update-available");
        let installs = Rc::new(Cell::new(0));
        let count = installs.clone();
        window.on_update_install_requested(move || count.set(count.get() + 1));
        click(&window, 465.0, 508.0);
        assert_eq!(installs.get(), 1, "update action works on its first click");
        window.invoke_play_requested("TestPlayer".into());
        assert!(
            !window.get_loading(),
            "update dialog must block Play callback"
        );
        window.set_update_busy(true);
        window.set_update_progress(0.42);
        window.set_update_message("Загрузка: 4,2 из 10,0 МБ".into());
        render(&window, &adapter, "update-downloading");
        window.set_update_can_cancel(false);
        click(&window, 775.0, 508.0);
        assert!(
            window.get_update_visible(),
            "cannot dismiss during helper handoff"
        );
        window.set_update_busy(false);
        window.set_update_error(true);
        window.set_update_message(
            "Контрольная сумма обновления не совпадает. Текущая версия продолжает работать.".into(),
        );
        render(&window, &adapter, "update-error");
        click(&window, 775.0, 508.0);
        assert!(!window.get_update_visible());
        drop(updates);
    }
}
