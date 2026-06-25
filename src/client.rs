use std::os::unix::io::RawFd;

/// A connected client.
pub(crate) struct Client {
    /// Unix socket fd for this client connection.
    pub(crate) fd: RawFd,
    /// Whether this client is currently receiving live PTY output.
    /// False while scrollback replay is in progress.
    pub(crate) attached: bool,
    /// Read offset into the scrollback ring buffer for the ongoing replay.
    pub(crate) replay_head: usize,
    /// Bytes of scrollback still to be sent before the client is fully caught up.
    pub(crate) replay_remaining: usize,
    /// PID of the peer process, obtained via `SO_PEERCRED` on accept.
    /// Zero when the kernel did not provide credentials.
    pub(crate) peer_pid: libc::pid_t,
}
