use std::os::unix::io::RawFd;

use crate::protocol::SCROLLBACK_SIZE;

/// Fixed-capacity ring buffer for replay-on-attach.
///
/// Capacity is [`SCROLLBACK_SIZE`] bytes.  When the buffer is full the
/// oldest data is overwritten.  The buffer is heap-allocated (`Vec<u8>`)
/// to avoid putting 128 KB on the call stack.
pub(crate) struct Scrollback {
    pub(crate) buf: Vec<u8>,
    pub(crate) head: usize,
    pub(crate) len: usize,
}

impl Scrollback {
    /// Create an empty scrollback buffer (heap-allocated).
    pub(crate) fn new() -> Self {
        Scrollback {
            buf: vec![0u8; SCROLLBACK_SIZE],
            head: 0,
            len: 0,
        }
    }

    /// Append `data` to the ring buffer.
    ///
    /// If `data` is larger than the buffer, only the last
    /// [`SCROLLBACK_SIZE`] bytes are stored.  Uses chunked
    /// `copy_from_slice` to avoid a byte-by-byte loop on the hot
    /// PTY-output path.
    pub(crate) fn append(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let data = if data.len() >= SCROLLBACK_SIZE {
            &data[data.len() - SCROLLBACK_SIZE..]
        } else {
            data
        };

        let mask = SCROLLBACK_SIZE - 1;
        let wp = (self.head + self.len) & mask;
        let n1 = (SCROLLBACK_SIZE - wp).min(data.len());
        self.buf[wp..wp + n1].copy_from_slice(&data[..n1]);
        if n1 < data.len() {
            self.buf[..data.len() - n1].copy_from_slice(&data[n1..]);
        }

        let new_total = self.len + data.len();
        if new_total <= SCROLLBACK_SIZE {
            self.len = new_total;
        } else {
            self.head = (self.head + new_total - SCROLLBACK_SIZE) & mask;
            self.len = SCROLLBACK_SIZE;
        }
    }
}

impl Default for Scrollback {
    fn default() -> Self {
        Self::new()
    }
}

/// Tracks the progress of sending scrollback data to a single client.
pub(crate) struct ReplayState {
    pub(crate) head: usize,
    pub(crate) remaining: usize,
}

impl ReplayState {
    /// Drain as much scrollback as possible to `client_fd`.
    ///
    /// Returns `true` when the write succeeded (potentially partially) so the
    /// caller should continue later.  Returns `false` on a write error other
    /// than `EAGAIN` / `EINTR`.
    pub(crate) fn drain(&mut self, client_fd: RawFd, scrollback: &Scrollback) -> bool {
        while self.remaining > 0 {
            let cont = SCROLLBACK_SIZE - self.head;
            let n = cont.min(self.remaining);
            let ret = unsafe {
                libc::write(
                    client_fd,
                    scrollback.buf.as_ptr().add(self.head) as *const libc::c_void,
                    n,
                )
            };
            if ret < 0 {
                match std::io::Error::last_os_error().kind() {
                    std::io::ErrorKind::Interrupted => continue,
                    std::io::ErrorKind::WouldBlock => return true,
                    _ => return false,
                }
            }
            self.head = (self.head + ret as usize) & (SCROLLBACK_SIZE - 1);
            self.remaining -= ret as usize;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Scrollback tests ──

    #[test]
    fn scrollback_new_is_empty() {
        let sb = Scrollback::new();
        assert_eq!(sb.len, 0);
        assert_eq!(sb.head, 0);
    }

    #[test]
    fn scrollback_append_small() {
        let mut sb = Scrollback::new();
        sb.append(b"hello");
        assert_eq!(sb.len, 5);
        let mut out = vec![0u8; 5];
        for i in 0..5 {
            out[i] = sb.buf[i];
        }
        assert_eq!(&out, b"hello");
    }

    #[test]
    fn scrollback_append_multiple() {
        let mut sb = Scrollback::new();
        sb.append(b"abc");
        sb.append(b"def");
        let mut out = vec![0u8; 6];
        for i in 0..6 {
            out[i] = sb.buf[(sb.head + i) & (SCROLLBACK_SIZE - 1)];
        }
        assert_eq!(&out, b"abcdef");
    }

    #[test]
    fn scrollback_append_wrap_around() {
        let mut sb = Scrollback::new();
        let chunk = vec![b'x'; SCROLLBACK_SIZE - 4];
        sb.append(&chunk);
        assert_eq!(sb.len, SCROLLBACK_SIZE - 4);

        sb.append(b"12345678");
        assert_eq!(sb.len, SCROLLBACK_SIZE);

        let tail_start = (sb.head + SCROLLBACK_SIZE - 12) & (SCROLLBACK_SIZE - 1);
        let mut tail = vec![0u8; 12];
        for i in 0..12 {
            tail[i] = sb.buf[(tail_start + i) & (SCROLLBACK_SIZE - 1)];
        }
        let expected: Vec<u8> = [b'x'; 4]
            .iter()
            .chain(b"12345678".iter())
            .copied()
            .collect();
        assert_eq!(tail, expected);
    }

    #[test]
    fn scrollback_append_larger_than_buffer() {
        let mut sb = Scrollback::new();
        let big = vec![b'a'; SCROLLBACK_SIZE + 100];
        sb.append(&big);
        assert_eq!(sb.len, SCROLLBACK_SIZE);
        for i in 0..SCROLLBACK_SIZE {
            assert_eq!(sb.buf[(sb.head + i) & (SCROLLBACK_SIZE - 1)], b'a');
        }
    }

    #[test]
    fn scrollback_append_empty() {
        let mut sb = Scrollback::new();
        sb.append(b"");
        assert_eq!(sb.len, 0);
        sb.append(b"x");
        assert_eq!(sb.len, 1);
    }

    // ── ReplayState tests ──

    #[test]
    fn replay_drain_complete() {
        let sb = {
            let mut s = Scrollback::new();
            s.append(b"scrollback data");
            s
        };
        let mut fds = [-1i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (r, w) = (fds[0], fds[1]);
        let flags = unsafe { libc::fcntl(w, libc::F_GETFL) };
        unsafe { libc::fcntl(w, libc::F_SETFL, flags | libc::O_NONBLOCK) };

        let mut rs = ReplayState {
            head: 0,
            remaining: sb.len,
        };
        let ok = rs.drain(w, &sb);
        assert!(ok);
        assert_eq!(rs.remaining, 0);

        let mut buf = [0u8; 64];
        let n = unsafe { libc::read(r, buf.as_mut_ptr() as *mut libc::c_void, 64) };
        assert_eq!(n, "scrollback data".len() as isize);
        assert_eq!(&buf[..n as usize], b"scrollback data");

        unsafe {
            libc::close(r);
            libc::close(w);
        }
    }

    #[test]
    fn replay_drain_partial() {
        let sb = {
            let mut s = Scrollback::new();
            let big = vec![b'x'; SCROLLBACK_SIZE];
            s.append(&big);
            s
        };
        let mut fds = [-1i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (r, w) = (fds[0], fds[1]);
        let flags = unsafe { libc::fcntl(w, libc::F_GETFL) };
        unsafe { libc::fcntl(w, libc::F_SETFL, flags | libc::O_NONBLOCK) };

        let mut rs = ReplayState {
            head: 0,
            remaining: sb.len,
        };
        let ok = rs.drain(w, &sb);
        assert!(ok);
        assert!(rs.remaining < sb.len);
        assert!(rs.remaining > 0);

        let _pipe_buf_size = {
            let mut buf = [0u8; 65536];
            let n = unsafe { libc::read(r, buf.as_mut_ptr() as *mut libc::c_void, 65536) };
            n as usize
        };
        let ok = rs.drain(w, &sb);
        assert!(ok);

        unsafe {
            libc::close(r);
            libc::close(w);
        }
    }
}
