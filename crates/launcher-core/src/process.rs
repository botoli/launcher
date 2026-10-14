use std::{
    env::JoinPathsError,
    error::Error,
    ffi::OsString,
    fmt, io,
    path::PathBuf,
    process::{ExitStatus, Stdio},
};

use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::{mpsc, oneshot, watch},
    task::{JoinError, JoinHandle},
};

use crate::process_command::configure_java_command;

const OUTPUT_CHUNK_SIZE: usize = 8 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchSpec {
    pub java_executable: PathBuf,
    pub jvm_args: Vec<OsString>,
    pub classpath: Vec<PathBuf>,
    pub main_class: String,
    pub game_args: Vec<OsString>,
    pub working_directory: PathBuf,
}

#[derive(Debug)]
pub enum LaunchSpecError {
    InvalidClasspath { source: JoinPathsError },
}

impl fmt::Display for LaunchSpecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidClasspath { .. } => {
                formatter.write_str("classpath contains a path-list separator")
            }
        }
    }
}

impl Error for LaunchSpecError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidClasspath { source } => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum ProcessError {
    InvalidSpec(LaunchSpecError),
    Spawn {
        executable_path: PathBuf,
        source: io::Error,
    },
    MissingOutputPipe {
        stream: &'static str,
    },
    Wait {
        source: io::Error,
    },
    Kill {
        source: io::Error,
    },
    ReadOutput {
        stream: &'static str,
        source: io::Error,
    },
    OutputTaskFailed {
        stream: &'static str,
        source: JoinError,
    },
    SupervisorStopped,
}

impl fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSpec(source) => {
                write!(formatter, "invalid launch specification: {source}")
            }
            Self::Spawn {
                executable_path,
                source,
            } => write!(
                formatter,
                "failed to launch {}: {source}",
                executable_path.display()
            ),
            Self::MissingOutputPipe { stream } => {
                write!(formatter, "launched process has no piped {stream}")
            }
            Self::Wait { source } => write!(formatter, "failed to wait for process: {source}"),
            Self::Kill { source } => write!(formatter, "failed to kill process: {source}"),
            Self::ReadOutput { stream, source } => {
                write!(formatter, "failed to read process {stream}: {source}")
            }
            Self::OutputTaskFailed { stream, source } => {
                write!(formatter, "process {stream} reader task failed: {source}")
            }
            Self::SupervisorStopped => {
                formatter.write_str("process supervisor stopped without a result")
            }
        }
    }
}

impl Error for ProcessError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidSpec(source) => Some(source),
            Self::Spawn { source, .. }
            | Self::Wait { source }
            | Self::Kill { source }
            | Self::ReadOutput { source, .. } => Some(source),
            Self::OutputTaskFailed { source, .. } => Some(source),
            Self::MissingOutputPipe { .. } | Self::SupervisorStopped => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessExit {
    pub status: ExitStatus,
    pub cancelled: bool,
}

#[derive(Clone, Debug)]
pub struct ProcessCancellation {
    sender: watch::Sender<bool>,
}

impl ProcessCancellation {
    pub fn cancel(&self) {
        self.sender.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.sender.borrow()
    }
}

#[derive(Debug)]
pub struct ProcessHandle {
    events: mpsc::UnboundedReceiver<ProcessEvent>,
    completion: Option<oneshot::Receiver<Result<ProcessExit, ProcessError>>>,
    cancellation: ProcessCancellation,
    cancel_on_drop: bool,
}

impl ProcessHandle {
    pub fn cancellation(&self) -> ProcessCancellation {
        self.cancellation.clone()
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub async fn next_event(&mut self) -> Option<ProcessEvent> {
        self.events.recv().await
    }

    pub async fn wait(mut self) -> Result<ProcessExit, ProcessError> {
        self.cancel_on_drop = false;
        let completion = self
            .completion
            .take()
            .expect("process completion receiver is present until wait");
        completion
            .await
            .unwrap_or(Err(ProcessError::SupervisorStopped))
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        if self.cancel_on_drop {
            self.cancellation.cancel();
        }
    }
}

pub fn build_command(spec: &LaunchSpec) -> Result<Command, LaunchSpecError> {
    let classpath = std::env::join_paths(&spec.classpath)
        .map_err(|source| LaunchSpecError::InvalidClasspath { source })?;

    let mut command = Command::new(&spec.java_executable);
    configure_java_command(&mut command);
    command
        .args(&spec.jvm_args)
        .arg("-cp")
        .arg(classpath)
        .arg(&spec.main_class)
        .args(&spec.game_args)
        .current_dir(&spec.working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(command)
}

pub fn launch_process(spec: LaunchSpec) -> Result<ProcessHandle, ProcessError> {
    let executable_path = spec.java_executable.clone();
    let command = build_command(&spec).map_err(ProcessError::InvalidSpec)?;
    spawn_command(command, executable_path)
}

fn spawn_command(
    mut command: Command,
    executable_path: PathBuf,
) -> Result<ProcessHandle, ProcessError> {
    let mut child = command.spawn().map_err(|source| ProcessError::Spawn {
        executable_path,
        source,
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or(ProcessError::MissingOutputPipe { stream: "stdout" })?;
    let stderr = child
        .stderr
        .take()
        .ok_or(ProcessError::MissingOutputPipe { stream: "stderr" })?;

    let (event_sender, events) = mpsc::unbounded_channel();
    let stdout_task = tokio::spawn(forward_output(
        stdout,
        event_sender.clone(),
        ProcessEvent::Stdout,
    ));
    let stderr_task = tokio::spawn(forward_output(stderr, event_sender, ProcessEvent::Stderr));
    let (cancellation_sender, cancellation_receiver) = watch::channel(false);
    let cancellation = ProcessCancellation {
        sender: cancellation_sender,
    };
    let (completion_sender, completion) = oneshot::channel();

    let supervisor = tokio::spawn(supervise_process(
        child,
        cancellation_receiver,
        stdout_task,
        stderr_task,
        completion_sender,
    ));
    drop(supervisor);

    Ok(ProcessHandle {
        events,
        completion: Some(completion),
        cancellation,
        cancel_on_drop: true,
    })
}

async fn forward_output<R>(
    mut reader: R,
    events: mpsc::UnboundedSender<ProcessEvent>,
    make_event: fn(Vec<u8>) -> ProcessEvent,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; OUTPUT_CHUNK_SIZE];
    loop {
        let bytes_read = reader.read(&mut buffer).await?;
        if bytes_read == 0 {
            return Ok(());
        }

        // Keep draining the pipe even if the event consumer has gone away. This
        // prevents a verbose child process from blocking on a full OS pipe.
        let _event_was_delivered = events.send(make_event(buffer[..bytes_read].to_vec()));
    }
}

async fn supervise_process(
    mut child: Child,
    mut cancellation: watch::Receiver<bool>,
    stdout_task: JoinHandle<io::Result<()>>,
    stderr_task: JoinHandle<io::Result<()>>,
    completion: oneshot::Sender<Result<ProcessExit, ProcessError>>,
) {
    let exit = match wait_for_exit_or_cancellation(&mut child, &mut cancellation).await {
        Ok(exit) => exit,
        Err(error) => {
            stdout_task.abort();
            stderr_task.abort();
            let _stdout_result = stdout_task.await;
            let _stderr_result = stderr_task.await;
            let _result_was_delivered = completion.send(Err(error));
            return;
        }
    };
    let stdout_result = output_task_result("stdout", stdout_task).await;
    let stderr_result = output_task_result("stderr", stderr_task).await;

    let result = stdout_result
        .map(|()| exit)
        .and_then(|exit| stderr_result.map(|()| exit));
    let _result_was_delivered = completion.send(result);
}

async fn wait_for_exit_or_cancellation(
    child: &mut Child,
    cancellation: &mut watch::Receiver<bool>,
) -> Result<ProcessExit, ProcessError> {
    loop {
        if *cancellation.borrow() {
            child
                .kill()
                .await
                .map_err(|source| ProcessError::Kill { source })?;
            let status = child
                .wait()
                .await
                .map_err(|source| ProcessError::Wait { source })?;
            return Ok(ProcessExit {
                status,
                cancelled: true,
            });
        }

        tokio::select! {
            biased;
            changed = cancellation.changed() => {
                if changed.is_err() || *cancellation.borrow() {
                    child
                        .kill()
                        .await
                        .map_err(|source| ProcessError::Kill { source })?;
                    let status = child
                        .wait()
                        .await
                        .map_err(|source| ProcessError::Wait { source })?;
                    return Ok(ProcessExit {
                        status,
                        cancelled: true,
                    });
                }
            }
            result = child.wait() => {
                return result
                    .map(|status| ProcessExit {
                        status,
                        cancelled: false,
                    })
                    .map_err(|source| ProcessError::Wait { source });
            }
        }
    }
}

async fn output_task_result(
    stream: &'static str,
    task: JoinHandle<io::Result<()>>,
) -> Result<(), ProcessError> {
    match task.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(source)) => Err(ProcessError::ReadOutput { stream, source }),
        Err(source) => Err(ProcessError::OutputTaskFailed { stream, source }),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::{env, path::Path, process::Stdio};

    use tokio::process::Command;

    use super::{ProcessError, ProcessEvent, ProcessExit, ProcessHandle, spawn_command};

    pub(crate) const HELPER_SCENARIO: &str = "LAUNCHER_CORE_PROCESS_TEST_SCENARIO";

    pub(crate) fn spawn_test_helper(
        scenario: &str,
        working_directory: &Path,
    ) -> Result<ProcessHandle, ProcessError> {
        let executable_path = env::current_exe().map_err(|source| ProcessError::Spawn {
            executable_path: "current-test-executable".into(),
            source,
        })?;
        let mut command = Command::new(&executable_path);
        command
            .args([
                "--exact",
                "process::tests::process_test_helper",
                "--nocapture",
            ])
            .env(HELPER_SCENARIO, scenario)
            .current_dir(working_directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        spawn_command(command, executable_path)
    }

    pub(crate) async fn collect_output(
        mut process: ProcessHandle,
    ) -> (Vec<u8>, Vec<u8>, ProcessExit) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        while let Some(event) = process.next_event().await {
            match event {
                ProcessEvent::Stdout(chunk) => stdout.extend(chunk),
                ProcessEvent::Stderr(chunk) => stderr.extend(chunk),
            }
        }
        let exit = process.wait().await.unwrap();
        (stdout, stderr, exit)
    }
}

#[cfg(test)]
mod tests {
    use std::{env, ffi::OsStr, path::Path, process, thread, time::Duration};

    use super::*;
    use crate::process::test_support::{HELPER_SCENARIO, collect_output, spawn_test_helper};

    #[test]
    fn launch_spec_preserves_spaces_and_argument_boundaries_without_shell() {
        let spec = LaunchSpec {
            java_executable: PathBuf::from(r"C:\Program Files\Java\bin\java.exe"),
            jvm_args: vec![OsString::from("-Dprofile=Mod Pack")],
            classpath: vec![
                PathBuf::from(r"C:\Minecraft Folder\client.jar"),
                PathBuf::from(r"C:\Minecraft Folder\libraries\library.jar"),
            ],
            main_class: "net.minecraft.client.main.Main".to_owned(),
            game_args: vec![
                OsString::from("--username"),
                OsString::from("Player Name; echo not-a-shell"),
            ],
            working_directory: PathBuf::from(r"C:\Minecraft Folder"),
        };
        let command = build_command(&spec).unwrap();
        let standard_command = command.as_std();
        let expected_classpath = std::env::join_paths(&spec.classpath).unwrap();
        let arguments: Vec<&OsStr> = standard_command.get_args().collect();

        assert_eq!(
            standard_command.get_program(),
            spec.java_executable.as_os_str()
        );
        assert_eq!(
            standard_command.get_current_dir(),
            Some(Path::new(r"C:\Minecraft Folder"))
        );
        assert_eq!(arguments[0], OsStr::new("-Dprofile=Mod Pack"));
        assert_eq!(arguments[1], OsStr::new("-cp"));
        assert_eq!(arguments[2], expected_classpath.as_os_str());
        assert_eq!(arguments[3], OsStr::new("net.minecraft.client.main.Main"));
        assert_eq!(arguments[4], OsStr::new("--username"));
        assert_eq!(arguments[5], OsStr::new("Player Name; echo not-a-shell"));
        assert_eq!(arguments.len(), 6);
    }

    #[test]
    fn process_test_helper() {
        let Ok(scenario) = env::var(HELPER_SCENARIO) else {
            return;
        };

        match scenario.as_str() {
            "capture" => {
                println!("portable stdout marker");
                eprintln!("portable stderr marker");
            }
            "delayed-capture" => {
                thread::sleep(Duration::from_millis(200));
                println!("portable stdout marker");
                eprintln!("portable stderr marker");
            }
            "exit" => process::exit(23),
            "sleep" => loop {
                thread::sleep(Duration::from_secs(1));
            },
            "working-directory" => println!("helper-cwd={}", env::current_dir().unwrap().display()),
            other => panic!("unknown process helper scenario: {other}"),
        }
    }

    #[tokio::test]
    async fn captures_stdout_stderr_and_successful_exit() {
        let working_directory = tempfile::tempdir().unwrap();
        let process = spawn_test_helper("capture", working_directory.path()).unwrap();

        let (stdout, stderr, exit) = collect_output(process).await;

        assert!(
            String::from_utf8(stdout)
                .unwrap()
                .contains("portable stdout marker")
        );
        assert!(
            String::from_utf8(stderr)
                .unwrap()
                .contains("portable stderr marker")
        );
        assert!(exit.status.success());
        assert!(!exit.cancelled);
    }

    #[tokio::test]
    async fn returns_child_exit_code() {
        let working_directory = tempfile::tempdir().unwrap();
        let process = spawn_test_helper("exit", working_directory.path()).unwrap();

        let (_, _, exit) = collect_output(process).await;

        assert_eq!(exit.status.code(), Some(23));
        assert!(!exit.cancelled);
    }

    #[tokio::test]
    async fn cancellation_kills_and_reaps_child_process() {
        let working_directory = tempfile::tempdir().unwrap();
        let mut process = spawn_test_helper("sleep", working_directory.path()).unwrap();
        let cancellation = process.cancellation();

        cancellation.cancel();
        while process.next_event().await.is_some() {}
        let exit = tokio::time::timeout(Duration::from_secs(5), process.wait())
            .await
            .expect("cancelled helper did not exit in time")
            .unwrap();

        assert!(exit.cancelled);
        assert!(!exit.status.success());
    }

    #[tokio::test]
    async fn applies_explicit_working_directory_with_spaces() {
        let working_directory = tempfile::Builder::new()
            .prefix("launcher core working directory ")
            .tempdir()
            .unwrap();
        let process = spawn_test_helper("working-directory", working_directory.path()).unwrap();

        let (stdout, _, exit) = collect_output(process).await;
        let stdout = String::from_utf8(stdout).unwrap();
        let reported_directory = stdout
            .lines()
            .find_map(|line| line.strip_prefix("helper-cwd="))
            .map(PathBuf::from)
            .expect("helper did not report its working directory");

        assert!(exit.status.success());
        assert_eq!(
            std::fs::canonicalize(reported_directory).unwrap(),
            std::fs::canonicalize(working_directory.path()).unwrap()
        );
    }
}
