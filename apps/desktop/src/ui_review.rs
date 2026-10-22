//! Opt-in native-window review. Uses real backend data and an isolated settings directory.
use super::*;
use slint::platform::{PointerEventButton, WindowEvent};
use std::{cell::Cell, path::PathBuf, time::Duration};

fn click(window: &AppWindow, x: f32, y: f32) {
    let position = LogicalPosition::new(x, y);
    window
        .window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    window.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    window
        .window()
        .dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
}

fn snapshot(window: &AppWindow, name: &str) {
    let directory = PathBuf::from(
        std::env::var_os("LAUNCHER_UI_PREVIEW_DIR").expect("Set LAUNCHER_UI_PREVIEW_DIR"),
    );
    std::fs::create_dir_all(&directory).unwrap();
    let pixels = window.window().take_snapshot().unwrap();
    assert!(
        pixels
            .as_bytes()
            .chunks_exact(4)
            .filter(|p| p[0] > 40)
            .take(1000)
            .count()
            == 1000,
        "Snapshot must contain a rendered frame"
    );
    image::save_buffer(
        directory.join(format!("native-{name}.png")),
        pixels.as_bytes(),
        pixels.width(),
        pixels.height(),
        image::ColorType::Rgba8,
    )
    .unwrap();
}

#[test]
#[ignore = "Opens a native window; run separately with LAUNCHER_DATA_DIR pointing to a temporary review folder"]
fn native_window_review() {
    assert!(
        std::env::var_os("LAUNCHER_DATA_DIR").is_some(),
        "Use isolated review data"
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let configured = runtime
        .block_on(configure_launcher(
            runtime.handle(),
            DesktopAppConfig::load().unwrap(),
            launcher_core::java_install::JavaSetup::channel().0,
        ))
        .unwrap();
    let window = AppWindow::new().unwrap();
    apply_static_display_state(&window, &configured.display);
    apply_launcher_state(&window, &configured.core.subscribe_state().borrow());
    bind_window_callbacks(&window);
    let profile = Rc::new(configured.profile_controller);
    let memory = MemorySettingsController::new(profile, configured.core, &configured.client_config);
    memory.bind(&window);
    let _monitor = ServerMonitor::start(
        &window,
        runtime.handle(),
        configured.client_config.server_address().unwrap_or(""),
    );
    let _telegram = telegram_login::TelegramLogin::bind(
        &window,
        runtime.handle().clone(),
        configured.telegram_gate,
        LauncherCore::new(runtime.handle()),
        configured.session_store,
    );
    let weak = window.as_weak();
    let plays = Rc::new(Cell::new(0));
    let played = plays.clone();
    window.on_play_requested(move |_| played.set(played.get() + 1));
    window
        .window()
        .set_size(slint::LogicalSize::new(1180., 787.));
    let stage = Rc::new(Cell::new(0));
    let final_stage = stage.clone();
    let original_position = Cell::new(LogicalPosition::new(0., 0.));
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(800),
        move || {
            let window = weak.upgrade().unwrap();
            match stage.get() {
                0 => window.window().request_redraw(),
                1 => snapshot(&window, "home"),
                2 => {
                    click(&window, 1025., 28.);
                    assert!(window.get_settings_visible());
                }
                3 => {
                    snapshot(&window, "settings");
                    window.invoke_memory_back_requested();
                    click(&window, 900., 28.);
                    assert!(window.get_auth_visible());
                }
                4 => {
                    snapshot(&window, "account");
                    window.invoke_auth_back_requested();
                }
                5 => {
                    let position = window
                        .window()
                        .position()
                        .to_logical(window.window().scale_factor());
                    original_position.set(position);
                    window.window().dispatch_event(WindowEvent::PointerPressed {
                        position: LogicalPosition::new(450., 20.),
                        button: PointerEventButton::Left,
                    });
                    window.window().dispatch_event(WindowEvent::PointerMoved {
                        position: LogicalPosition::new(470., 40.),
                    });
                    window
                        .window()
                        .dispatch_event(WindowEvent::PointerReleased {
                            position: LogicalPosition::new(470., 40.),
                            button: PointerEventButton::Left,
                        });
                }
                6 => {
                    let position = window
                        .window()
                        .position()
                        .to_logical(window.window().scale_factor());
                    assert!(
                        (position.x - original_position.get().x - 20.).abs() < 2.,
                        "Title drag changes window position"
                    );
                    click(&window, 1090., 28.);
                }
                7 => {
                    assert!(window.window().is_minimized());
                    window.window().set_minimized(false);
                }
                8 => {
                    window
                        .window()
                        .set_size(slint::LogicalSize::new(1536., 1024.));
                }
                9 => {
                    snapshot(&window, "reference-size");
                    window
                        .window()
                        .set_size(slint::LogicalSize::new(1180., 787.));
                }
                10 => {
                    click(&window, 600., 575.);
                    assert_eq!(plays.get(), 1, "Play is connected");
                    window.set_loading(true);
                    window.set_has_progress(true);
                    window.set_progress(0.42);
                    window.set_progress_percent(42);
                    window.set_launcher_status("Загрузка".into());
                    window.set_launcher_detail("mods.zip".into());
                }
                11 => {
                    snapshot(&window, "progress");
                    click(&window, 1150., 28.);
                }
                _ => panic!("Close must leave the event loop"),
            }
            window.window().request_redraw();
            stage.set(stage.get() + 1);
        },
    );
    window.run().unwrap();
    assert_eq!(final_stage.get(), 12, "Window close ended the event loop");
}
