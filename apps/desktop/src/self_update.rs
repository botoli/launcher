//! Windows replacement helper. The running launcher is copied into staging and
//! re-entered in helper mode before config, logging, or the single-instance guard.
use launcher_core::launcher_update::{EXECUTABLE, README, StagedUpdate};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Child, Command, ExitCode},
    time::{Duration, Instant},
};

const APPLY: &str = "--launcher-apply-update";
const HEALTH: &str = "--launcher-update-health";
const ROLLBACK: &str = "--launcher-update-rollback";
const PLAN: &str = "plan.json";
const HELPER: &str = "updater-helper.exe";
const PREFIX: &str = ".launcher-update-";

#[derive(Debug, Serialize, Deserialize)]
struct Plan {
    target: PathBuf,
    working_directory: PathBuf,
    parent_pid: u32,
    original_sha256: String,
    original_readme_sha256: Option<String>,
    executable_sha256: String,
    readme_sha256: String,
}
fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}

pub fn dispatch_helper() -> io::Result<Option<ExitCode>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_none_or(|arg| arg != APPLY) {
        return Ok(None);
    }
    if args.len() != 2 {
        return Err(invalid("Invalid update helper arguments"));
    }
    let plan_path = PathBuf::from(&args[1]);
    match run_helper(&plan_path) {
        Ok(()) => Ok(Some(ExitCode::SUCCESS)),
        Err(error) => {
            crate::platform::show_error_message(
                "Обновление лаунчера",
                &format!("Не удалось обновить лаунчер: {error}"),
            );
            Ok(Some(ExitCode::FAILURE))
        }
    }
}
pub fn is_rollback_launch() -> bool {
    std::env::args_os().any(|arg| arg == ROLLBACK)
}

pub fn schedule_health_signal() -> io::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_none_or(|arg| arg != HEALTH) {
        return Ok(());
    }
    if args.len() != 2 {
        return Err(invalid("Invalid update health arguments"));
    }
    let stage = PathBuf::from(&args[1]).canonicalize()?;
    let target = std::env::current_exe()?.canonicalize()?;
    validate_stage(
        &stage,
        target
            .parent()
            .ok_or_else(|| invalid("Missing EXE parent"))?,
    )?;
    let plan = read_plan(&stage.join(PLAN))?;
    if plan.target != target {
        return Err(invalid("Health signal belongs to another launcher"));
    }
    slint::Timer::single_shot(Duration::ZERO, move || {
        // Emitted only once the new executable has initialized and entered the UI loop.
        if let Err(error) = fs::write(stage.join("healthy"), b"ok") {
            tracing::error!(%error, "failed to acknowledge launcher update startup");
        }
    });
    Ok(())
}
fn validate_stage(stage: &Path, root: &Path) -> io::Result<()> {
    if stage.parent() != Some(root)
        || !stage
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with(PREFIX))
    {
        return Err(invalid(
            "Update staging is outside the installation directory",
        ));
    }
    Ok(())
}
fn read_plan(path: &Path) -> io::Result<Plan> {
    if path.file_name().is_none_or(|n| n != PLAN) || fs::metadata(path)?.len() > 16 * 1024 {
        return Err(invalid("Invalid update plan"));
    }
    serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)
}
fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn regular_file(path: &Path) -> io::Result<()> {
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(invalid("Update target must be a regular file"));
    }
    Ok(())
}
fn validate_plan(stage: &Path, plan: &Plan) -> io::Result<()> {
    let root = stage
        .parent()
        .ok_or_else(|| invalid("Missing installation directory"))?;
    validate_stage(stage, root)?;
    if !plan.target.is_absolute()
        || plan.target.parent() != Some(root)
        || plan
            .target
            .extension()
            .is_none_or(|e| !e.eq_ignore_ascii_case("exe"))
        || plan.parent_pid == 0
        || plan.parent_pid == std::process::id()
    {
        return Err(invalid("Invalid update target or parent process"));
    }
    regular_file(&plan.target)?;
    let readme = root.join(README);
    if readme.exists() {
        regular_file(&readme)?;
    }
    for (name, expected) in [
        (EXECUTABLE, &plan.executable_sha256),
        (README, &plan.readme_sha256),
    ] {
        let path = stage.join("new").join(name);
        regular_file(&path)?;
        if path.canonicalize()?.parent() != Some(stage.join("new").as_path())
            || hash_file(&path)? != *expected
        {
            return Err(invalid("Staged update has changed since verification"));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn verify_release_version(path: &Path, expected: &str) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW,
    };
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let size = unsafe { GetFileVersionInfoSizeW(path.as_ptr(), std::ptr::null_mut()) };
    if size == 0 || size > 1024 * 1024 {
        return Err(invalid("EXE has no valid Windows version information"));
    }
    let mut data = vec![0u64; (size as usize).div_ceil(8)];
    if unsafe { GetFileVersionInfoW(path.as_ptr(), 0, size, data.as_mut_ptr().cast()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut info = std::ptr::null_mut();
    let mut length = 0;
    let query = [92u16, 0];
    if unsafe { VerQueryValueW(data.as_ptr().cast(), query.as_ptr(), &mut info, &mut length) } == 0
        || info.is_null()
        || length < std::mem::size_of::<VS_FIXEDFILEINFO>() as u32
    {
        return Err(invalid("EXE version information is incomplete"));
    }
    let info = unsafe { std::ptr::read_unaligned(info.cast::<VS_FIXEDFILEINFO>()) };
    let actual = format!(
        "{}.{}.{}",
        info.dwProductVersionMS >> 16,
        info.dwProductVersionMS & 0xffff,
        info.dwProductVersionLS >> 16
    );
    if info.dwSignature != 0xfeef04bd || info.dwProductVersionLS & 0xffff != 0 || actual != expected
    {
        return Err(invalid(&format!(
            "Версия EXE ({actual}) не совпадает с опубликованной ({expected}). Загрузите правильную сборку"
        )));
    }
    Ok(())
}
#[cfg(not(windows))]
fn verify_release_version(_: &Path, _: &str) -> io::Result<()> {
    Err(invalid("Self-update requires Windows"))
}

pub fn launch_helper(update: StagedUpdate, target: PathBuf) -> io::Result<Child> {
    let target = target.canonicalize()?;
    let stage = update.directory.path().canonicalize()?;
    validate_stage(
        &stage,
        target
            .parent()
            .ok_or_else(|| invalid("Missing installation directory"))?,
    )?;
    let plan = Plan {
        target: target.clone(),
        working_directory: std::env::current_dir()?,
        parent_pid: std::process::id(),
        original_sha256: hash_file(&target)?,
        original_readme_sha256: match fs::symlink_metadata(target.parent().unwrap().join(README)) {
            Ok(_) => Some(hash_file(&target.parent().unwrap().join(README))?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        },
        executable_sha256: update.executable_sha256,
        readme_sha256: update.readme_sha256,
    };
    verify_release_version(&stage.join("new").join(EXECUTABLE), &update.version)?;
    let plan_path = stage.join(PLAN);
    fs::write(&plan_path, serde_json::to_vec(&plan)?)?;
    fs::copy(&target, stage.join(HELPER))?;
    let mut command = Command::new(stage.join(HELPER));
    command
        .arg(APPLY)
        .arg(&plan_path)
        .current_dir(&plan.working_directory);
    hide(&mut command);
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !stage.join("helper-ready").is_file() {
        if child.try_wait()?.is_some() {
            return Err(invalid("Update helper exited before it was ready"));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(invalid("Update helper did not start in time"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // The helper now owns staging; keep it across the parent process shutdown.
    let _ = update.directory.keep();
    Ok(child)
}
fn hide(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    #[cfg(not(windows))]
    let _ = command;
}

#[cfg(windows)]
fn wait_parent(stage: &Path, pid: u32) -> io::Result<()> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };
    // Open while the parent is alive, retaining the exact process rather than
    // polling a PID which Windows could recycle after it exits.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let ready = fs::write(stage.join("helper-ready"), b"ok");
    let result = if ready.is_ok() {
        unsafe { WaitForSingleObject(handle, 60_000) }
    } else {
        u32::MAX
    };
    unsafe {
        CloseHandle(handle);
    }
    ready?;
    if result != WAIT_OBJECT_0 {
        return Err(invalid(
            "Launcher did not exit; installed files were left unchanged",
        ));
    }
    Ok(())
}
#[cfg(not(windows))]
fn wait_parent(_: &Path, _: u32) -> io::Result<()> {
    Err(invalid("Self-update requires Windows"))
}

fn launch_and_check(plan: &Plan, stage: &Path) -> io::Result<()> {
    let mut command = Command::new(&plan.target);
    command
        .arg(HEALTH)
        .arg(stage)
        .current_dir(&plan.working_directory);
    hide(&mut command);
    let mut child = command.spawn()?;
    let result = await_health(&mut child, stage, Duration::from_secs(75));
    if result.is_err() {
        // Release its image before restoring the old executable.
        let _ = child.kill();
        child.wait()?;
    }
    result
}
fn await_health(child: &mut Child, stage: &Path, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return Err(invalid("New launcher exited during startup"));
        }
        if stage.join("healthy").is_file() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(invalid("New launcher did not confirm startup"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn activate(stage: &Path, plan: &Plan, start: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    let root = plan
        .target
        .parent()
        .ok_or_else(|| invalid("Missing installation directory"))?;
    let readme = root.join(README);
    let backup = stage.join("previous");
    fs::create_dir(&backup)?;
    fs::copy(&plan.target, backup.join(EXECUTABLE))?;
    let had_readme = readme.exists();
    if had_readme {
        fs::copy(&readme, backup.join(README))?;
    }
    let mut changed_readme = false;
    let mut changed_exe = false;
    let result = (|| {
        fs::rename(stage.join("new").join(README), &readme)?;
        changed_readme = true;
        fs::rename(stage.join("new").join(EXECUTABLE), &plan.target)?;
        changed_exe = true;
        start()
    })();
    if let Err(error) = result {
        let restore = (|| -> io::Result<()> {
            if changed_exe {
                fs::copy(backup.join(EXECUTABLE), stage.join("restore.exe"))?;
                fs::rename(stage.join("restore.exe"), &plan.target)?;
            }
            if changed_readme {
                if had_readme {
                    fs::copy(backup.join(README), stage.join("restore.txt"))?;
                    fs::rename(stage.join("restore.txt"), &readme)?;
                } else {
                    fs::remove_file(&readme)?;
                }
            }
            Ok(())
        })();
        return match restore {
            Ok(()) => Err(error),
            Err(rollback) => Err(invalid(&format!(
                "{error}; восстановление: {rollback}; резервная копия: {}",
                backup.display()
            ))),
        };
    }
    Ok(())
}
fn originals_restored(plan: &Plan) -> bool {
    if !hash_file(&plan.target).is_ok_and(|hash| hash == plan.original_sha256) {
        return false;
    }
    let Some(root) = plan.target.parent() else {
        return false;
    };
    let readme = root.join(README);
    match &plan.original_readme_sha256 {
        Some(expected) => hash_file(&readme).is_ok_and(|hash| hash == *expected),
        None => {
            matches!(fs::symlink_metadata(readme), Err(error) if error.kind() == io::ErrorKind::NotFound)
        }
    }
}

fn run_helper(plan_path: &Path) -> io::Result<()> {
    let helper = std::env::current_exe()?.canonicalize()?;
    let stage = helper
        .parent()
        .ok_or_else(|| invalid("Missing helper directory"))?;
    if helper.file_name().is_none_or(|n| n != HELPER)
        || plan_path.canonicalize()? != stage.join(PLAN)
    {
        return Err(invalid(
            "Update helper must run from its own staging directory",
        ));
    }
    let plan = read_plan(plan_path)?;
    validate_plan(stage, &plan)?;
    wait_parent(stage, plan.parent_pid)?;
    let result = (|| {
        validate_plan(stage, &plan)?;
        if !originals_restored(&plan) {
            return Err(invalid("Installed launcher changed during update"));
        }
        activate(stage, &plan, || launch_and_check(&plan, stage))
    })();
    if let Err(error) = result {
        // A successful rollback leaves byte-for-byte copies of both old files.
        // Do not claim restoration or restart if restoration itself failed.
        let restored = originals_restored(&plan);
        if restored {
            let mut command = Command::new(&plan.target);
            command.arg(ROLLBACK).current_dir(&plan.working_directory);
            hide(&mut command);
            command.spawn()?;
            crate::platform::show_error_message(
                "Обновление лаунчера",
                &format!("Обновление не установлено. Запущена прежняя версия.\n{error}"),
            );
            fs::write(stage.join("complete"), b"rolled-back")?;
            return Ok(());
        }
        return Err(error);
    }
    fs::write(stage.join("complete"), b"installed")?;
    Ok(())
}

pub fn cleanup_completed() {
    let Ok(exe) = std::env::current_exe().and_then(|p| p.canonicalize()) else {
        return;
    };
    let Some(root) = exe.parent() else {
        return;
    };
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_dir())
            || validate_stage(&path, root).is_err()
            || !read_plan(&path.join(PLAN)).is_ok_and(|plan| plan.target.parent() == Some(root))
            || !path.join("complete").is_file()
            || path.canonicalize().ok().as_ref() != Some(&path)
        {
            continue;
        }
        // A running Windows helper keeps its image locked. If it is still alive,
        // leave the whole directory for the next ordinary launcher startup.
        if fs::remove_file(path.join(HELPER)).is_ok() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(with_readme: bool) -> (tempfile::TempDir, PathBuf, Plan) {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let stage = root_path.join(".launcher-update-test");
        fs::create_dir_all(stage.join("new")).unwrap();
        let target = root_path.join(EXECUTABLE);
        fs::write(&target, b"old exe").unwrap();
        if with_readme {
            fs::write(root_path.join(README), b"old readme").unwrap();
        }
        fs::write(stage.join("new").join(EXECUTABLE), b"new exe").unwrap();
        fs::write(stage.join("new").join(README), b"new readme").unwrap();
        let plan = Plan {
            target,
            working_directory: root_path,
            parent_pid: std::process::id().saturating_add(100000),
            original_sha256: format!("{:x}", Sha256::digest(b"old exe")),
            original_readme_sha256: with_readme
                .then(|| format!("{:x}", Sha256::digest(b"old readme"))),
            executable_sha256: format!("{:x}", Sha256::digest(b"new exe")),
            readme_sha256: format!("{:x}", Sha256::digest(b"new readme")),
        };
        (root, stage, plan)
    }
    #[test]
    fn replaces_both_files_only_after_full_staging_and_retains_backup() {
        let (_root, stage, plan) = fixture(true);
        validate_plan(&stage, &plan).unwrap();
        activate(&stage, &plan, || {
            assert_eq!(fs::read(&plan.target).unwrap(), b"new exe");
            assert_eq!(
                fs::read(plan.target.parent().unwrap().join(README)).unwrap(),
                b"new readme"
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(
            fs::read(stage.join("previous").join(EXECUTABLE)).unwrap(),
            b"old exe"
        );
        assert_eq!(
            fs::read(stage.join("previous").join(README)).unwrap(),
            b"old readme"
        );
    }
    #[test]
    fn failed_start_restores_exact_originals_including_missing_readme() {
        for with_readme in [true, false] {
            let (_root, stage, plan) = fixture(with_readme);
            assert!(activate(&stage, &plan, || Err(invalid("simulated startup failure"))).is_err());
            assert_eq!(fs::read(&plan.target).unwrap(), b"old exe");
            let readme = plan.target.parent().unwrap().join(README);
            if with_readme {
                assert_eq!(fs::read(readme).unwrap(), b"old readme");
            } else {
                assert!(!readme.exists());
            }
        }
    }
    #[test]
    fn partial_activation_failure_restores_readme_and_preserves_exe() {
        let (_root, stage, plan) = fixture(true);
        fs::remove_file(stage.join("new").join(EXECUTABLE)).unwrap();
        assert!(
            activate(&stage, &plan, || panic!(
                "must not launch incomplete update"
            ))
            .is_err()
        );
        assert_eq!(fs::read(&plan.target).unwrap(), b"old exe");
        assert_eq!(
            fs::read(plan.target.parent().unwrap().join(README)).unwrap(),
            b"old readme"
        );
    }
    #[test]
    fn rejects_tampered_staging_and_targets_outside_installation() {
        let (_root, stage, mut plan) = fixture(true);
        fs::write(stage.join("new").join(EXECUTABLE), b"tampered").unwrap();
        assert!(validate_plan(&stage, &plan).is_err());
        fs::write(stage.join("new").join(EXECUTABLE), b"new exe").unwrap();
        plan.target = stage.join(EXECUTABLE);
        assert!(validate_plan(&stage, &plan).is_err());
        assert!(validate_stage(&stage, &stage).is_err());
    }
    #[cfg(windows)]
    #[test]
    fn locked_installed_exe_is_preserved_and_readme_is_rolled_back() {
        use std::os::windows::fs::OpenOptionsExt;
        let (_root, stage, plan) = fixture(true);
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&plan.target)
            .unwrap();
        assert!(
            activate(&stage, &plan, || panic!(
                "locked executable cannot launch update"
            ))
            .is_err()
        );
        assert_eq!(fs::read(&plan.target).unwrap(), b"old exe");
        assert_eq!(
            fs::read(plan.target.parent().unwrap().join(README)).unwrap(),
            b"old readme"
        );
        drop(locked);
    }
    #[cfg(windows)]
    #[test]
    fn embedded_version_matches_release_and_rejects_wrong_published_version() {
        let exe = std::env::current_exe().unwrap();
        verify_release_version(&exe, env!("CARGO_PKG_VERSION")).unwrap();
        assert!(verify_release_version(&exe, "999.0.0").is_err());
    }
    #[test]
    fn updater_child_process() {
        if std::env::var_os("LAUNCHER_UPDATER_TEST_CHILD").is_some() {
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    fn child() -> Child {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "self_update::tests::updater_child_process"])
            .env("LAUNCHER_UPDATER_TEST_CHILD", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        hide(&mut command);
        command.spawn().unwrap()
    }
    #[cfg(windows)]
    #[test]
    fn waits_for_exact_parent_process_and_signals_readiness() {
        let stage = tempfile::tempdir().unwrap();
        let mut process = child();
        wait_parent(stage.path(), process.id()).unwrap();
        assert!(stage.path().join("helper-ready").is_file());
        assert!(process.wait().unwrap().success());
    }
    #[test]
    fn health_check_accepts_ready_child_and_detects_early_exit() {
        let stage = tempfile::tempdir().unwrap();
        let mut process = child();
        assert!(await_health(&mut process, stage.path(), Duration::from_secs(3)).is_err());
        process.wait().unwrap();
        fs::write(stage.path().join("healthy"), b"ok").unwrap();
        let mut process = child();
        await_health(&mut process, stage.path(), Duration::from_secs(3)).unwrap();
        let _ = process.kill();
        process.wait().unwrap();
    }
    #[test]
    fn rollback_is_complete_only_when_both_original_files_are_restored() {
        let (_root, _stage, plan) = fixture(true);
        assert!(originals_restored(&plan));
        fs::write(plan.target.parent().unwrap().join(README), b"not restored").unwrap();
        assert!(!originals_restored(&plan));
        let (_root, _stage, plan) = fixture(false);
        assert!(originals_restored(&plan));
        fs::write(
            plan.target.parent().unwrap().join(README),
            b"unexpected new readme",
        )
        .unwrap();
        assert!(!originals_restored(&plan));
    }
}
