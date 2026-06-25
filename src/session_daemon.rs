//! Session daemon — the master process.
//!
//! After a session is created the daemon forks into the background and runs
//! an event loop built around [`select`](libc::select).  It monitors:
//!
//! 1. The **control socket** — where attach/push/kill clients connect.
//! 2. The **PTY master fd** — forwards child output to attached clients and
//!    detects when the child exits.
//! 3. **Client fds** — reads stdin data (Push), attach/detach handshakes,
//!    winsize changes (Winch), redraw requests, and kill commands.
//!
//! The [`SessionDaemon`] struct owns all daemon state.  The entry point is
//! [`session_daemon_main`], which creates the listener, forks the daemon, and
//! delegates to [`SessionDaemon::run`].

use std::os::unix::io::{AsRawFd, IntoRawFd, RawFd};
use std::os::unix::net::UnixListener;
use std::process;

use crate::Session;
use crate::app_log::{AppLog, LogLevel};
use crate::client::Client;
use crate::protocol::*;
use crate::pty::Pty;
use crate::scrollback::{ReplayState, Scrollback};
use crate::session_log::SessionLog;
use crate::sock;
use crate::util::session_shortname;

/// Remove a session's socket and optionally close the log file.
///
/// If `log` is `Some`, a session-ended marker is written before closing.
fn cleanup_session(sockname: &str, log: &mut Option<SessionLog>, progname: &str) {
    if let Some(l) = log.take() {
        let marker = format!(
            "\r\n[{}: session '{}' ended]\r\n",
            progname,
            session_shortname(sockname)
        );
        l.close(&marker);
    }
    let _ = std::fs::remove_file(sockname);
}

/// Install `SIG_IGN` handlers for signals the daemon must ignore.
fn setup_signals() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGTTIN, libc::SIG_IGN);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }
}

/// Redirect stdin, stdout and stderr to `/dev/null`.
///
/// Called after the daemon detaches from the terminal so background I/O
/// does not interact with the controlling terminal.
fn redirect_to_devnull() {
    use std::os::unix::io::IntoRawFd;
    if let Ok(null) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
    {
        // Take ownership of the fd so File::drop does not close it prematurely;
        // we close it manually below after dup2-ing it onto 0/1/2.
        let nullfd = null.into_raw_fd();
        unsafe {
            libc::dup2(nullfd, 0);
            libc::dup2(nullfd, 1);
            libc::dup2(nullfd, 2);
            if nullfd > 2 {
                libc::close(nullfd);
            }
        }
    }
}

/// Return the PID of the peer on a Unix-domain socket via `SO_PEERCRED`.
///
/// Returns 0 when credentials are unavailable (very old kernels).
fn peer_pid(fd: RawFd) -> libc::pid_t {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let ok = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if ok == 0 { cred.pid } else { 0 }
}

/// Toggle the executable bit on the session socket.
///
/// When at least one client is attached (`exec = true`) the socket is made
/// executable so that `ls -L` on unix sockets acts as a connectivity check.
/// When no clients are attached the executable bit is removed.
fn update_socket_modes(sockname: &str, exec: bool) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = match std::fs::metadata(sockname) {
        Ok(m) => m,
        Err(_) => return,
    };
    let mode = meta.mode();
    // S_IXUSR = 0o100 (owner execute bit)
    let new_mode = if exec { mode | 0o100 } else { mode & !0o100 };
    if new_mode != mode {
        let _ = std::fs::set_permissions(sockname, std::fs::Permissions::from_mode(new_mode));
    }
}

/// Deserialise a `libc::winsize` from the 8-byte packet payload.
///
/// The layout mirrors what [`get_winsize_raw`](crate::session) writes:
/// `ws_row | ws_col | ws_xpixel | ws_ypixel`, each as a native-endian `u16`.
/// Using explicit field assignment avoids `std::mem::transmute` and makes the
/// byte mapping visible without relying on platform struct layout assumptions.
///
/// Native-endian equals little-endian on all three supported targets
/// (x86_64, aarch64, arm/aarch32 Linux), so encoding is consistent across
/// a mixed-arch cluster.  The compile-time assert below catches any future
/// platform where `libc::winsize` grows or shrinks.
// Verify that libc::winsize is still exactly four u16 fields (8 bytes) on
// every platform we compile for.  Fails at compile time if the assumption
// ever breaks (e.g. a hypothetical big-endian port would still build but
// the assert would at least flag the changed size if the kernel ABI shifted).
const _WINSIZE_SIZE_CHECK: () = assert!(
    std::mem::size_of::<libc::winsize>() == 8,
    "libc::winsize is not 8 bytes — winsize_from_bytes field offsets are wrong"
);
fn winsize_from_bytes(data: &[u8; 8]) -> libc::winsize {
    libc::winsize {
        ws_row: u16::from_ne_bytes([data[0], data[1]]),
        ws_col: u16::from_ne_bytes([data[2], data[3]]),
        ws_xpixel: u16::from_ne_bytes([data[4], data[5]]),
        ws_ypixel: u16::from_ne_bytes([data[6], data[7]]),
    }
}

/// The daemon state machine.
///
/// Owns the listener socket, the PTY, all client connections, the scrollback
/// ring buffer, and the optional log file.  Created once per session by
/// [`SessionDaemon::new`] and driven by [`SessionDaemon::run`].
struct SessionDaemon {
    /// File descriptor of the listener socket (used in `select`).
    listener_fd: RawFd,
    /// The listener socket itself (for `accept`).
    listener: UnixListener,
    /// The pseudo-terminal and child PID.
    pty: Pty,
    /// All currently-connected clients.
    clients: Vec<Client>,
    /// Ring buffer of PTY output for replay-on-attach.
    scrollback: Scrollback,
    /// On-disk session log (optional, gated by `log_max_size`).
    log: Option<SessionLog>,
    /// Path to the session socket (used for `cleanup`).
    sockname: String,
    /// `argv[0]` of the daemon binary (used for error messages).
    progname: String,
    /// Application log context.
    app_log: AppLog,
    /// Whether at least one client is attached (drives socket mode).
    has_attached: bool,
    /// Set by `handle_force_detach`; cleared and acted on in `run` after each
    /// packet batch so we never mutate `clients` while iterating it.
    evict_attached: bool,
}

impl SessionDaemon {
    /// Create a new daemon instance.
    ///
    /// Performs initialisation that must happen before the event loop:
    ///
    /// 1. Captures the terminal window size.
    /// 2. Detaches from the terminal (`setsid`).
    /// 3. Opens the log file if logging is enabled.
    /// 4. Installs signal handlers.
    /// 5. Forks the PTY child (the shell).
    /// 6. Redirects its own stdin/stdout/stderr to `/dev/null`.
    ///
    /// # Errors
    ///
    /// Returns `Err(exit_code)` if the PTY cannot be created.
    fn new(
        listener: UnixListener,
        argv: &[std::ffi::CString],
        sess: &Session,
    ) -> Result<Self, i32> {
        let mut init_ws: libc::winsize = unsafe { std::mem::zeroed() };
        unsafe {
            libc::ioctl(
                0,
                libc::TIOCGWINSZ,
                &mut init_ws as *mut _ as *mut libc::c_void,
            );
        }
        if init_ws.ws_row == 0 {
            init_ws.ws_row = 24;
        }
        if init_ws.ws_col == 0 {
            init_ws.ws_col = 80;
        }

        unsafe {
            libc::setsid();
        }

        let mut log: Option<SessionLog> = None;
        if sess.log_max_size > 0 {
            let log_path = format!("{}.log", sess.sockname);
            log = SessionLog::open(&log_path, sess.log_max_size);
        }
        let scrollback = Scrollback::new();
        let clients = Vec::new();

        setup_signals();

        let pty = match Pty::new(argv, sess, &init_ws) {
            Ok(p) => p,
            Err(msg) => {
                sess.app_log.record(LogLevel::Error, &msg);
                eprintln!("{}: {}", sess.progname, msg);
                return Err(1);
            }
        };
        // Set PTY master fd to non-blocking so handle_pty_output never
        // blocks on read and can safely retry on EAGAIN / EINTR.
        {
            let flags = unsafe { libc::fcntl(pty.fd, libc::F_GETFL) };
            if flags >= 0 {
                unsafe {
                    libc::fcntl(pty.fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
            }
        }

        redirect_to_devnull();

        let listener_fd = listener.as_raw_fd();
        let sockname = sess.sockname.clone();
        let progname = sess.progname.clone();
        let app_log = sess.app_log.clone();

        Ok(SessionDaemon {
            listener_fd,
            listener,
            pty,
            clients,
            scrollback,
            log,
            sockname,
            progname,
            app_log,
            has_attached: false,
            evict_attached: false,
        })
    }

    /// Write a warning line to the session log (stderr is /dev/null in the daemon).
    fn log_warn(&mut self, msg: &str) {
        self.app_log.record(LogLevel::Warn, msg);
        if let Some(log) = &mut self.log {
            log.write(format!("[{}: warn: {}]\r\n", self.progname, msg).as_bytes());
        }
    }

    /// Run the main event loop until the child exits or a fatal error occurs.
    ///
    /// The loop:
    ///
    /// 1. Builds the fd sets for `select`.
    /// 2. Calls `select`.
    /// 3. Accepts new client connections.
    /// 4. Processes client packets (push, attach, detach, winch, redraw, kill).
    /// 5. Drains replay data to newly-attached clients.
    /// 6. Reads PTY output and forwards it to attached clients.
    ///
    /// When the PTY returns EOF (child exited) the method calls [`Self::cleanup`]
    /// and exits the process via [`process::exit`] with the child's exit status.
    fn run(&mut self) -> i32 {
        loop {
            let (mut rfds, mut wfds, maxfd) = self.build_fd_sets();

            let sel = unsafe {
                libc::select(
                    maxfd + 1,
                    &mut rfds,
                    &mut wfds,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if sel < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }

            if unsafe { libc::FD_ISSET(self.listener_fd, &rfds) } {
                self.accept_client();
            }

            self.handle_client_packets(&rfds);

            if self.evict_attached {
                self.evict_attached = false;
                self.clients.retain(|c| {
                    if c.attached {
                        unsafe {
                            libc::close(c.fd);
                        }
                        false
                    } else {
                        true
                    }
                });
            }

            self.drain_replay(&wfds);
            self.handle_pty_output(&rfds);
        }

        self.cleanup();
        0
    }

    /// Build `rfds`, `wfds` and compute `maxfd` for the next `select` call.
    ///
    /// The read set always contains the listener socket and the PTY master fd.
    /// Client fds are added to the read set; client fds with pending replay
    /// data are also added to the write set.  Updates the socket executable
    /// bit based on whether any client is attached.
    fn build_fd_sets(&mut self) -> (libc::fd_set, libc::fd_set, RawFd) {
        let mut rfds: libc::fd_set = unsafe { std::mem::zeroed() };
        let mut wfds: libc::fd_set = unsafe { std::mem::zeroed() };
        unsafe {
            libc::FD_ZERO(&mut rfds);
            libc::FD_ZERO(&mut wfds);
        }

        let mut maxfd = self.listener_fd;
        unsafe {
            libc::FD_SET(self.listener_fd, &mut rfds);
        }

        unsafe {
            libc::FD_SET(self.pty.fd, &mut rfds);
        }
        if self.pty.fd > maxfd {
            maxfd = self.pty.fd;
        }

        let mut new_has = false;
        for c in &self.clients {
            unsafe {
                libc::FD_SET(c.fd, &mut rfds);
            }
            if c.fd > maxfd {
                maxfd = c.fd;
            }
            if c.attached {
                new_has = true;
            }
            if c.replay_remaining > 0 {
                unsafe {
                    libc::FD_SET(c.fd, &mut wfds);
                }
            }
        }

        update_socket_modes(&self.sockname, new_has);
        self.has_attached = new_has;

        (rfds, wfds, maxfd)
    }

    /// Accept a new client connection and register it.
    ///
    /// The socket is set to non-blocking mode immediately so that reads
    /// from this client never block the daemon.
    fn accept_client(&mut self) {
        if let Ok((stream, _)) = self.listener.accept() {
            if stream.set_nonblocking(true).is_err() {
                // stream drops here, closing the fd and rejecting the client
                self.log_warn("could not set client socket non-blocking; connection rejected");
                return;
            }
            let fd = stream.into_raw_fd();
            let peer_pid = peer_pid(fd);
            self.clients.push(Client {
                fd,
                attached: false,
                replay_head: 0,
                replay_remaining: 0,
                peer_pid,
            });
        }
    }

    /// Read and dispatch packets from every readable client fd.
    ///
    /// Clients that close their connection or encounter a read error are
    /// removed from the client list.  Each recognised packet is dispatched
    /// to the corresponding `handle_*` method.
    fn handle_client_packets(&mut self, rfds: &libc::fd_set) {
        let mut i = 0;
        while i < self.clients.len() {
            let cfd = self.clients[i].fd;
            if !unsafe { libc::FD_ISSET(cfd, rfds) } {
                i += 1;
                continue;
            }

            let pkt = match Packet::read_from(cfd) {
                Ok(pkt) => pkt,
                Err(ProtocolError::Eof) => {
                    unsafe {
                        libc::close(cfd);
                    }
                    self.clients.swap_remove(i);
                    continue;
                }
                Err(ProtocolError::WouldBlock) => {
                    i += 1;
                    continue;
                }
                Err(ProtocolError::Io(_)) => {
                    unsafe {
                        libc::close(cfd);
                    }
                    self.clients.swap_remove(i);
                    continue;
                }
            };

            let keep = match MsgType::from_u8(pkt.msg_type) {
                Some(MsgType::Push) => self.handle_push(&pkt),
                Some(MsgType::Attach) => self.handle_attach(i, cfd, &pkt),
                Some(MsgType::Detach) => self.handle_detach(i),
                Some(MsgType::Winch) => self.handle_winch(&pkt),
                Some(MsgType::Redraw) => self.handle_redraw(&pkt),
                Some(MsgType::Kill) => self.handle_kill(&pkt),
                Some(MsgType::ForceDetach) => self.handle_force_detach(),
                Some(MsgType::Info) => self.handle_info(cfd),
                None => true,
            };

            if keep {
                i += 1;
            } else {
                unsafe {
                    libc::close(cfd);
                }
                self.clients.swap_remove(i);
            }
        }
    }

    /// Handle a [`MsgType::Push`] packet: write data to the PTY master.
    fn handle_push(&mut self, pkt: &Packet) -> bool {
        let dlen = (pkt.len as usize).min(pkt.data.len());
        if dlen > 0 {
            unsafe {
                libc::write(self.pty.fd, pkt.data.as_ptr() as *const libc::c_void, dlen);
            }
        }
        true
    }

    /// Handle a [`MsgType::Attach`] packet.
    ///
    /// When `pkt.len != 0` the client is attached immediately (it was already
    /// attached and is just re-affirming).  When `pkt.len == 0` the client is
    /// requesting a fresh attachment and receives a scrollback replay first.
    fn handle_attach(&mut self, i: usize, cfd: RawFd, pkt: &Packet) -> bool {
        if pkt.len != 0 || self.scrollback.len == 0 {
            self.clients[i].attached = true;
        } else {
            self.clients[i].replay_head = self.scrollback.head;
            self.clients[i].replay_remaining = self.scrollback.len;
            let mut rs = ReplayState {
                head: self.scrollback.head,
                remaining: self.scrollback.len,
            };
            let ok = rs.drain(cfd, &self.scrollback);
            self.clients[i].replay_head = rs.head;
            self.clients[i].replay_remaining = rs.remaining;
            // Only mark attached once replay is *fully* drained.  Setting it
            // while replay_remaining > 0 would let send_to_clients push live
            // PTY output at the same time as drain_replay sends old scrollback,
            // interleaving the two streams on the client side.  drain_replay()
            // handles the completion path for partial initial drains.
            if ok && rs.remaining == 0 {
                self.clients[i].attached = true;
            }
        }
        true
    }

    /// Handle a [`MsgType::Detach`] packet: mark the client as detached.
    fn handle_detach(&mut self, i: usize) -> bool {
        self.clients[i].attached = false;
        true
    }

    /// Handle a [`MsgType::ForceDetach`] packet from a control client.
    ///
    /// Sets `evict_attached` so that `run` will close all currently attached
    /// client connections after the packet batch is fully processed.  Returns
    /// `false` so the control connection itself is removed immediately.
    fn handle_force_detach(&mut self) -> bool {
        self.evict_attached = true;
        false
    }

    /// Handle a [`MsgType::Info`] packet: write a plain-text summary of all
    /// attached clients to the requester and close the connection.
    ///
    /// Format:
    /// ```text
    /// attached: N
    /// pid: PPPP
    /// …
    /// ```
    /// PID is 0 when `SO_PEERCRED` was unavailable at accept time.
    fn handle_info(&self, requester_fd: RawFd) -> bool {
        let attached: Vec<&Client> = self.clients.iter().filter(|c| c.attached).collect();
        let header = format!("attached: {}\n", attached.len());
        write_buf(requester_fd, header.as_bytes());
        for c in &attached {
            let line = format!("pid: {}\n", c.peer_pid);
            write_buf(requester_fd, line.as_bytes());
        }
        false
    }

    /// Handle a [`MsgType::Winch`] packet: update the PTY window size.
    fn handle_winch(&mut self, pkt: &Packet) -> bool {
        let ws = winsize_from_bytes(&pkt.data);
        self.pty.set_winsize(&ws);
        true
    }

    /// Handle a [`MsgType::Redraw`] packet.
    ///
    /// Sends SIGWINCH to the child, or sends Ctrl-L through the PTY when
    /// the redraw method is `CtrlL` and the terminal is in raw mode.
    fn handle_redraw(&mut self, pkt: &Packet) -> bool {
        let ws = winsize_from_bytes(&pkt.data);
        self.pty.set_winsize(&ws);
        if pkt.len == RedrawMethod::CtrlL as u8 {
            let mut term: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(self.pty.fd, &mut term) } >= 0
                && (term.c_lflag & (libc::ECHO | libc::ICANON)) == 0
                && term.c_cc[libc::VMIN] == 1
            {
                let c = b'\x0c';
                unsafe {
                    libc::write(self.pty.fd, &c as *const u8 as *const libc::c_void, 1);
                }
            }
        } else if pkt.len == RedrawMethod::Winch as u8 {
            self.pty.kill(libc::SIGWINCH);
        }
        true
    }

    /// Handle a [`MsgType::Kill`] packet: send a signal to the child process
    /// group.  The signal comes from `pkt.len` (defaults to `SIGTERM`).
    fn handle_kill(&mut self, pkt: &Packet) -> bool {
        self.pty.kill(if pkt.len != 0 {
            pkt.len as i32
        } else {
            libc::SIGTERM
        });
        true
    }

    /// Drain buffered scrollback data to clients whose fds are writable.
    ///
    /// Clients that finish receiving their replay are marked as attached.
    fn drain_replay(&mut self, wfds: &libc::fd_set) {
        for c in &mut self.clients {
            if c.replay_remaining > 0 && unsafe { libc::FD_ISSET(c.fd, wfds) } {
                let mut rs = ReplayState {
                    head: c.replay_head,
                    remaining: c.replay_remaining,
                };
                let ok = rs.drain(c.fd, &self.scrollback);
                c.replay_head = rs.head;
                c.replay_remaining = rs.remaining;
                if ok && c.replay_remaining == 0 {
                    c.attached = true;
                }
            }
        }
    }

    /// Read PTY output and forward it to attached clients.
    ///
    /// When the read returns zero bytes the child has exited.  The method
    /// reaps the child, calls [`Self::cleanup`], and terminates the daemon
    /// via [`process::exit`].
    fn handle_pty_output(&mut self, rfds: &libc::fd_set) {
        if !unsafe { libc::FD_ISSET(self.pty.fd, rfds) } {
            return;
        }
        let mut buf = [0u8; BUFSIZE];
        let len =
            unsafe { libc::read(self.pty.fd, buf.as_mut_ptr() as *mut libc::c_void, BUFSIZE) };
        if len > 0 {
            let data = &buf[..len as usize];
            self.scrollback.append(data);
            if let Some(l) = &mut self.log {
                l.write(data);
            }
            self.send_to_clients(data);
            return;
        }
        if len < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted
                || e.kind() == std::io::ErrorKind::WouldBlock
            {
                return;
            }
        }
        let mut status = 0;
        let _ = unsafe { libc::waitpid(self.pty.pid, &mut status, 0) };
        self.cleanup();
        process::exit(if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            1
        });
    }

    /// Forward PTY data to every attached client.
    ///
    /// Uses an inner `select` loop to wait until at least one client is
    /// writable.  New connections arriving on the control socket or all
    /// clients detaching cause the loop to abort early.
    ///
    /// Clients whose writes fail (or are partial) are closed **and removed**
    /// from `self.clients` inside the same iteration.  Previously the code
    /// only called `libc::close` while iterating an immutable borrow, leaving
    /// a dangling fd in the vec.  On the next outer `select` that closed fd
    /// re-entered the fd_set, causing `select(2)` to return `EBADF` which
    /// terminated the daemon.
    fn send_to_clients(&mut self, data: &[u8]) {
        if self.clients.iter().filter(|c| c.attached).count() == 0 {
            return;
        }
        loop {
            let mut rfds: libc::fd_set = unsafe { std::mem::zeroed() };
            let mut wfds: libc::fd_set = unsafe { std::mem::zeroed() };
            unsafe {
                libc::FD_ZERO(&mut rfds);
                libc::FD_ZERO(&mut wfds);
                libc::FD_SET(self.listener_fd, &mut rfds);
            }
            let mut maxfd = self.listener_fd;
            for c in self.clients.iter() {
                if !c.attached {
                    continue;
                }
                unsafe {
                    libc::FD_SET(c.fd, &mut wfds);
                }
                if c.fd > maxfd {
                    maxfd = c.fd;
                }
            }
            let sel = unsafe {
                libc::select(
                    maxfd + 1,
                    &mut rfds,
                    &mut wfds,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if sel < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }

            // Collect fds that failed so we can close+evict after the read-only loop.
            let mut bad_fds: Vec<RawFd> = Vec::new();
            let mut any_written = false;
            for c in self.clients.iter() {
                if !c.attached || !unsafe { libc::FD_ISSET(c.fd, &wfds) } {
                    continue;
                }
                let n =
                    unsafe { libc::write(c.fd, data.as_ptr() as *const libc::c_void, data.len()) };
                if n == data.len() as isize {
                    any_written = true;
                } else if n < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock
                {
                    // transient; select reported writable but the socket buffer filled
                    continue;
                } else {
                    // write error or unexpected partial write — evict this client
                    bad_fds.push(c.fd);
                }
            }

            // Close and remove in one pass; retain() is O(n) but bad_fds is tiny.
            for fd in bad_fds {
                unsafe {
                    libc::close(fd);
                }
                self.clients.retain(|c| c.fd != fd);
            }

            if any_written {
                return;
            }
            if unsafe { libc::FD_ISSET(self.listener_fd, &rfds) } {
                return;
            }
            if self.clients.iter().filter(|c| c.attached).count() == 0 {
                return;
            }
        }
    }

    /// Close the log file and unlink the session socket.
    fn cleanup(&mut self) {
        if let Some(l) = self.log.take() {
            let marker = format!(
                "\r\n[{}: session '{}' ended]\r\n",
                self.progname,
                session_shortname(&self.sockname)
            );
            l.close(&marker);
        }
        let _ = std::fs::remove_file(&self.sockname);
    }
}

/// Entry point for session creation.
///
/// Creates the listener socket.  If `dontfork` is true, runs the daemon in
/// the foreground.  Otherwise forks a background daemon and monitors the
/// error pipe for exec failures.
pub fn session_daemon_main(
    sess: &Session,
    argv: &[std::ffi::CString],
    _waitattach: bool,
    dontfork: bool,
) -> i32 {
    let sockname = sess.sockname.clone();
    let listener = match sock::listen_unix(&sockname) {
        Ok(l) => l,
        Err(err) => {
            if err == libc::EADDRINUSE {
                sess.app_log.record(
                    LogLevel::Error,
                    &format!(
                        "session '{}' is already running",
                        session_shortname(&sockname)
                    ),
                );
                println!(
                    "{}: session '{}' is already running",
                    sess.progname,
                    session_shortname(&sockname)
                );
            } else {
                let msg = std::io::Error::from_raw_os_error(err);
                sess.app_log
                    .record(LogLevel::Error, &format!("{}: {}", sockname, msg));
                println!("{}: {}: {}", sess.progname, sockname, msg);
            }
            return 1;
        }
    };

    if dontfork {
        let mut master = match SessionDaemon::new(listener, argv, sess) {
            Ok(m) => m,
            Err(ec) => return ec,
        };
        return master.run();
    }

    // pipe2(O_CLOEXEC) sets close-on-exec atomically, avoiding a window
    // where a concurrent exec could inherit the fd.  Available on Linux
    // ≥ 2.6.27 (released 2008) on all supported targets: x86_64, aarch64,
    // and arm/aarch32.  No known supported deployment predates that kernel.
    let mut error_pipe = [-1i32; 2];
    unsafe {
        libc::pipe2(error_pipe.as_mut_ptr(), libc::O_CLOEXEC);
    }

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let msg = std::io::Error::last_os_error();
        sess.app_log
            .record(LogLevel::Error, &format!("fork: {}", msg));
        println!("{}: fork: {}", sess.progname, msg);
        cleanup_session(&sockname, &mut None, &sess.progname);
        return 1;
    }
    if pid == 0 {
        // Close the read end; we only write.
        if error_pipe[0] >= 0 {
            unsafe {
                libc::close(error_pipe[0]);
            }
        }
        let mut master = match SessionDaemon::new(listener, argv, sess) {
            Ok(m) => m,
            Err(ec) => {
                // Write one byte so the parent's read returns > 0 (failure signal).
                // SessionDaemon::new already printed the error message via eprintln!, so we
                // don't need to duplicate it — just signal the parent to return 1.
                if error_pipe[1] >= 0 {
                    let flag = [1u8];
                    unsafe {
                        libc::write(error_pipe[1], flag.as_ptr() as *const libc::c_void, 1);
                        libc::close(error_pipe[1]);
                    }
                }
                process::exit(ec);
            }
        };
        // Startup succeeded — close write end so parent's read gets EOF immediately.
        if error_pipe[1] >= 0 {
            unsafe {
                libc::close(error_pipe[1]);
            }
        }
        let exit_code = master.run();
        process::exit(exit_code);
    }

    if error_pipe[0] >= 0 && error_pipe[1] >= 0 {
        unsafe {
            libc::close(error_pipe[1]);
        }
        let mut flag = [0u8; 1];
        let n = unsafe { libc::read(error_pipe[0], flag.as_mut_ptr() as *mut libc::c_void, 1) };
        unsafe {
            libc::close(error_pipe[0]);
        }
        if n > 0 {
            // Child wrote a failure byte — it already printed the error to stderr.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            return 1;
        }
        // n == 0: write end was closed after successful startup (EOF = ok).
    }
    0
}

#[cfg(test)]
mod tests {
    use std::os::linux::fs::MetadataExt;

    use super::*;

    // ── cleanup_session tests ──

    #[test]
    fn cleanup_session_unlinks_socket() {
        let dir = std::env::temp_dir().join(format!("ztch-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sockpath = dir.join("test-sock");
        {
            let _ = std::os::unix::net::UnixListener::bind(&sockpath).expect("bind test socket");
        }
        assert!(sockpath.exists());
        let sock_str = sockpath.to_string_lossy().to_string();

        cleanup_session(&sock_str, &mut None, "test-prog");

        assert!(!sockpath.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_session_nonexistent() {
        cleanup_session("/nonexistent/ztch-test-sock", &mut None, "test-prog");
    }

    // ── update_socket_modes tests ──

    #[test]
    fn update_socket_modes_toggle() {
        let dir = std::env::temp_dir().join(format!("ztch-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sockpath = dir.join("mode-test");
        {
            let _ = std::os::unix::net::UnixListener::bind(&sockpath).expect("bind test socket");
        }
        let sock_str = sockpath.to_string_lossy().to_string();

        update_socket_modes(&sock_str, false);
        let meta = std::fs::metadata(&sockpath).unwrap();
        let initial_mode = meta.st_mode() as libc::mode_t;
        assert_eq!(initial_mode & libc::S_IXUSR, 0);

        update_socket_modes(&sock_str, true);
        let meta = std::fs::metadata(&sockpath).unwrap();
        assert_ne!(meta.st_mode() as libc::mode_t & libc::S_IXUSR, 0);

        update_socket_modes(&sock_str, false);
        let meta = std::fs::metadata(&sockpath).unwrap();
        assert_eq!(meta.st_mode() as libc::mode_t & libc::S_IXUSR, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_socket_modes_nonexistent() {
        update_socket_modes("/nonexistent/ztch-mode-test", false);
        update_socket_modes("/nonexistent/ztch-mode-test", true);
    }
}
