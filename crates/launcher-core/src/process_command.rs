use tokio::process::Command;

pub(crate) fn configure_java_command(command: &mut Command) {
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);

    #[cfg(not(windows))]
    let _ = command;
}
