use crate::{AppWindow, self_update};
use launcher_core::{
    InstallCancellation,
    launcher_update::{LauncherRelease, UpdateClient, UpdateError, UpdateProgress, UpdateStage},
};
use slint::ComponentHandle;
use std::{cell::RefCell, path::PathBuf, rc::Rc, time::Duration};
use tokio::{runtime::Handle, sync::watch};

pub struct UpdateController {
    window: slint::Weak<AppWindow>,
    runtime: Handle,
    client: UpdateClient,
    release: RefCell<Option<LauncherRelease>>,
    cancellation: RefCell<InstallCancellation>,
    timer: slint::Timer,
}
impl UpdateController {
    pub fn bind(window: &AppWindow, runtime: Handle, client: UpdateClient) -> Rc<Self> {
        window.set_update_current_version(env!("CARGO_PKG_VERSION").into());
        let this = Rc::new(Self {
            window: window.as_weak(),
            runtime,
            client,
            release: RefCell::new(None),
            cancellation: RefCell::new(InstallCancellation::default()),
            timer: slint::Timer::default(),
        });
        let c = this.clone();
        window.on_update_install_requested(move || c.install());
        let c = this.clone();
        window.on_update_dismiss_requested(move || c.dismiss());
        let weak = Rc::downgrade(&this);
        this.timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(500),
            move || {
                if let Some(c) = weak.upgrade() {
                    c.offer_when_idle();
                }
            },
        );
        if cfg!(all(windows, target_arch = "x86_64")) && !self_update::is_rollback_launch() {
            this.check();
        }
        this
    }
    fn check(self: &Rc<Self>) {
        let client = self.client.clone();
        let task = self
            .runtime
            .spawn(async move { client.check(env!("CARGO_PKG_VERSION")).await });
        let c = self.clone();
        let _ = slint::spawn_local(async move {
            match task.await {
                Ok(Ok(release)) => {
                    *c.release.borrow_mut() = release;
                    c.offer_when_idle();
                }
                Ok(Err(error)) => tracing::warn!(%error, "launcher update check skipped"),
                Err(error) => tracing::warn!(%error, "launcher update check task failed"),
            }
        });
    }
    fn offer_when_idle(&self) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        if w.get_loading()
            || w.get_settings_visible()
            || w.get_diagnostics_visible()
            || w.get_auth_busy()
            || w.get_auth_visible()
            || w.get_update_visible()
        {
            return;
        }
        let Some(release) = self.release.borrow().clone() else {
            return;
        };
        w.set_update_version(release.version().into());
        w.set_update_message(format!("Доступна новая версия. Размер загрузки: {:.1} МБ. После установки лаунчер перезапустится.", release.size() as f64 / 1048576.0).into());
        w.set_update_error(false);
        w.set_update_visible(true);
    }
    fn dismiss(&self) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        if w.get_update_busy() {
            if w.get_update_can_cancel() {
                self.cancellation.borrow().cancel();
                w.set_update_can_cancel(false);
                w.set_update_message("Отмена загрузки…".into());
            }
        } else {
            self.release.borrow_mut().take();
            w.set_update_visible(false);
        }
    }
    fn install(self: &Rc<Self>) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        if w.get_loading() || w.get_update_busy() || !w.get_update_visible() {
            return;
        }
        let Some(release) = self.release.borrow().clone() else {
            return;
        };
        let target = match std::env::current_exe().and_then(|p| p.canonicalize()) {
            Ok(path) => path,
            Err(error) => {
                self.error(&error.to_string());
                return;
            }
        };
        let Some(directory) = target.parent().map(PathBuf::from) else {
            return;
        };
        let cancel = InstallCancellation::default();
        *self.cancellation.borrow_mut() = cancel.clone();
        w.set_update_busy(true);
        w.set_update_error(false);
        w.set_update_can_cancel(true);
        w.set_update_progress(0.0);
        w.set_update_message("Подключение к серверу обновлений…".into());
        let (tx, mut rx) = watch::channel::<Option<UpdateProgress>>(None);
        let weak = self.window.clone();
        let _ = slint::spawn_local(async move {
            while rx.changed().await.is_ok() {
                let Some(p) = *rx.borrow_and_update() else {
                    continue;
                };
                let Some(w) = weak.upgrade() else {
                    break;
                };
                if !w.get_update_busy() {
                    continue;
                }
                w.set_update_progress(
                    (p.downloaded as f32 / p.total.max(1) as f32).clamp(0.0, 1.0),
                );
                w.set_update_message(
                    match p.stage {
                        UpdateStage::Downloading => format!(
                            "Загрузка: {:.1} из {:.1} МБ",
                            p.downloaded as f64 / 1048576.0,
                            p.total as f64 / 1048576.0
                        ),
                        UpdateStage::Verifying => "Проверка контрольной суммы…".into(),
                        UpdateStage::Extracting => "Проверка файлов обновления…".into(),
                    }
                    .into(),
                );
            }
        });
        let client = self.client.clone();
        let attempt = cancel.clone();
        let task = self.runtime.spawn(async move {
            client
                .stage(&release, &directory, &attempt, move |p| {
                    tx.send_replace(Some(p));
                })
                .await
        });
        let c = self.clone();
        let _ = slint::spawn_local(async move {
            let result = task.await;
            let Some(w) = c.window.upgrade() else {
                cancel.cancel();
                return;
            };
            let staged = match result {
                Ok(Ok(staged)) if !cancel.is_cancelled() => staged,
                Ok(Err(UpdateError::Cancelled)) | Ok(Ok(_)) => {
                    w.set_update_busy(false);
                    w.set_update_visible(false);
                    c.release.borrow_mut().take();
                    return;
                }
                Ok(Err(error)) => {
                    c.error(&error.to_string());
                    return;
                }
                Err(error) => {
                    c.error(&error.to_string());
                    return;
                }
            };
            w.set_update_can_cancel(false);
            w.set_update_message("Подготовка перезапуска…".into());
            let task = c
                .runtime
                .spawn_blocking(move || self_update::launch_helper(staged, target));
            match task.await {
                Ok(Ok(mut helper)) => {
                    // End the event loop; helper waits for this process (including
                    // runtime and mutex teardown) before touching installed files.
                    if let Err(error) = slint::quit_event_loop() {
                        let _ = helper.kill();
                        let _ = helper.wait();
                        c.error(&error.to_string());
                    }
                }
                Ok(Err(error)) => c.error(&error.to_string()),
                Err(error) => c.error(&error.to_string()),
            }
        });
    }
    fn error(&self, message: &str) {
        tracing::warn!(reason = message, "launcher update was not installed");
        if let Some(w) = self.window.upgrade() {
            w.set_update_busy(false);
            w.set_update_error(true);
            w.set_update_can_cancel(true);
            w.set_update_message(format!("{message}\nТекущая версия продолжает работать. Можно повторить попытку или обновить позже.").into());
        }
    }
}
