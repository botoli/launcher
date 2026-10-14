use std::path::{Path, PathBuf};

use rolling_file::{BasicRollingFileAppender, RollingConditionBasic};
use thiserror::Error;
use tracing_appender::non_blocking::WorkerGuard;
#[cfg(debug_assertions)]
use tracing_subscriber::fmt::writer::MakeWriterExt;

const MAX_LOG_FILE_SIZE: u64 = 5 * 1024 * 1024;
const LOG_ARCHIVE_COUNT: usize = 3;

pub struct LoggingGuard {
    _worker_guard: WorkerGuard,
}

pub fn initialize(logs_directory: &Path) -> Result<LoggingGuard, LoggingError> {
    let log_path = logs_directory.join("launcher.log");
    let appender = BasicRollingFileAppender::new(
        &log_path,
        RollingConditionBasic::new().max_size(MAX_LOG_FILE_SIZE),
        LOG_ARCHIVE_COUNT,
    )
    .map_err(|error| LoggingError::Appender {
        path: log_path,
        message: error.to_string(),
    })?;
    let (file_writer, worker_guard) = tracing_appender::non_blocking(appender);

    #[cfg(debug_assertions)]
    tracing_subscriber::fmt()
        .with_target(false)
        .with_ansi(false)
        .with_writer(file_writer.and(std::io::stderr))
        .try_init()
        .map_err(|error| LoggingError::Subscriber(error.to_string()))?;

    #[cfg(not(debug_assertions))]
    tracing_subscriber::fmt()
        .with_target(false)
        .with_ansi(false)
        .with_writer(file_writer)
        .try_init()
        .map_err(|error| LoggingError::Subscriber(error.to_string()))?;

    Ok(LoggingGuard {
        _worker_guard: worker_guard,
    })
}

#[derive(Debug, Error)]
pub enum LoggingError {
    #[error("failed to open rotating launcher log {path}: {message}")]
    Appender { path: PathBuf, message: String },
    #[error("failed to install launcher logging subscriber: {0}")]
    Subscriber(String),
}
