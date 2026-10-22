use crate::AppWindow;
use launcher_core::java_install::{
    DownloadedJavaInstaller, JavaInstallerSource, JavaSetup, JavaSetupOutcome, JavaSetupRequest,
};
use slint::ComponentHandle;
use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    rc::Rc,
};
use tokio::{runtime::Handle, sync::watch};

pub struct JavaInstallController {
    window: slint::Weak<AppWindow>,
    runtime: Handle,
    logs: PathBuf,
    source: RefCell<String>,
    setup: JavaSetup,
    pending: RefCell<Option<JavaSetupRequest>>,
    generation: Cell<u64>,
    attempt: Cell<u64>,
}
impl JavaInstallController {
    pub fn bind(window: &AppWindow, runtime: Handle, logs: PathBuf) -> Rc<Self> {
        let (setup, mut requests) = JavaSetup::channel();
        let this = Rc::new(Self {
            window: window.as_weak(),
            runtime,
            logs,
            source: RefCell::new(String::new()),
            setup,
            pending: RefCell::new(None),
            generation: Cell::new(0),
            attempt: Cell::new(0),
        });
        window.set_java_can_cancel(true);
        let c = this.clone();
        window.on_java_install_requested(move || c.install());
        let c = this.clone();
        window.on_java_dismiss_requested(move || {
            if let Some(w) = c.window.upgrade()
                && w.get_java_can_cancel()
            {
                if let Some(r) = c.pending.borrow().as_ref() {
                    r.cancellation.cancel();
                }
                c.finish(JavaSetupOutcome::Cancelled);
            }
        });
        let c = this.clone();
        let _ = slint::spawn_local(async move {
            while let Some(request) = requests.recv().await {
                c.check(request);
            }
        });
        this
    }
    pub fn setup(&self) -> JavaSetup {
        self.setup.clone()
    }
    pub fn set_source(&self, url: String) {
        *self.source.borrow_mut() = url;
    }
    fn current(&self, generation: u64) -> bool {
        self.generation.get() == generation && self.pending.borrow().is_some()
    }

    fn check(self: &Rc<Self>, request: JavaSetupRequest) {
        if !cfg!(all(windows, target_arch = "x86_64")) || request.required_major != 21 {
            let _ = request.reply.send(Ok(JavaSetupOutcome::Ready));
            return;
        }
        if self.pending.borrow().is_some() {
            let _ = request
                .reply
                .send(Err("Установка Java уже выполняется.".into()));
            return;
        }
        let cancel = request.cancellation.clone();
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        *self.pending.borrow_mut() = Some(request);
        let cancellation = cancel.clone();
        let cancelled = self
            .runtime
            .spawn(async move { cancellation.cancelled().await });
        let c = self.clone();
        let _ = slint::spawn_local(async move {
            let _ = cancelled.await;
            if c.current(generation)
                && let Some(w) = c.window.upgrade()
                && w.get_java_can_cancel()
            {
                c.finish(JavaSetupOutcome::Cancelled);
            }
        });
        let task = self.runtime.spawn(async move {
            tokio::select! { biased; _ = cancel.cancelled() => None, result = launcher_core::find_java(21) => Some(result) }
        });
        let c = self.clone();
        let _ = slint::spawn_local(async move {
            let result = task.await;
            if !c.current(generation) {
                return;
            }
            let Some(w) = c.window.upgrade() else {
                return;
            };
            match result {
                Ok(Some(Ok(_))) => c.finish(JavaSetupOutcome::Ready),
                Ok(Some(Err(error))) => {
                    tracing::info!(%error, "Java 21 missing after mods synchronization");
                    w.set_java_visible(true);
                    w.set_java_busy(false);
                    w.set_java_can_cancel(true);
                    w.set_java_error(false);
                    w.set_java_progress(0.0);
                    w.set_java_message("Конфигурация и моды проверены. Для запуска нужна Java 21 x64. Скачать и установить Eclipse Temurin OpenJDK 21.0.3+9? После загрузки откроется мастер Windows; может потребоваться разрешение администратора. Оставьте стандартную папку установки. Затем запуск Minecraft продолжится автоматически.".into());
                }
                Ok(None) => c.finish(JavaSetupOutcome::Cancelled),
                Err(error) => c.error(&format!("Не удалось проверить Java: {error}")),
            }
        });
    }
    fn finish(&self, outcome: JavaSetupOutcome) {
        if let Some(w) = self.window.upgrade() {
            w.set_java_visible(false);
            w.set_java_busy(false);
            w.set_java_can_cancel(true);
        }
        if let Some(request) = self.pending.borrow_mut().take() {
            let _ = request.reply.send(Ok(outcome));
        }
    }
    fn error(&self, message: &str) {
        tracing::warn!(message, "Java installation did not complete");
        if let Some(w) = self.window.upgrade() {
            w.set_java_visible(true);
            w.set_java_busy(false);
            w.set_java_can_cancel(true);
            w.set_java_error(true);
            w.set_java_message(message.into());
        }
    }
    fn install(self: &Rc<Self>) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        if !w.get_java_visible() || w.get_java_busy() {
            return;
        }
        let source = match JavaInstallerSource::new(&self.source.borrow()) {
            Ok(s) => s,
            Err(e) => {
                self.error(&e);
                return;
            }
        };
        let Some(cancel) = self
            .pending
            .borrow()
            .as_ref()
            .map(|p| p.cancellation.clone())
        else {
            return;
        };
        w.set_java_busy(true);
        w.set_java_can_cancel(true);
        w.set_java_error(false);
        w.set_java_progress(0.0);
        w.set_java_message("Подключение к хосту установщика Java…".into());
        let generation = self.generation.get();
        let attempt = self.attempt.get().wrapping_add(1);
        self.attempt.set(attempt);
        let (tx, mut rx) =
            watch::channel((0u64, launcher_core::java_install::JAVA21_INSTALLER_SIZE));
        let c = self.clone();
        let _ = slint::spawn_local(async move {
            while rx.changed().await.is_ok() {
                if !c.current(generation) || c.attempt.get() != attempt {
                    break;
                }
                let (bytes, total) = *rx.borrow_and_update();
                if let Some(w) = c.window.upgrade() {
                    if !w.get_java_can_cancel() || !w.get_java_busy() || w.get_java_error() {
                        break;
                    }
                    w.set_java_progress(bytes as f32 / total.max(1) as f32);
                    w.set_java_message(format!("Скачивание Java: {:.1} из {:.1} МБ. Файл будет проверен перед установкой.", bytes as f64 / 1048576.0, total as f64 / 1048576.0).into());
                }
            }
        });
        let task = self.runtime.spawn(async move {
            source
                .download(&cancel, |bytes, total| {
                    tx.send_replace((bytes, total));
                })
                .await
        });
        let c = self.clone();
        let _ = slint::spawn_local(async move {
            let result = task.await;
            if !c.current(generation) {
                return;
            }
            match result {
                Ok(Ok(installer)) => c.run_installer(installer, generation),
                Ok(Err(e)) => c.error(&e),
                Err(e) => c.error(&format!("Ошибка загрузки Java: {e}")),
            }
        });
    }
    fn run_installer(self: &Rc<Self>, installer: DownloadedJavaInstaller, generation: u64) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        if self
            .pending
            .borrow()
            .as_ref()
            .is_none_or(|p| p.cancellation.is_cancelled())
        {
            self.finish(JavaSetupOutcome::Cancelled);
            return;
        }
        // Disable launcher cancellation before native MSI starts; use its own Cancel.
        w.set_java_can_cancel(false);
        w.set_java_message("Java скачана и проверена. Завершите мастер установки Windows. Отменить установку можно в самом мастере.".into());
        let logs = self.logs.clone();
        let task = self.runtime.spawn(async move {
            let result = install_msi(&installer.path(), &logs).await;
            let cleanup = installer.cleanup();
            result?; cleanup?;
            launcher_core::find_java(21).await.map_err(|e| format!("Установщик завершён, но Java 21 не найдена: {e}. Проверьте стандартную папку установки или перезагрузите компьютер."))?;
            Ok::<_,String>(())
        });
        let c = self.clone();
        let _ = slint::spawn_local(async move {
            let result = task.await;
            if !c.current(generation) {
                return;
            }
            if c.pending
                .borrow()
                .as_ref()
                .is_none_or(|p| p.reply.is_closed())
            {
                c.finish(JavaSetupOutcome::Cancelled);
                return;
            }
            match result {
                Ok(Ok(())) => c.finish(JavaSetupOutcome::Ready),
                Ok(Err(e)) => c.error(&e),
                Err(e) => c.error(&format!("Ошибка установщика Java: {e}")),
            }
        });
    }
}
#[cfg(windows)]
async fn install_msi(installer: &Path, logs: &Path) -> Result<(), String> {
    tokio::fs::create_dir_all(logs)
        .await
        .map_err(|e| e.to_string())?;
    let log = logs.join("java-install.log");
    let system = std::env::var_os("SystemRoot").ok_or("Не найден каталог Windows")?;
    let status = tokio::process::Command::new(PathBuf::from(system).join("System32/msiexec.exe"))
        .arg("/i")
        .arg(installer)
        .arg("/norestart")
        .arg("/L*v")
        .arg(&log)
        .status()
        .await
        .map_err(|e| format!("Не удалось запустить установщик Java: {e}"))?;
    launcher_core::java_install::installation_result(status.code(), &log)
}
#[cfg(not(windows))]
async fn install_msi(_installer: &Path, _logs: &Path) -> Result<(), String> {
    Err("Установка Java через MSI поддерживается только в Windows x64.".into())
}
