use std::io;

const SINGLE_INSTANCE_MUTEX_NAME: &str =
    "Local\\EquestrianDreamLauncher-4A8CF876-8460-4C22-8771-657F401FBFA0";

pub enum SingleInstanceState {
    Acquired(SingleInstanceGuard),
    AlreadyRunning,
}

pub struct SingleInstanceGuard {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
}

pub fn acquire_single_instance() -> io::Result<SingleInstanceState> {
    acquire_named_instance(SINGLE_INSTANCE_MUTEX_NAME)
}

#[cfg(windows)]
fn acquire_named_instance(name: &str) -> io::Result<SingleInstanceState> {
    use std::ptr;
    use windows_sys::Win32::{
        Foundation::{ERROR_ALREADY_EXISTS, GetLastError},
        System::Threading::CreateMutexW,
    };

    let name = null_terminated_utf16(name);
    // SAFETY: the mutex name is a valid, NUL-terminated UTF-16 buffer that
    // remains alive for the duration of the call. No security descriptor is used.
    let handle = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: GetLastError has no preconditions and is called immediately after
    // CreateMutexW, before another Windows API call can replace the thread error.
    let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already_exists {
        // SAFETY: CreateMutexW returned a valid owned handle above.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(handle);
        }
        return Ok(SingleInstanceState::AlreadyRunning);
    }

    Ok(SingleInstanceState::Acquired(SingleInstanceGuard {
        handle,
    }))
}

#[cfg(not(windows))]
fn acquire_named_instance(_name: &str) -> io::Result<SingleInstanceState> {
    Ok(SingleInstanceState::Acquired(SingleInstanceGuard {}))
}

#[cfg(windows)]
impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        // SAFETY: the guard exclusively owns the valid handle returned by
        // CreateMutexW and closes it exactly once during drop.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

pub fn show_error_message(title: &str, message: &str) {
    #[cfg(windows)]
    {
        use std::ptr;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MessageBoxW,
        };

        let title = null_terminated_utf16(title);
        let message = null_terminated_utf16(message);
        // SAFETY: both strings are valid, NUL-terminated UTF-16 buffers and no
        // owner window is required for this process-level error dialog.
        unsafe {
            MessageBoxW(
                ptr::null_mut(),
                message.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
            );
        }
    }

    #[cfg(not(windows))]
    let _ = (title, message);
}

#[cfg(windows)]
fn null_terminated_utf16(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn second_mutex_acquisition_reports_an_existing_instance() {
        let name = format!("Local\\EquestrianDreamLauncher-Test-{}", std::process::id());
        let first = acquire_named_instance(&name).unwrap();
        assert!(matches!(first, SingleInstanceState::Acquired(_)));

        let second = acquire_named_instance(&name).unwrap();
        assert!(matches!(second, SingleInstanceState::AlreadyRunning));

        drop(first);
        let third = acquire_named_instance(&name).unwrap();
        assert!(matches!(third, SingleInstanceState::Acquired(_)));
    }
}
