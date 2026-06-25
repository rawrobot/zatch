//! Best-effort application log for ztch's own status and error messages.
//!
//! Session output is logged separately as `<session>.log`. This module records
//! the tool's own messages in the standard cache directory so failures and
//! status updates can be inspected after the terminal output is gone.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::util::get_session_dir;

pub const APP_LOG_FILE_NAME: &str = ".ztch-app.log";
const APP_LOG_MAX_SIZE: usize = 256 * 1024;

/// Application log severity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

impl LogLevel {
    fn as_str(self) -> &'static str {
        match self {
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }
}

/// Cached application-log context.
#[derive(Clone, Debug)]
pub struct AppLog {
    progname: String,
    path: PathBuf,
}

impl AppLog {
    /// Create an application-log context for `progname`.
    pub fn new(progname: &str) -> Self {
        AppLog {
            progname: progname.to_string(),
            path: get_session_dir(progname).join(APP_LOG_FILE_NAME),
        }
    }

    /// Program name used in user-facing messages.
    pub fn progname(&self) -> &str {
        &self.progname
    }

    /// Path used for the application log.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Record one application log line from an already-formatted message.
    pub fn record(&self, level: LogLevel, message: &str) {
        let line = format!(
            "{} {} {}\n",
            unix_timestamp(),
            level.as_str(),
            escape_line(message)
        );

        use std::os::unix::fs::OpenOptionsExt;
        let mut file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)
        {
            Ok(file) => file,
            Err(_) => return,
        };

        let _ = file.write_all(line.as_bytes());
        drop(file);
        rotate_path(&self.path, APP_LOG_MAX_SIZE);
    }
}

fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn escape_line(message: &str) -> String {
    message.replace('\r', "\\r").replace('\n', "\\n")
}

fn rotate_path(path: &std::path::Path, max_size: usize) {
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(file) => file,
        Err(_) => return,
    };
    rotate_file(&mut file, max_size);
}

fn rotate_file(file: &mut std::fs::File, max_size: usize) {
    let size = file.seek(SeekFrom::End(0)).unwrap_or(0);
    if size <= max_size as u64 {
        return;
    }

    if max_size == 0 {
        let _ = file.set_len(0);
        return;
    }

    let offset = size - max_size as u64;
    let mut buf = vec![0u8; max_size];
    let _ = file.seek(SeekFrom::Start(offset));
    let n = file.read(&mut buf).unwrap_or(0);

    if n > 0 {
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = file.write_all(&buf[..n]);
    }
    let _ = file.seek(SeekFrom::End(0));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_CTR: AtomicU32 = AtomicU32::new(0);

    fn tmp_path() -> std::path::PathBuf {
        let n = TEST_CTR.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("ztch-app-log-{}-{}", std::process::id(), n))
    }

    #[test]
    fn escape_line_makes_one_line() {
        assert_eq!(escape_line("a\r\nb"), "a\\r\\nb");
    }

    #[test]
    fn rotate_file_keeps_tail() {
        let p = tmp_path();
        {
            let mut f = std::fs::File::create(&p).unwrap();
            f.write_all(b"aaaabbbbcccc").unwrap();
        }

        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&p)
            .unwrap();
        rotate_file(&mut f, 8);

        let mut got = String::new();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.read_to_string(&mut got).unwrap();
        assert_eq!(got, "bbbbcccc");
        let _ = std::fs::remove_file(&p);
    }
}
