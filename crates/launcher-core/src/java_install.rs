//! Download a pinned Java installer and coordinate desktop consent after mods sync.
use crate::InstallCancellation;
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncWriteExt,
    sync::{mpsc, oneshot},
};

pub const JAVA21_INSTALLER_SIZE: u64 = 179_679_232;
pub const JAVA21_INSTALLER_SHA256: &str =
    "264db89e74213f3ea2d7b7379d1c2ac346797d03b4d88cbf4ce72c3ff96477a1";

#[derive(Clone, Debug)]
pub struct JavaInstallerSource {
    url: reqwest::Url,
    size: u64,
    sha256: String,
}

impl JavaInstallerSource {
    pub fn new(url: &str) -> Result<Self, String> {
        let url = reqwest::Url::parse(url).map_err(|_| {
            "Укажите прямую HTTPS-ссылку на MSI в java21_msi_url файла launcher-config.json."
                .to_owned()
        })?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(
                "Адрес установщика Java должен быть HTTPS URL без логина, пароля и fragment."
                    .into(),
            );
        }
        Ok(Self {
            url,
            size: JAVA21_INSTALLER_SIZE,
            sha256: JAVA21_INSTALLER_SHA256.into(),
        })
    }

    pub async fn download(
        &self,
        cancellation: &InstallCancellation,
        on_progress: impl Fn(u64, u64),
    ) -> Result<DownloadedJavaInstaller, String> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 5 || attempt.url().scheme() != "https" {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(|e| e.to_string())?;
        let directory = tempfile::Builder::new()
            .prefix("EquestrianDream-java-")
            .tempdir()
            .map_err(|e| e.to_string())?;
        let partial = directory.path().join("java21.part");
        let mut file = tokio::fs::File::create(&partial)
            .await
            .map_err(|e| e.to_string())?;
        let verified: Result<(), String> = async {
        let mut response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err("Загрузка Java отменена.".into()),
            result = client.get(self.url.clone()).send() => result.map_err(|e| format!("Не удалось скачать Java: {e}"))?,
        };
        if response.status() != reqwest::StatusCode::OK {
            return Err(format!(
                "Хост установщика Java вернул HTTP {}.",
                response.status()
            ));
        }
        if response
            .content_length()
            .is_some_and(|size| size != self.size)
        {
            return Err("Размер установщика Java не совпадает с ожидаемым.".into());
        }
        let mut received = 0u64;
        let mut hash = Sha256::new();
        let mut last_report = Instant::now();
        on_progress(0, self.size);
        loop {
            let chunk = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err("Загрузка Java отменена.".into()),
                result = response.chunk() => result.map_err(|e| format!("Загрузка Java прервана: {e}"))?,
            };
            let Some(chunk) = chunk else {
                break;
            };
            received = received
                .checked_add(chunk.len() as u64)
                .ok_or("Установщик Java слишком большой")?;
            if received > self.size {
                return Err("Установщик Java превышает допустимый размер.".into());
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err("Загрузка Java отменена.".into()),
                result = file.write_all(&chunk) => result.map_err(|e| format!("Не удалось сохранить Java: {e}"))?,
            }
            hash.update(&chunk);
            if last_report.elapsed() >= Duration::from_millis(250) {
                on_progress(received, self.size);
                last_report = Instant::now();
            }
        }
        if received != self.size || format!("{:x}", hash.finalize()) != self.sha256 {
            return Err(
                "Установщик Java повреждён: размер или SHA-256 не совпадает. Повторите загрузку."
                    .into(),
            );
        }
        file.sync_all().await.map_err(|e| e.to_string())?;
        Ok(())
        }.await;
        // Wait for outstanding Windows file I/O before removing a cancelled or bad download.
        drop(file.into_std().await);
        verified?;
        if cancellation.is_cancelled() {
            return Err("Загрузка Java отменена.".into());
        }
        tokio::fs::rename(&partial, directory.path().join("java21.msi"))
            .await
            .map_err(|e| e.to_string())?;
        on_progress(self.size, self.size);
        Ok(DownloadedJavaInstaller { directory })
    }
}

#[derive(Debug)]
pub struct DownloadedJavaInstaller {
    directory: tempfile::TempDir,
}
impl DownloadedJavaInstaller {
    pub fn path(&self) -> std::path::PathBuf {
        self.directory.path().join("java21.msi")
    }
    /// Call only after Windows Installer has exited and released its source file.
    pub fn cleanup(self) -> Result<(), String> {
        self.directory
            .close()
            .map_err(|e| format!("Не удалось удалить временный установщик Java: {e}"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JavaSetupOutcome {
    Ready,
    Cancelled,
}

pub struct JavaSetupRequest {
    pub required_major: u32,
    pub cancellation: InstallCancellation,
    pub reply: oneshot::Sender<Result<JavaSetupOutcome, String>>,
}

#[derive(Clone, Debug)]
pub struct JavaSetup {
    sender: mpsc::UnboundedSender<JavaSetupRequest>,
}

impl JavaSetup {
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<JavaSetupRequest>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Self { sender }, receiver)
    }

    pub(crate) async fn ensure(&self, required_major: u32) -> Result<JavaSetupOutcome, String> {
        struct CancelOnDrop(InstallCancellation);
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                self.0.cancel();
            }
        }
        let guard = CancelOnDrop(InstallCancellation::default());
        let (reply, response) = oneshot::channel();
        self.sender
            .send(JavaSetupRequest {
                required_major,
                cancellation: guard.0.clone(),
                reply,
            })
            .map_err(|_| "Обработчик установки Java недоступен.".to_owned())?;
        response
            .await
            .map_err(|_| "Окно установки Java закрыто.".to_owned())?
    }
}

pub fn installation_result(code: Option<i32>, log: &Path) -> Result<(), String> {
    match code {
        Some(0 | 3010) => Ok(()),
        Some(1602) => {
            Err("Установка Java отменена. Можно повторить или вернуться в лаунчер.".into())
        }
        Some(1618) => Err(
            "В Windows уже выполняется другая установка. Дождитесь её завершения и повторите."
                .into(),
        ),
        _ => Err(format!(
            "Не удалось установить Java (код {code:?}). Журнал: {}",
            log.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::test_support::{TestResponse, TestServer};

    fn source(url: reqwest::Url, expected: &[u8]) -> JavaInstallerSource {
        JavaInstallerSource {
            url,
            size: expected.len() as u64,
            sha256: format!("{:x}", Sha256::digest(expected)),
        }
    }

    #[test]
    fn requires_https_and_pins_the_requested_java_package() {
        for url in [
            "",
            "http://host/java.msi",
            "file:///java.msi",
            "https://user:pass@host/java.msi",
            "https://host/java.msi#fragment",
        ] {
            assert!(JavaInstallerSource::new(url).is_err());
        }
        let source = JavaInstallerSource::new("https://host/java.msi").unwrap();
        assert_eq!(source.size, JAVA21_INSTALLER_SIZE);
        assert_eq!(source.sha256, JAVA21_INSTALLER_SHA256);
    }

    #[tokio::test]
    async fn verifies_download_without_content_length_and_deletes_installer() {
        let server = TestServer::start().await.unwrap();
        let bytes = vec![42; 8192];
        server.route("/java.msi", TestResponse::ok(bytes.clone()).chunked());
        let source = source(server.url("java.msi").unwrap(), &bytes);
        let progress = std::sync::Mutex::new(Vec::new());
        let installer = source
            .download(&InstallCancellation::default(), |n, total| {
                progress.lock().unwrap().push((n, total));
            })
            .await
            .unwrap();
        let path = installer.path();
        let directory = path.parent().unwrap().to_owned();
        assert_eq!(tokio::fs::read(&path).await.unwrap(), bytes);
        assert_eq!(progress.lock().unwrap().last().copied(), Some((8192, 8192)));
        installer.cleanup().unwrap();
        assert!(!directory.exists());
    }

    #[tokio::test]
    async fn rejects_corruption_http_errors_wrong_size_and_allows_retry() {
        let server = TestServer::start().await.unwrap();
        let source = source(server.url("java.msi").unwrap(), b"good");
        for response in [
            TestResponse::ok(b"evil".to_vec()),
            TestResponse::ok(b"truncated".to_vec()),
            TestResponse::ok(b"x".to_vec()).with_advertised_length(4),
            TestResponse::ok(b"too big".to_vec()).chunked(),
            TestResponse::status(404),
        ] {
            server.route("/java.msi", response);
            assert!(
                source
                    .download(&InstallCancellation::default(), |_, _| {})
                    .await
                    .is_err()
            );
        }
        server.route("/java.msi", TestResponse::ok(b"good".to_vec()));
        source
            .download(&InstallCancellation::default(), |_, _| {})
            .await
            .unwrap()
            .cleanup()
            .unwrap();
    }

    #[tokio::test]
    async fn cancellation_does_not_wait_for_a_stalled_body() {
        let server = TestServer::start().await.unwrap();
        let bytes = vec![1; 1024];
        server.route(
            "/java.msi",
            TestResponse::slow(bytes.clone(), 1, Duration::from_secs(5)),
        );
        let source = source(server.url("java.msi").unwrap(), &bytes);
        let cancel = InstallCancellation::default();
        let (started, ready) = oneshot::channel();
        let started = std::sync::Mutex::new(Some(started));
        let token = cancel.clone();
        let task = tokio::spawn(async move {
            source
                .download(&token, |_, _| {
                    if let Some(tx) = started.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                })
                .await
        });
        ready.await.unwrap();
        cancel.cancel();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
    }

    #[tokio::test]
    async fn setup_waits_for_consent_and_cancels_download_when_core_stops() {
        let (setup, mut requests) = JavaSetup::channel();
        let copy = setup.clone();
        let task = tokio::spawn(async move { copy.ensure(21).await });
        let request = requests.recv().await.unwrap();
        assert_eq!(request.required_major, 21);
        assert!(!task.is_finished());
        task.abort();
        let _ = task.await;
        assert!(request.cancellation.is_cancelled());
        assert!(request.reply.is_closed());
        let task = tokio::spawn(async move { setup.ensure(21).await });
        requests
            .recv()
            .await
            .unwrap()
            .reply
            .send(Ok(JavaSetupOutcome::Ready))
            .unwrap();
        assert_eq!(task.await.unwrap().unwrap(), JavaSetupOutcome::Ready);
    }

    #[test]
    fn cancelled_or_failed_msi_must_not_launch_minecraft() {
        let log = Path::new("java-install.log");
        for code in [0, 3010] {
            assert!(installation_result(Some(code), log).is_ok());
        }
        for code in [1602, 1603, 1618, 1641] {
            assert!(installation_result(Some(code), log).is_err());
        }
        assert!(installation_result(None, log).is_err());
    }
}
