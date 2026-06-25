/// A rolling log file for terminal session output.
///
/// On construction the file is opened (or created) and immediately
/// pruned so that it never exceeds `max_size` bytes.  All subsequent
/// [`write`](SessionLog::write) calls append without further rotation.
///
/// Call [`close`](SessionLog::close) to write an end-of-session marker
/// before the underlying file descriptor is closed.
///
/// # Example
///
/// ```ignore
/// let mut log = SessionLog::open("/tmp/sess.log", 4096).expect("create log");
/// log.write(b"hello\n");
/// log.close("[session ended]\n");
/// ```
use std::io::{Read, Seek, SeekFrom, Write};
pub struct SessionLog {
    /// The backing file, opened read-write.
    file: std::fs::File,
    /// Maximum size in bytes.  The file is trimmed to this on open.
    max_size: usize,
}

impl SessionLog {
    /// Open (or create) a log file at `path`, keeping at most `max_size` bytes.
    ///
    /// If the existing file is larger than `max_size` the oldest bytes
    /// are discarded so that only the trailing `max_size` bytes remain.
    /// The file pointer is positioned at the end so that subsequent
    /// writes append.
    ///
    /// Returns `None` when the file cannot be opened or created.
    pub fn open(path: &str, max_size: usize) -> Option<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .ok()?;

        Self::rotate(&mut file, max_size);
        Some(SessionLog { file, max_size })
    }

    /// Append `data` to the log file.
    ///
    /// Errors from the underlying write are silently ignored (the log
    /// is best-effort and must never block or crash the session).
    pub fn write(&mut self, data: &[u8]) {
        let _ = self.file.write_all(data);
    }

    /// Write an end-of-session `marker` then close the log.
    ///
    /// After this call the log is consumed and the underlying file is
    /// closed.  The marker is typically something like
    /// `"[name: session 'foo' ended]\r\n"`.
    pub fn close(mut self, marker: &str) {
        let _ = self.file.write_all(marker.as_bytes());
    }

    /// Prune `file` so it contains at most `max_size` bytes.
    ///
    /// Works by seeking to `max_size` bytes from the end, reading the
    /// remainder, truncating the file, and writing the remainder back.
    /// The file position ends at the end (ready for appending).
    fn rotate(file: &mut std::fs::File, max_size: usize) {
        let size = file.seek(SeekFrom::End(0)).unwrap_or(0);
        if size <= max_size as u64 {
            // Nothing to trim; just leave the cursor at the end.
            return;
        }

        let offset = size - max_size as u64;
        let mut buf = vec![0u8; max_size];

        file.seek(SeekFrom::Start(offset)).ok();
        let n = file.read(&mut buf).unwrap_or(0);

        if n > 0 {
            file.set_len(0).ok();
            file.seek(SeekFrom::Start(0)).ok();
            let _ = file.write_all(&buf[..n]);
        }

        // Advance the cursor to the end for subsequent appends.
        file.seek(SeekFrom::End(0)).ok();
    }
}

impl std::fmt::Debug for SessionLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionLog")
            .field("max_size", &self.max_size)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom};
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_CTR: AtomicU32 = AtomicU32::new(0);

    fn tmp_path() -> std::path::PathBuf {
        let n = TEST_CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join("ztch-test-log");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("log-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn open_creates_file() {
        let p = tmp_path();
        assert!(!p.exists());
        let _log = SessionLog::open(p.to_str().unwrap(), 1024).expect("open");
        assert!(p.exists());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn write_appends() {
        let p = tmp_path();
        let mut log = SessionLog::open(p.to_str().unwrap(), 1024).expect("open");
        log.write(b"hello\n");
        log.write(b"world\n");

        let content = std::fs::read_to_string(&p).unwrap();
        assert_eq!(content, "hello\nworld\n");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn close_writes_marker() {
        let p = tmp_path();
        let log = SessionLog::open(p.to_str().unwrap(), 1024).expect("open");
        log.close("[ended]\n");

        let content = std::fs::read_to_string(&p).unwrap();
        assert_eq!(content, "[ended]\n");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn rotate_trims_to_max_size() {
        let p = tmp_path();
        let max = 32usize;

        // Write 64 bytes.
        let mut log = SessionLog::open(p.to_str().unwrap(), max).expect("open");
        log.write(b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
        log.write(b"BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB");
        // At this point the on-disk file is 64 bytes; SessionLog::open below will trim it.

        // Close and re-open — rotation happens on open.
        log.close("");
        let mut log2 = SessionLog::open(p.to_str().unwrap(), max).expect("open");

        // Write a tiny footer to prove the cursor is at end after rotation.
        log2.write(b"CC");

        let mut file = std::fs::File::open(&p).unwrap();
        let size = file.seek(SeekFrom::End(0)).unwrap();
        assert!(size as usize <= max + 2, "size={} max={}", size, max);

        let mut buf = String::new();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.read_to_string(&mut buf).unwrap();

        // Should contain only the tail of the 64 bytes + "CC".
        assert!(buf.ends_with("CC"), "buf ends with CC, got: {:?}", buf);
        // The first character should be 'B' (we kept the last 32 of 64 written bytes).
        assert_eq!(buf.chars().next(), Some('B'));

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn rotate_noop_when_under_max() {
        let p = tmp_path();
        let mut log = SessionLog::open(p.to_str().unwrap(), 1024).expect("open");
        log.write(b"small");

        // Re-open — file is 5 bytes, well under 1024, so no trimming.
        log.close("");
        let _log2 = SessionLog::open(p.to_str().unwrap(), 1024).expect("open");

        let content = std::fs::read_to_string(&p).unwrap();
        assert_eq!(content, "small");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn debug_format() {
        let p = tmp_path();
        let log = SessionLog::open(p.to_str().unwrap(), 42).expect("open");
        let s = format!("{:?}", log);
        assert!(s.contains("SessionLog"));
        assert!(s.contains("42"));
        let _ = std::fs::remove_file(&p);
    }
}
