//! Optional Windows launcher releases, independent of Minecraft/mod downloads.
use crate::InstallCancellation;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;

pub const EXECUTABLE: &str = "Nevermore.exe";
pub const README: &str = "README.txt";
pub const MAX_ARCHIVE_SIZE: u64 = 256 * 1024 * 1024;
const MAX_EXTRACTED_SIZE: u64 = 512 * 1024 * 1024;
const MAX_METADATA: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("Проверка обновления недоступна: {0}")]
    Network(#[from] reqwest::Error),
    #[error("Сервер обновлений вернул HTTP {0}")]
    Http(u16),
    #[error("Некорректные сведения об обновлении")]
    InvalidMetadata,
    #[error("Размер архива обновления не совпадает с опубликованным")]
    Size,
    #[error("Контрольная сумма обновления не совпадает. Файл не установлен")]
    Checksum,
    #[error("Некорректный ZIP обновления: {0}")]
    Archive(#[from] zip::result::ZipError),
    #[error("ZIP должен содержать только Windows x64 EXE и README.txt в корне")]
    Layout,
    #[error(
        "Не удалось подготовить обновление. Проверьте свободное место и права на папку лаунчера: {0}"
    )]
    Io(#[from] io::Error),
    #[error("Обновление отменено")]
    Cancelled,
    #[error("Не удалось завершить проверку обновления")]
    Task,
}

#[derive(Clone, Debug)]
pub struct LauncherRelease {
    version: String,
    url: reqwest::Url,
    sha256: String,
    size: u64,
}
impl LauncherRelease {
    pub fn version(&self) -> &str {
        &self.version
    }
    pub fn size(&self) -> u64 {
        self.size
    }
}
#[derive(Clone)]
pub struct UpdateClient {
    client: reqwest::Client,
    base: reqwest::Url,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateStage {
    Downloading,
    Verifying,
    Extracting,
}
#[derive(Clone, Copy, Debug)]
pub struct UpdateProgress {
    pub stage: UpdateStage,
    pub downloaded: u64,
    pub total: u64,
}
#[derive(Debug)]
pub struct StagedUpdate {
    pub directory: TempDir,
    pub version: String,
    pub executable_sha256: String,
    pub readme_sha256: String,
}

fn cancelled(cancel: &InstallCancellation) -> Result<(), UpdateError> {
    if cancel.is_cancelled() {
        Err(UpdateError::Cancelled)
    } else {
        Ok(())
    }
}
impl UpdateClient {
    pub fn new(base: reqwest::Url) -> Result<Self, UpdateError> {
        // Remote code is accepted only over HTTPS. Plain HTTP is for local fixtures/dev.
        let loopback = matches!(base.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if (base.scheme() != "https" && !(base.scheme() == "http" && loopback))
            || !base.username().is_empty()
            || base.password().is_some()
        {
            return Err(UpdateError::InvalidMetadata);
        }
        Ok(Self {
            client: reqwest::Client::builder()
                // Fail on redirects rather than downloading executable code from another origin.
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(Duration::from_secs(30))
                .user_agent(concat!("Nevermore/", env!("CARGO_PKG_VERSION")))
                .build()?,
            base,
        })
    }

    pub async fn check(&self, current: &str) -> Result<Option<LauncherRelease>, UpdateError> {
        let mut url = self
            .base
            .join("/api/launcher-update")
            .map_err(|_| UpdateError::InvalidMetadata)?;
        url.query_pairs_mut().append_pair("platform", "windows-x64");
        let mut response = self
            .client
            .get(url)
            .timeout(Duration::from_secs(15))
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(UpdateError::Http(response.status().as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_METADATA as u64)
        {
            return Err(UpdateError::InvalidMetadata);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len().saturating_add(chunk.len()) > MAX_METADATA {
                return Err(UpdateError::InvalidMetadata);
            }
            bytes.extend_from_slice(&chunk);
        }
        #[derive(Deserialize)]
        struct Metadata {
            version: String,
            url: String,
            sha256: String,
            size: u64,
        }
        let m: Metadata =
            serde_json::from_slice(&bytes).map_err(|_| UpdateError::InvalidMetadata)?;
        let version =
            semver::Version::parse(&m.version).map_err(|_| UpdateError::InvalidMetadata)?;
        let current = semver::Version::parse(current).map_err(|_| UpdateError::InvalidMetadata)?;
        let url = self
            .base
            .join(&m.url)
            .map_err(|_| UpdateError::InvalidMetadata)?;
        if url.origin() != self.base.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.query().is_some()
            || !url.path().starts_with("/download/launcher/")
            || !url.path().ends_with(".zip")
            || m.size == 0
            || m.size > MAX_ARCHIVE_SIZE
            || m.sha256.len() != 64
            || !m.sha256.bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err(UpdateError::InvalidMetadata);
        }
        if !version.cmp_precedence(&current).is_gt() {
            return Ok(None);
        }
        Ok(Some(LauncherRelease {
            version: m.version,
            url,
            sha256: m.sha256.to_ascii_lowercase(),
            size: m.size,
        }))
    }

    pub async fn stage(
        &self,
        release: &LauncherRelease,
        install_directory: &Path,
        cancel: &InstallCancellation,
        progress: impl Fn(UpdateProgress) + Send + Sync + 'static,
    ) -> Result<StagedUpdate, UpdateError> {
        cancelled(cancel)?;
        if release.url.origin() != self.base.origin() {
            return Err(UpdateError::InvalidMetadata);
        }
        // Same filesystem as the running EXE, so activation uses renames and fails
        // before shutdown if this installation directory is not writable.
        let directory = tempfile::Builder::new()
            .prefix(".launcher-update-")
            .tempdir_in(install_directory)?;
        let archive_path = directory.path().join("release.zip");
        let mut output = tokio::fs::File::create(&archive_path).await?;
        let mut response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(UpdateError::Cancelled),
            result = self.client.get(release.url.clone()).send() => result?,
        };
        if !response.status().is_success() {
            return Err(UpdateError::Http(response.status().as_u16()));
        }
        if response.content_length().is_some_and(|n| n != release.size) {
            return Err(UpdateError::Size);
        }
        let mut received = 0u64;
        let mut hash = Sha256::new();
        let mut last = Instant::now();
        progress(UpdateProgress {
            stage: UpdateStage::Downloading,
            downloaded: 0,
            total: release.size,
        });
        loop {
            let chunk = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(UpdateError::Cancelled),
                result = response.chunk() => result?,
            };
            let Some(chunk) = chunk else {
                break;
            };
            received = received
                .checked_add(chunk.len() as u64)
                .ok_or(UpdateError::Size)?;
            if received > release.size || received > MAX_ARCHIVE_SIZE {
                return Err(UpdateError::Size);
            }
            hash.update(&chunk);
            output.write_all(&chunk).await?;
            // Finish Tokio's buffered write before cancellation can drop staging.
            output.flush().await?;
            if last.elapsed() >= Duration::from_millis(250) {
                progress(UpdateProgress {
                    stage: UpdateStage::Downloading,
                    downloaded: received,
                    total: release.size,
                });
                last = Instant::now();
            }
        }
        output.sync_all().await?;
        drop(output);
        cancelled(cancel)?;
        if received != release.size {
            return Err(UpdateError::Size);
        }
        progress(UpdateProgress {
            stage: UpdateStage::Verifying,
            downloaded: received,
            total: release.size,
        });
        if format!("{:x}", hash.finalize()) != release.sha256 {
            return Err(UpdateError::Checksum);
        }
        progress(UpdateProgress {
            stage: UpdateStage::Extracting,
            downloaded: received,
            total: release.size,
        });
        let version = release.version.clone();
        let cancel = cancel.clone();
        tokio::task::spawn_blocking(move || {
            let (executable_sha256, readme_sha256) =
                unpack(&archive_path, directory.path(), &cancel)?;
            fs::remove_file(archive_path)?;
            Ok(StagedUpdate {
                directory,
                version,
                executable_sha256,
                readme_sha256,
            })
        })
        .await
        .map_err(|_| UpdateError::Task)?
    }
}

fn unpack(
    archive: &Path,
    stage: &Path,
    cancel: &InstallCancellation,
) -> Result<(String, String), UpdateError> {
    let mut archive = zip::ZipArchive::new(File::open(archive)?)?;
    if archive.len() != 2 {
        return Err(UpdateError::Layout);
    }
    fs::create_dir(stage.join("new"))?;
    let mut exe_hash = None;
    let mut readme_hash = None;
    let mut total = 0u64;
    for index in 0..archive.len() {
        cancelled(cancel)?;
        let mut entry = archive.by_index(index)?;
        let name = entry.name().to_owned();
        let slot = match name.as_str() {
            EXECUTABLE if exe_hash.is_none() => &mut exe_hash,
            README if readme_hash.is_none() => &mut readme_hash,
            _ => return Err(UpdateError::Layout),
        };
        let file_type = entry.unix_mode().unwrap_or(0) & 0o170000;
        if !entry.is_file() || !matches!(file_type, 0 | 0o100000) {
            return Err(UpdateError::Layout);
        }
        total = total.checked_add(entry.size()).ok_or(UpdateError::Size)?;
        if total > MAX_EXTRACTED_SIZE
            || (name == README && entry.size() > 1024 * 1024)
            || entry.size() == 0
        {
            return Err(UpdateError::Size);
        }
        let path = stage.join("new").join(&name);
        let mut output = File::create_new(&path)?;
        let mut hash = Sha256::new();
        let mut count = 0u64;
        let mut bytes = [0u8; 64 * 1024];
        loop {
            cancelled(cancel)?;
            let n = entry.read(&mut bytes)?;
            if n == 0 {
                break;
            }
            count += n as u64;
            if count > entry.size() {
                return Err(UpdateError::Size);
            }
            hash.update(&bytes[..n]);
            output.write_all(&bytes[..n])?;
        }
        if count != entry.size() {
            return Err(UpdateError::Size);
        }
        output.sync_all()?;
        *slot = Some(format!("{:x}", hash.finalize()));
    }
    validate_windows_executable(&stage.join("new").join(EXECUTABLE))?;
    Ok((
        exe_hash.ok_or(UpdateError::Layout)?,
        readme_hash.ok_or(UpdateError::Layout)?,
    ))
}

fn validate_windows_executable(path: &Path) -> Result<(), UpdateError> {
    let mut file = File::open(path)?;
    let mut dos = [0u8; 64];
    file.read_exact(&mut dos).map_err(|_| UpdateError::Layout)?;
    if &dos[..2] != b"MZ" {
        return Err(UpdateError::Layout);
    }
    let offset = u32::from_le_bytes(dos[60..64].try_into().unwrap()) as u64;
    if offset < 64 || offset.saturating_add(26) > file.metadata()?.len() {
        return Err(UpdateError::Layout);
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut pe = [0u8; 26];
    file.read_exact(&mut pe)?;
    if &pe[..4] != b"PE\0\0"
        || u16::from_le_bytes([pe[4], pe[5]]) != 0x8664
        || u16::from_le_bytes([pe[24], pe[25]]) != 0x20b
        || pe[22] & 2 == 0
    {
        return Err(UpdateError::Layout);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    type Response = (u16, Vec<u8>);
    async fn server(
        responses: Vec<Response>,
    ) -> (reqwest::Url, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0; 1024];
                    let n = stream.read(&mut buffer).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..n]);
                    if request.windows(4).any(|s| s == b"\r\n\r\n") {
                        break;
                    }
                }
                requests.push(String::from_utf8(request).unwrap());
                stream.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
                stream.write_all(&body).await.unwrap();
            }
            requests
        });
        (base, task)
    }
    fn metadata(version: &str, url: &str, size: u64, hash: &str) -> Vec<u8> {
        serde_json::to_vec(
            &serde_json::json!({"version":version,"url":url,"size":size,"sha256":hash}),
        )
        .unwrap()
    }
    fn pe() -> Vec<u8> {
        let mut bytes = vec![0; 90];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[60..64].copy_from_slice(&64u32.to_le_bytes());
        bytes[64..68].copy_from_slice(b"PE\0\0");
        bytes[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
        bytes[86] = 2;
        bytes[88..90].copy_from_slice(&0x20bu16.to_le_bytes());
        bytes
    }
    fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in files {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }
    fn good_zip() -> Vec<u8> {
        zip(&[(EXECUTABLE, &pe()), (README, b"Launcher instructions")])
    }

    #[tokio::test]
    async fn checks_numeric_version_precedence_and_relative_url_without_credentials() {
        let data = metadata(
            "1.10.0",
            "/download/launcher/1.10.0.zip",
            100,
            &"a".repeat(64),
        );
        let (base, task) = server(vec![(200, data)]).await;
        let client = UpdateClient::new(base.clone()).unwrap();
        let release = client.check("1.9.0").await.unwrap().unwrap();
        assert_eq!(release.version(), "1.10.0");
        assert_eq!(
            release.url,
            base.join("/download/launcher/1.10.0.zip").unwrap()
        );
        let requests = task.await.unwrap();
        assert!(requests[0].starts_with("GET /api/launcher-update?platform=windows-x64 HTTP/1.1"));
        assert!(!requests[0].to_ascii_lowercase().contains("authorization:"));
    }
    #[tokio::test]
    async fn no_publication_equal_build_metadata_and_downgrade_do_not_update() {
        for (status, body) in [
            (404, b"{}".to_vec()),
            (
                200,
                metadata("1.0.0", "/download/launcher/x.zip", 1, &"0".repeat(64)),
            ),
            (
                200,
                metadata("0.9.9", "/download/launcher/x.zip", 1, &"0".repeat(64)),
            ),
            (
                200,
                metadata(
                    "1.0.0+build.9",
                    "/download/launcher/x.zip",
                    1,
                    &"0".repeat(64),
                ),
            ),
        ] {
            let (base, task) = server(vec![(status, body)]).await;
            assert!(
                UpdateClient::new(base)
                    .unwrap()
                    .check("1.0.0")
                    .await
                    .unwrap()
                    .is_none()
            );
            task.await.unwrap();
        }
    }
    #[tokio::test]
    async fn rejects_invalid_metadata_untrusted_urls_and_remote_plain_http() {
        assert!(UpdateClient::new("http://example.com/".parse().unwrap()).is_err());
        for data in [
            metadata(
                "1.1.0",
                "https://other.example/download/launcher/x.zip",
                10,
                &"0".repeat(64),
            ),
            metadata("bad", "/download/launcher/x.zip", 10, &"0".repeat(64)),
            metadata("1.1.0", "/mods.zip", 10, &"0".repeat(64)),
            metadata(
                "1.1.0",
                "/download/launcher/x.zip#fragment",
                10,
                &"0".repeat(64),
            ),
            metadata(
                "1.1.0",
                "/download/launcher/x.zip",
                MAX_ARCHIVE_SIZE + 1,
                &"0".repeat(64),
            ),
            metadata("1.1.0", "/download/launcher/x.zip", 0, &"0".repeat(64)),
            metadata("1.1.0", "/download/launcher/x.zip", 10, "bad"),
            vec![b'x'; MAX_METADATA + 1],
        ] {
            let (base, task) = server(vec![(200, data)]).await;
            assert!(matches!(
                UpdateClient::new(base).unwrap().check("1.0.0").await,
                Err(UpdateError::InvalidMetadata)
            ));
            task.await.unwrap();
        }
    }
    #[tokio::test]
    async fn stages_verified_archive_and_leaves_installed_files_untouched() {
        let zip = good_zip();
        let data = metadata(
            "1.1.0",
            "/download/launcher/1.1.0.zip",
            zip.len() as u64,
            &format!("{:x}", Sha256::digest(&zip)),
        );
        let (base, task) = server(vec![(200, data), (200, zip)]).await;
        let client = UpdateClient::new(base).unwrap();
        let release = client.check("1.0.0").await.unwrap().unwrap();
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(EXECUTABLE), "old executable").unwrap();
        let staged = client
            .stage(
                &release,
                root.path(),
                &InstallCancellation::default(),
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(
            fs::read(staged.directory.path().join("new").join(EXECUTABLE)).unwrap(),
            pe()
        );
        assert_eq!(
            fs::read_to_string(root.path().join(EXECUTABLE)).unwrap(),
            "old executable"
        );
        assert_eq!(
            staged.executable_sha256,
            format!("{:x}", Sha256::digest(pe()))
        );
        assert!(!staged.directory.path().join("release.zip").exists());
        let path = staged.directory.path().to_owned();
        drop(staged);
        assert!(!path.exists());
        assert_eq!(task.await.unwrap().len(), 2);
    }
    #[tokio::test]
    async fn rejects_checksum_and_size_mismatches_without_staging_leftovers() {
        let zip = good_zip();
        for (size, hash) in [
            (zip.len() as u64, "0".repeat(64)),
            (zip.len() as u64 + 1, format!("{:x}", Sha256::digest(&zip))),
        ] {
            let (base, task) = server(vec![
                (
                    200,
                    metadata("1.1.0", "/download/launcher/x.zip", size, &hash),
                ),
                (200, zip.clone()),
            ])
            .await;
            let client = UpdateClient::new(base).unwrap();
            let release = client.check("1.0.0").await.unwrap().unwrap();
            let root = tempfile::tempdir().unwrap();
            assert!(matches!(
                client
                    .stage(
                        &release,
                        root.path(),
                        &InstallCancellation::default(),
                        |_| {}
                    )
                    .await,
                Err(UpdateError::Checksum | UpdateError::Size)
            ));
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
            task.await.unwrap();
        }
    }
    #[test]
    fn rejects_zip_slip_wrappers_duplicates_missing_files_and_non_windows_exe() {
        let mut x86 = pe();
        x86[68..70].copy_from_slice(&0x14cu16.to_le_bytes());
        for bytes in [
            zip(&[("../outside.exe", &pe()), (README, b"readme")]),
            zip(&[("folder/Nevermore.exe", &pe()), (README, b"readme")]),
            zip(&[(EXECUTABLE, &pe())]),
            zip(&[
                (EXECUTABLE, &pe()),
                (README, b"readme"),
                ("extra.dll", b"extra"),
            ]),
            zip(&[(EXECUTABLE, &x86), (README, b"readme")]),
            zip(&[(EXECUTABLE, b"not an EXE"), (README, b"readme")]),
            zip(&[(EXECUTABLE, &pe()), (README, b"")]),
        ] {
            let root = tempfile::tempdir().unwrap();
            let stage = tempfile::tempdir_in(root.path()).unwrap();
            let archive = root.path().join("archive.zip");
            fs::write(&archive, bytes).unwrap();
            assert!(unpack(&archive, stage.path(), &InstallCancellation::default()).is_err());
            assert!(!root.path().join("outside.exe").exists());
        }
    }
    #[tokio::test]
    async fn cancellation_interrupts_waiting_for_response_and_cleans_staging() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let client = UpdateClient::new(base).unwrap();
        let release = LauncherRelease {
            version: "1.1.0".into(),
            url: client.base.join("/download/launcher/x.zip").unwrap(),
            sha256: "0".repeat(64),
            size: 100,
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let cancel = InstallCancellation::default();
        let root = tempfile::tempdir().unwrap();
        let copy = cancel.clone();
        let path = root.path().to_owned();
        let task = tokio::spawn(async move { client.stage(&release, &path, &copy, |_| {}).await });
        rx.await.unwrap();
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(UpdateError::Cancelled)));
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
        server.abort();
    }
}
