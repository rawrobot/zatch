/// Size of the pty read buffer and the tail-read chunk size.
pub const BUFSIZE: usize = 4096;

/// Capacity of the in-memory scrollback ring buffer.
pub const SCROLLBACK_SIZE: usize = 128 * 1024;

/// Default maximum size of the on-disk session log (1 MB).
pub const LOG_MAX_SIZE: usize = 1024 * 1024;

/// Message types sent between clients and the master daemon.
///
/// Each variant corresponds to a single-byte value in the wire
/// protocol, matching the C original's enum.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MsgType {
    Push = 0,
    Attach = 1,
    /// Sent by an attached client to signal it is leaving voluntarily.
    Detach = 2,
    Winch = 3,
    Redraw = 4,
    Kill = 5,
    /// Sent by a control client (e.g. `ztch detach`) to force all currently
    /// attached clients to disconnect.  The session itself keeps running.
    ForceDetach = 6,
    /// Sent by a control client (e.g. `ztch info`) to query attached-client
    /// information.  The master writes a text response and closes the connection.
    Info = 7,
}

impl MsgType {
    /// Convert a raw `u8` to a [`MsgType`].
    ///
    /// Returns `None` for values that do not correspond to any known variant.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Push),
            1 => Some(Self::Attach),
            2 => Some(Self::Detach),
            3 => Some(Self::Winch),
            4 => Some(Self::Redraw),
            5 => Some(Self::Kill),
            6 => Some(Self::ForceDetach),
            7 => Some(Self::Info),
            _ => None,
        }
    }
}

/// Wire-format packet: 10 bytes, `#[repr(C, packed)]`.
///
/// Layout (must match the C original exactly):
///   - `msg_type` (1 byte) — one of [`MsgType`]
///   - `len` (1 byte)      — payload length or sub-command
///   - `data` (8 bytes)    — opaque payload
///
/// For `Push` messages `data` holds up to 8 bytes of terminal input.
/// For `Winch` and `Redraw` messages `data` holds a `winsize` struct.
/// For `Kill` messages `data` is unused; `len` stores the signal number.
#[repr(C, packed)]
#[derive(Debug)]
pub struct Packet {
    pub msg_type: u8,
    pub len: u8,
    pub data: [u8; 8],
}

/// Error returned by [`Packet::read_from`] and [`Packet::write_to`].
#[derive(Debug, PartialEq, Eq)]
pub enum ProtocolError {
    /// Remote end closed the connection (read returned 0, or write returned 0).
    Eof,
    /// Transient: `EAGAIN` / `EWOULDBLOCK` — the caller should defer and retry.
    WouldBlock,
    /// Unrecoverable OS error.  The inner value is the raw `errno`.
    Io(i32),
}

impl ProtocolError {
    /// Map to a raw errno integer for callers that still need one.
    pub fn as_errno(&self) -> i32 {
        match self {
            Self::Eof => 0,
            Self::WouldBlock => libc::EAGAIN,
            Self::Io(e) => *e,
        }
    }
}

impl Packet {
    /// Build a zeroed packet with the given message type.
    pub fn new(t: MsgType) -> Self {
        Packet {
            msg_type: t as u8,
            len: 0,
            data: [0u8; 8],
        }
    }

    /// Build a packet with a message type and a `len` value.
    ///
    /// This is a convenience for one-shot control packets (e.g. Kill)
    /// where `data` is left zeroed.
    pub fn encode(t: MsgType, len: u8) -> Self {
        Packet {
            msg_type: t as u8,
            len,
            data: [0u8; 8],
        }
    }

    /// Read exactly one `Packet` from `fd`.
    ///
    /// Handles partial reads (common with non-blocking sockets) by looping
    /// until all 10 bytes arrive.  `EINTR` is retried transparently inside
    /// the loop; `EAGAIN` / `EWOULDBLOCK` surfaces as
    /// [`ProtocolError::WouldBlock`] so the caller can defer.
    ///
    /// # Errors
    ///
    /// - [`ProtocolError::Eof`] — remote end closed the connection.
    /// - [`ProtocolError::WouldBlock`] — fd not ready; caller should retry later.
    /// - [`ProtocolError::Io`] — unrecoverable OS error.
    pub fn read_from(fd: std::os::unix::io::RawFd) -> Result<Self, ProtocolError> {
        let mut pkt = Packet::new(MsgType::Push);
        let ptr = &mut pkt as *mut Packet as *mut libc::c_void;
        let size = std::mem::size_of::<Packet>();
        let mut off = 0usize;
        while off < size {
            let n = unsafe { libc::read(fd, ptr.add(off), size - off) };
            if n > 0 {
                off += n as usize;
            } else if n == 0 {
                return Err(ProtocolError::Eof);
            } else {
                let err = std::io::Error::last_os_error();
                match err.kind() {
                    std::io::ErrorKind::Interrupted => continue,
                    std::io::ErrorKind::WouldBlock => return Err(ProtocolError::WouldBlock),
                    _ => return Err(ProtocolError::Io(err.raw_os_error().unwrap_or(libc::EIO))),
                }
            }
        }
        Ok(pkt)
    }

    /// Write exactly one `Packet` to `fd`.
    ///
    /// Handles partial writes by looping until all 10 bytes are sent.
    /// `EINTR` is retried transparently; `EAGAIN` surfaces as
    /// [`ProtocolError::WouldBlock`].  A zero-byte write (connection closed
    /// mid-packet) returns [`ProtocolError::Eof`] rather than spinning
    /// forever.
    ///
    /// # Errors
    ///
    /// - [`ProtocolError::Eof`] — peer closed the connection mid-write.
    /// - [`ProtocolError::WouldBlock`] — fd not ready; caller should retry later.
    /// - [`ProtocolError::Io`] — unrecoverable OS error.
    pub fn write_to(&self, fd: std::os::unix::io::RawFd) -> Result<(), ProtocolError> {
        let ptr = self as *const Packet as *const libc::c_void;
        let size = std::mem::size_of::<Packet>();
        let mut off = 0usize;
        while off < size {
            let n = unsafe { libc::write(fd, ptr.add(off), size - off) };
            if n > 0 {
                off += n as usize;
            } else if n == 0 {
                return Err(ProtocolError::Eof);
            } else {
                let err = std::io::Error::last_os_error();
                match err.kind() {
                    std::io::ErrorKind::Interrupted => continue,
                    std::io::ErrorKind::WouldBlock => return Err(ProtocolError::WouldBlock),
                    _ => return Err(ProtocolError::Io(err.raw_os_error().unwrap_or(libc::EIO))),
                }
            }
        }
        Ok(())
    }
}

/// How the master should respond to a [`Redraw`](MsgType::Redraw) request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RedrawMethod {
    Unspec = 0,
    None = 1,
    CtrlL = 2,
    Winch = 3,
}

/// How the attach client should clear the terminal before attachment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClearMethod {
    Unspec = 0,
    None = 1,
    Move = 2,
}

/// Best-effort write of `buf` to `fd`.
///
/// Loops on partial writes and `EINTR` but gives up on other errors.
/// Errors are silently ignored — this is used for stdout/stderr where
/// blocking or crashing on a broken pipe is worse than losing output.
pub fn write_buf(fd: std::os::unix::io::RawFd, buf: &[u8]) {
    let mut off = 0usize;
    while off < buf.len() {
        let n = unsafe {
            libc::write(
                fd,
                buf.as_ptr().add(off) as *const libc::c_void,
                buf.len() - off,
            )
        };
        if n > 0 {
            off += n as usize;
        } else if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_new() {
        let p = Packet::new(MsgType::Push);
        assert_eq!(p.msg_type, 0);
        assert_eq!(p.len, 0);
        assert_eq!(p.data, [0u8; 8]);

        let p = Packet::new(MsgType::Kill);
        assert_eq!(p.msg_type, 5);
    }

    #[test]
    fn packet_encode() {
        let p = Packet::encode(MsgType::Kill, 9);
        assert_eq!(p.msg_type, 5);
        assert_eq!(p.len, 9);
        assert_eq!(p.data, [0u8; 8]);
    }

    #[test]
    fn packet_roundtrip() {
        let mut fds = [-1i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (r, w) = (fds[0], fds[1]);

        let out = Packet::encode(MsgType::Push, 3);
        out.write_to(w).unwrap();

        let inp = Packet::read_from(r).unwrap();
        assert_eq!(inp.msg_type, 0);
        assert_eq!(inp.len, 3);

        unsafe {
            libc::close(r);
            libc::close(w);
        }
    }

    #[test]
    fn packet_read_eof() {
        let mut fds = [-1i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (r, w) = (fds[0], fds[1]);
        unsafe {
            libc::close(w);
        }

        let err = Packet::read_from(r).unwrap_err();
        assert_eq!(err, ProtocolError::Eof);
        unsafe {
            libc::close(r);
        }
    }

    #[test]
    fn write_buf_works() {
        let mut fds = [-1i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (r, w) = (fds[0], fds[1]);

        let msg = b"hello!";
        write_buf(w, msg);

        let mut buf = [0u8; 16];
        let n = unsafe { libc::read(r, buf.as_mut_ptr() as *mut libc::c_void, 16) };
        assert_eq!(n, 6);
        assert_eq!(&buf[..6], msg);

        unsafe {
            libc::close(r);
            libc::close(w);
        }
    }

    #[test]
    fn format_age_examples() {
        assert_eq!(format_age(0), "0s");
        assert_eq!(format_age(1), "1s");
        assert_eq!(format_age(59), "59s");
        assert_eq!(format_age(60), "1m 0s");
        assert_eq!(format_age(61), "1m 1s");
        assert_eq!(format_age(3599), "59m 59s");
        assert_eq!(format_age(3600), "1h 0m 0s");
        assert_eq!(format_age(3661), "1h 1m 1s");
        assert_eq!(format_age(86399), "23h 59m 59s");
        assert_eq!(format_age(86400), "1d 0h 0m 0s");
        assert_eq!(format_age(90061), "1d 1h 1m 1s");
    }

    #[test]
    fn msg_type_values() {
        assert_eq!(MsgType::Push as u8, 0);
        assert_eq!(MsgType::Attach as u8, 1);
        assert_eq!(MsgType::Detach as u8, 2);
        assert_eq!(MsgType::Winch as u8, 3);
        assert_eq!(MsgType::Redraw as u8, 4);
        assert_eq!(MsgType::Kill as u8, 5);
        assert_eq!(MsgType::ForceDetach as u8, 6);
        assert_eq!(MsgType::Info as u8, 7);
    }

    #[test]
    fn msg_type_from_u8_all_variants() {
        assert_eq!(MsgType::from_u8(0), Some(MsgType::Push));
        assert_eq!(MsgType::from_u8(1), Some(MsgType::Attach));
        assert_eq!(MsgType::from_u8(2), Some(MsgType::Detach));
        assert_eq!(MsgType::from_u8(3), Some(MsgType::Winch));
        assert_eq!(MsgType::from_u8(4), Some(MsgType::Redraw));
        assert_eq!(MsgType::from_u8(5), Some(MsgType::Kill));
        assert_eq!(MsgType::from_u8(6), Some(MsgType::ForceDetach));
        assert_eq!(MsgType::from_u8(7), Some(MsgType::Info));
    }

    #[test]
    fn msg_type_from_u8_invalid() {
        assert_eq!(MsgType::from_u8(8), None);
        assert_eq!(MsgType::from_u8(255), None);
    }

    #[test]
    fn packet_size() {
        assert_eq!(std::mem::size_of::<Packet>(), 10);
    }
}

/// Format a duration in seconds into a human-readable string.
///
/// Examples:
///   - `0`       → `"0s"`
///   - `65`      → `"1m 5s"`
///   - `3661`    → `"1h 1m 1s"`
///   - `90061`   → `"1d 1h 1m 1s"`
pub fn format_age(secs: u64) -> String {
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if d > 0 {
        format!("{}d {}h {}m {}s", d, h, m, s)
    } else if h > 0 {
        format!("{}h {}m {}s", h, m, s)
    } else if m > 0 {
        format!("{}m {}s", m, s)
    } else {
        format!("{}s", s)
    }
}
