use std::{
    collections::HashSet,
    env,
    error::Error,
    fmt, io,
    path::{Path, PathBuf},
};

use tokio::process::Command;

use crate::process_command::configure_java_command;

#[cfg(windows)]
const JAVA_EXECUTABLE: &str = "java.exe";
#[cfg(not(windows))]
const JAVA_EXECUTABLE: &str = "java";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JavaRuntime {
    pub executable_path: PathBuf,
    pub major_version: u32,
}

#[derive(Debug)]
pub enum JavaError {
    JavaNotFound {
        required_major: u32,
    },
    VersionMismatch {
        required_major: u32,
        found_runtimes: Vec<JavaRuntime>,
    },
    FailedToExecute {
        executable_path: PathBuf,
        source: io::Error,
    },
    InvalidVersionOutput {
        executable_path: PathBuf,
        output: String,
    },
}

impl fmt::Display for JavaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::JavaNotFound { required_major } => {
                write!(formatter, "Java {required_major} was not found")
            }
            Self::VersionMismatch {
                required_major,
                found_runtimes,
            } => {
                write!(formatter, "Java {required_major} was not found; discovered")?;
                for (index, runtime) in found_runtimes.iter().enumerate() {
                    if index == 0 {
                        write!(formatter, " Java {}", runtime.major_version)?;
                    } else {
                        write!(formatter, ", Java {}", runtime.major_version)?;
                    }
                }
                Ok(())
            }
            Self::FailedToExecute {
                executable_path,
                source,
            } => write!(
                formatter,
                "failed to execute Java candidate {}: {source}",
                executable_path.display()
            ),
            Self::InvalidVersionOutput {
                executable_path, ..
            } => write!(
                formatter,
                "Java candidate {} returned unrecognized version output",
                executable_path.display()
            ),
        }
    }
}

impl Error for JavaError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::FailedToExecute { source, .. } => Some(source),
            Self::JavaNotFound { .. }
            | Self::VersionMismatch { .. }
            | Self::InvalidVersionOutput { .. } => None,
        }
    }
}

pub async fn find_java(required_major: u32) -> Result<JavaRuntime, JavaError> {
    let candidates = discover_candidates().await;
    if candidates.is_empty() {
        return Err(JavaError::JavaNotFound { required_major });
    }

    let mut found_runtimes = Vec::new();
    let mut first_invalid_output = None;
    let mut first_execution_error = None;

    for executable_path in candidates {
        match inspect_java(&executable_path).await {
            Ok(runtime) if runtime.major_version == required_major => return Ok(runtime),
            Ok(runtime) => found_runtimes.push(runtime),
            Err(error @ JavaError::InvalidVersionOutput { .. }) => {
                first_invalid_output.get_or_insert(error);
            }
            Err(error @ JavaError::FailedToExecute { .. }) => {
                first_execution_error.get_or_insert(error);
            }
            Err(JavaError::JavaNotFound { .. } | JavaError::VersionMismatch { .. }) => {
                unreachable!("a Java probe cannot perform discovery")
            }
        }
    }

    if !found_runtimes.is_empty() {
        return select_required_runtime(required_major, found_runtimes);
    }
    if let Some(error) = first_invalid_output {
        return Err(error);
    }
    if let Some(error) = first_execution_error {
        return Err(error);
    }

    Err(JavaError::JavaNotFound { required_major })
}

async fn inspect_java(executable_path: &Path) -> Result<JavaRuntime, JavaError> {
    let mut command = Command::new(executable_path);
    configure_java_command(&mut command);
    let output =
        command
            .arg("-version")
            .output()
            .await
            .map_err(|source| JavaError::FailedToExecute {
                executable_path: executable_path.to_owned(),
                source,
            })?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined_output = if stderr.is_empty() {
        stdout.into_owned()
    } else if stdout.is_empty() {
        stderr.into_owned()
    } else {
        format!("{stderr}\n{stdout}")
    };

    if !output.status.success() {
        return Err(JavaError::FailedToExecute {
            executable_path: executable_path.to_owned(),
            source: io::Error::other(format!("process exited with {}", output.status)),
        });
    }

    let major_version =
        parse_java_major(&combined_output).ok_or_else(|| JavaError::InvalidVersionOutput {
            executable_path: executable_path.to_owned(),
            output: combined_output,
        })?;

    Ok(JavaRuntime {
        executable_path: executable_path.to_owned(),
        major_version,
    })
}

fn parse_java_major(output: &str) -> Option<u32> {
    output.lines().find_map(|line| {
        let (_, version_suffix) = line.split_once("version")?;
        let quoted_version = version_suffix.split_once('"')?.1.split_once('"')?.0;
        let mut components = quoted_version.split(['.', '_', '-']);
        let first = components.next()?.parse::<u32>().ok()?;
        if first == 1 {
            components.next()?.parse().ok()
        } else {
            Some(first)
        }
    })
}

fn select_required_runtime(
    required_major: u32,
    found_runtimes: Vec<JavaRuntime>,
) -> Result<JavaRuntime, JavaError> {
    if let Some(runtime) = found_runtimes
        .iter()
        .find(|runtime| runtime.major_version == required_major)
    {
        return Ok(runtime.clone());
    }

    Err(JavaError::VersionMismatch {
        required_major,
        found_runtimes,
    })
}

async fn discover_candidates() -> Vec<PathBuf> {
    let mut raw_candidates = Vec::new();

    if let Some(java_home) = env::var_os("JAVA_HOME") {
        raw_candidates.push(PathBuf::from(java_home).join("bin").join(JAVA_EXECUTABLE));
    }

    if let Some(path) = env::var_os("PATH") {
        raw_candidates
            .extend(env::split_paths(&path).map(|directory| directory.join(JAVA_EXECUTABLE)));
    }

    #[cfg(windows)]
    for root in windows_java_roots() {
        raw_candidates.push(root.join("bin").join(JAVA_EXECUTABLE));
        if let Ok(mut entries) = tokio::fs::read_dir(&root).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                raw_candidates.push(entry.path().join("bin").join(JAVA_EXECUTABLE));
            }
        }
    }

    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    for candidate in raw_candidates {
        if seen.insert(candidate.clone())
            && tokio::fs::metadata(&candidate)
                .await
                .is_ok_and(|metadata| metadata.is_file())
        {
            candidates.push(candidate);
        }
    }
    candidates
}

#[cfg(windows)]
fn windows_java_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = env::var_os(variable).map(PathBuf::from) {
            for vendor in [
                "Java",
                "Eclipse Adoptium",
                "Microsoft",
                "Amazon Corretto",
                "Azul Systems",
                "BellSoft",
            ] {
                roots.push(base.join(vendor));
            }
        }
    }

    if let Some(local_app_data) = env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        for vendor in ["Eclipse Adoptium", "Microsoft", "Java"] {
            roots.push(local_app_data.join("Programs").join(vendor));
        }
    }
    if let Some(user_profile) = env::var_os("USERPROFILE").map(PathBuf::from) {
        roots.push(user_profile.join(".jdks"));
    }

    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_java_8_version() {
        assert_eq!(
            parse_java_major("java version \"1.8.0_431\"\nJava(TM) SE Runtime Environment"),
            Some(8)
        );
    }

    #[test]
    fn parses_java_17_version() {
        assert_eq!(
            parse_java_major("openjdk version \"17.0.13\" 2024-10-15"),
            Some(17)
        );
    }

    #[test]
    fn parses_java_21_version() {
        assert_eq!(
            parse_java_major("openjdk version \"21\" 2023-09-19 LTS"),
            Some(21)
        );
    }

    #[test]
    fn rejects_invalid_version_output() {
        assert_eq!(parse_java_major("not a Java version response"), None);
    }

    #[test]
    fn reports_version_mismatch() {
        let error = select_required_runtime(
            21,
            vec![JavaRuntime {
                executable_path: PathBuf::from("java17"),
                major_version: 17,
            }],
        )
        .unwrap_err();

        match error {
            JavaError::VersionMismatch {
                required_major,
                found_runtimes,
            } => {
                assert_eq!(required_major, 21);
                assert_eq!(found_runtimes[0].major_version, 17);
            }
            other => panic!("expected VersionMismatch, got {other:?}"),
        }
    }
}
