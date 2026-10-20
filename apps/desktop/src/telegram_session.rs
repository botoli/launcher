use atomic_write_file::AtomicWriteFile;
use launcher_core::auth::telegram::TelegramSession;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct SessionStore {
    path: PathBuf,
    server: String,
}
#[derive(Serialize, Deserialize)]
struct Record {
    server: String,
    session: TelegramSession,
}
impl SessionStore {
    pub fn new(path: PathBuf, server: String) -> Self {
        Self { path, server }
    }
    pub fn load(&self) -> io::Result<Option<TelegramSession>> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if bytes.len() > 65536 {
            return Err(io::Error::other("Invalid session size"));
        }
        let decoded = protect(&bytes, false)?;
        let record: Record = serde_json::from_slice(&decoded).map_err(io::Error::other)?;
        // Only migrate the former built-in origin to its verified HTTPS endpoint.
        // The restored token is still validated by /api/auth/me before use.
        let legacy_production = record.server.trim_end_matches('/') == "http://87.58.220.248:3000"
            && self.server.trim_end_matches('/') == "https://89.125.214.122";
        Ok((record.server == self.server || legacy_production).then_some(record.session))
    }
    pub fn save(&self, session: TelegramSession) -> io::Result<()> {
        let bytes = serde_json::to_vec(&Record {
            server: self.server.clone(),
            session,
        })
        .map_err(io::Error::other)?;
        let encrypted = protect(&bytes, true)?;
        let mut file = AtomicWriteFile::open(&self.path)?;
        file.write_all(&encrypted)?;
        file.commit()
    }
    pub fn remove(&self) -> io::Result<()> {
        remove_if_exists(&self.path)
    }
}
fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[cfg(windows)]
fn protect(bytes: &[u8], encrypt: bool) -> io::Result<Vec<u8>> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len().try_into().map_err(io::Error::other)?,
        pbData: bytes.as_ptr().cast_mut(),
    };
    let mut output: CRYPT_INTEGER_BLOB = unsafe { std::mem::zeroed() };
    // DPAPI is scoped to the current Windows user. UI and machine-wide scope are disabled.
    let ok = unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let result =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData.cast());
    }
    Ok(result)
}
#[cfg(not(windows))]
fn protect(_: &[u8], _: bool) -> io::Result<Vec<u8>> {
    Err(io::Error::other("Session storage requires Windows DPAPI"))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use launcher_core::auth::telegram::TelegramUser;
    #[test]
    fn session_is_encrypted_bound_to_server_and_removable() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("session.dat");
        let store = SessionStore::new(path.clone(), "https://one.example".into());
        store.save(TelegramSession {
            token: "secret-never-in-plaintext".into(),
            expires_at: 9999999999999,
            user: TelegramUser {
                login: "tg_1".into(),
                nickname: "Player".into(),
                registration_status: "pending".into(),
            },
        })?;
        assert!(
            !fs::read(&path)?
                .windows(25)
                .any(|s| s == b"secret-never-in-plaintext")
        );
        assert_eq!(store.load()?.unwrap().user.nickname, "Player");
        assert!(
            SessionStore::new(path.clone(), "https://other.example".into())
                .load()?
                .is_none()
        );
        let session = store.load()?.unwrap();
        SessionStore::new(path.clone(), "http://87.58.220.248:3000/".into()).save(session)?;
        assert!(
            SessionStore::new(path.clone(), "https://89.125.214.122".into())
                .load()?
                .is_some()
        );
        assert!(
            SessionStore::new(path.clone(), "https://other.example".into())
                .load()?
                .is_none()
        );
        fs::write(&path, "corrupt")?;
        assert!(store.load().is_err());
        store.remove()?;
        assert!(store.load()?.is_none());
        Ok(())
    }
}
