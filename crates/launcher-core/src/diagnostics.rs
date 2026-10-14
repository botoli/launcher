use std::error::Error;

const MAX_LOG_TAIL_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LauncherDiagnostic {
    pub message: String,
    pub log_tail: String,
}

impl LauncherDiagnostic {
    pub fn from_error(error: &(dyn Error + 'static)) -> Self {
        let mut message = error.to_string();
        let mut source = error.source();
        // Some providers repeat their cause in Display; avoid repeating it again.
        for _ in 0..8 {
            let Some(cause) = source else {
                break;
            };
            let detail = cause.to_string();
            if !message.contains(&detail) {
                message.push_str("\nПричина: ");
                message.push_str(&detail);
            }
            source = cause.source();
        }
        Self::message(message)
    }

    pub fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            log_tail: String::new(),
        }
    }
}

#[derive(Default)]
pub(crate) struct LogTail(String);

impl LogTail {
    pub(crate) fn push(&mut self, stream: &str, bytes: &[u8]) {
        self.0.push_str(&format!("[{stream}] "));
        self.0.push_str(&String::from_utf8_lossy(bytes));
        if !self.0.ends_with('\n') {
            self.0.push('\n');
        }
        if self.0.len() > MAX_LOG_TAIL_BYTES {
            let mut start = self.0.len() - MAX_LOG_TAIL_BYTES;
            while !self.0.is_char_boundary(start) {
                start += 1;
            }
            self.0.drain(..start);
        }
    }

    pub(crate) fn into_string(self) -> String {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_tail_is_bounded_and_preserves_last_utf8_message() {
        let mut tail = LogTail::default();
        tail.push("stdout", "старые данные".repeat(10000).as_bytes());
        tail.push("stderr", "Последняя причина сбоя".as_bytes());
        let result = tail.into_string();
        assert!(result.len() <= MAX_LOG_TAIL_BYTES);
        assert!(result.ends_with("[stderr] Последняя причина сбоя\n"));
    }
}
