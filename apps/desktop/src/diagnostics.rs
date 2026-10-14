use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use launcher_core::{LauncherDiagnostic, LauncherState, LauncherStatus};
use slint::ComponentHandle;
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    runtime::Handle,
};

use crate::{AppWindow, platform};

const MAX_LAUNCHER_TAIL_BYTES: u64 = 32 * 1024;

pub struct DiagnosticsController {
    runtime: Handle,
    log_path: PathBuf,
    prefix: RefCell<String>,
    generation: Arc<AtomicU64>,
}

impl DiagnosticsController {
    pub fn new(runtime: Handle, logs_directory: &Path) -> Rc<Self> {
        Rc::new(Self {
            runtime,
            log_path: logs_directory.join("launcher.log"),
            prefix: RefCell::new(String::new()),
            generation: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn clear(&self, window: &AppWindow) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.prefix.borrow_mut().clear();
        window.set_diagnostics_visible(false);
        window.set_diagnostics_available(false);
        window.set_diagnostics_refreshing(false);
        window.set_diagnostics_text("".into());
    }

    pub fn observe_state(&self, window: &AppWindow, state: &LauncherState) {
        if state.status() != LauncherStatus::Error {
            return;
        }
        let diagnostic = state.diagnostic().cloned().unwrap_or_else(|| {
            LauncherDiagnostic::message(window.get_launcher_detail().to_string())
        });
        let mut context = String::new();
        if let Some(stage) = state.stage() {
            context.push_str(&format!("Этап: {}\n", crate::launch_stage_text(stage).0));
        }
        if let Some(config) = state.client_config() {
            context.push_str(&format!(
                "Minecraft {} • {} {} • Java {}\n",
                config.version, config.loader, config.loader_version, config.java_version
            ));
        }
        if let Some(progress) = state.mods_progress() {
            let (operation, detail, metrics) = crate::mods_progress_text(progress);
            context.push_str(&format!("Операция: {operation}\n{detail}\n{metrics}\n"));
        }
        self.show(window, &window.get_launcher_status(), &diagnostic, &context);
    }

    pub fn show(
        &self,
        window: &AppWindow,
        summary: &str,
        diagnostic: &LauncherDiagnostic,
        context: &str,
    ) {
        let mut text = format!("{context}\n{}\n", diagnostic.message);
        if !diagnostic.log_tail.is_empty() {
            text.push_str("\nПоследний вывод Minecraft:\n");
            text.push_str(&diagnostic.log_tail);
        }
        self.prefix.replace(text.clone());
        window.set_diagnostics_summary(summary.into());
        window.set_diagnostics_text(text.into());
        window.set_diagnostics_available(true);
        window.set_settings_visible(false);
        window.set_diagnostics_visible(true);
        self.refresh(window);
    }

    fn refresh(&self, window: &AppWindow) {
        let request = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let generation = self.generation.clone();
        let path = self.log_path.clone();
        let prefix = self.prefix.borrow().clone();
        let weak = window.as_weak();
        window.set_diagnostics_refreshing(true);
        self.runtime.spawn(async move {
            let result = read_launcher_tail(&path).await;
            let text = match result {
                Ok(tail) if !tail.is_empty() => {
                    format!("{prefix}\nПоследние строки launcher.log:\n{tail}")
                }
                Ok(_) => format!(
                    "{prefix}\nlauncher.log пока пуст. Нажмите «Обновить» после записи лога."
                ),
                Err(error) => format!("{prefix}\nНе удалось прочитать launcher.log: {error}"),
            };
            let _ = slint::invoke_from_event_loop(move || {
                if generation.load(Ordering::Relaxed) == request
                    && let Some(window) = weak.upgrade()
                {
                    window.set_diagnostics_text(text.into());
                    window.set_diagnostics_refreshing(false);
                }
            });
        });
    }

    pub fn bind(self: &Rc<Self>, window: &AppWindow) {
        window.set_launcher_log_path(self.log_path.display().to_string().into());
        let weak = window.as_weak();
        window.on_diagnostics_close_requested(move || {
            if let Some(window) = weak.upgrade() {
                window.set_diagnostics_visible(false);
            }
        });
        let weak = window.as_weak();
        let controller = self.clone();
        window.on_diagnostics_requested(move || {
            if let Some(window) = weak.upgrade()
                && window.get_diagnostics_available()
            {
                window.set_settings_visible(false);
                window.set_diagnostics_visible(true);
                controller.refresh(&window);
            }
        });
        let weak = window.as_weak();
        let controller = self.clone();
        window.on_diagnostics_refresh_requested(move || {
            if let Some(window) = weak.upgrade() {
                controller.refresh(&window);
            }
        });
        let weak = window.as_weak();
        let directory = self
            .log_path
            .parent()
            .expect("log path has a parent")
            .to_owned();
        window.on_logs_folder_requested(move || {
            if let Err(error) = platform::open_directory(&directory) {
                tracing::warn!(%error, "failed to open logs directory");
                if let Some(window) = weak.upgrade() {
                    window.set_diagnostics_text(
                        format!(
                            "{}\nНе удалось открыть папку логов: {error}",
                            window.get_diagnostics_text()
                        )
                        .into(),
                    );
                }
            }
        });
    }
}

async fn read_launcher_tail(path: &Path) -> std::io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let length = file.metadata().await?.len();
    file.seek(std::io::SeekFrom::Start(
        length.saturating_sub(MAX_LAUNCHER_TAIL_BYTES),
    ))
    .await?;
    let mut bytes = Vec::new();
    file.take(MAX_LAUNCHER_TAIL_BYTES)
        .read_to_end(&mut bytes)
        .await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_only_the_tail_of_a_large_log_and_handles_missing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("launcher.log");
        assert!(read_launcher_tail(&path).await.is_err());
        let mut text = "old line\n".repeat(10000);
        text.push_str("latest failure\n");
        tokio::fs::write(&path, text).await.unwrap();
        let tail = read_launcher_tail(&path).await.unwrap();
        assert!(tail.len() <= MAX_LAUNCHER_TAIL_BYTES as usize);
        assert!(tail.ends_with("latest failure\n"));
    }
}
