use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};

/// Connect to a Unix domain socket at `path`.
///
/// Wraps [`UnixStream::connect`] and translates `std::io::Error` into
/// the raw errno integer so callers can match on specific errors
/// (e.g. `ENOENT`, `ECONNREFUSED`).
///
/// # Errors
///
/// Returns the POSIX errno on failure.  Common values:
///   - `ENOENT` — the socket file does not exist
///   - `ECONNREFUSED` — the file exists but nothing is listening
///
/// # Example
///
/// ```ignore
/// let stream = connect_unix("/tmp/my-session").expect("connect");
/// let fd = stream.as_raw_fd();  // for use with select(2)
/// ```
pub fn connect_unix(path: &str) -> Result<UnixStream, i32> {
    UnixStream::connect(path).map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))
}

/// Create and bind a listening Unix domain socket at `path`.
///
/// The socket is:
///   - bound with [`UnixListener::bind`]
///   - set to non-blocking mode so [`accept`](UnixListener::accept)
///     never blocks the event loop
///   - permissions set to `0600` (owner-only access)
///   - configured with a listen backlog of 128
///
/// # Errors
///
/// Returns the POSIX errno on failure.  Common values:
///   - `EADDRINUSE` — the socket file already exists and is active
///   - `EACCES` — permission denied on the parent directory
///
/// # Example
///
/// ```ignore
/// let listener = listen_unix("/tmp/my-session").expect("bind");
/// let fd = listener.as_raw_fd();  // for use with select(2)
/// ```
pub fn listen_unix(path: &str) -> Result<UnixListener, i32> {
    let listener = UnixListener::bind(path).map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
    let perm = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perm).map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_CTR: AtomicU32 = AtomicU32::new(0);

    fn tmp_path() -> (std::path::PathBuf, String) {
        let n = TEST_CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join("ztch-test-sock");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("sock-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_file(&path);
        let s = path.to_string_lossy().to_string();
        (path, s)
    }

    #[test]
    fn listen_and_connect() {
        let (_p, path) = tmp_path();
        let listener = listen_unix(&path).expect("listen");
        let _fd = listener.as_raw_fd();

        let stream = connect_unix(&path).expect("connect");
        assert!(stream.as_raw_fd() >= 0);

        // accept on listener
        let (accepted, _) = listener.accept().expect("accept");
        assert!(accepted.as_raw_fd() >= 0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn listen_eaddrinuse() {
        let (_p, path) = tmp_path();
        let _l1 = listen_unix(&path).expect("first listen");
        let err = listen_unix(&path).unwrap_err();
        assert_eq!(err, libc::EADDRINUSE);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn connect_enoent() {
        let path = "/tmp/ztch-test-sock-nonexistent-xxxxxxxx";
        let err = connect_unix(path).unwrap_err();
        assert_eq!(err, libc::ENOENT);
    }

    #[test]
    fn send_data_through() {
        let (_p, path) = tmp_path();
        let listener = listen_unix(&path).expect("listen");
        let stream = connect_unix(&path).expect("connect");

        let (accepted, _) = listener.accept().expect("accept");

        let msg = b"hello from sock";
        let n = unsafe {
            libc::write(
                stream.as_raw_fd(),
                msg.as_ptr() as *const libc::c_void,
                msg.len(),
            )
        };
        assert_eq!(n, msg.len() as isize);

        let mut buf = [0u8; 32];
        let n = unsafe {
            libc::read(
                accepted.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                32,
            )
        };
        assert_eq!(n, msg.len() as isize);
        assert_eq!(&buf[..n as usize], msg);

        let _ = std::fs::remove_file(&path);
    }
}
