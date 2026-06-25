//! Client-side operations: attach to sessions, push data, kill, list, remove.
//!
//! All CLI commands are implemented as methods on [`Session`], along with
//! private helpers ([`require_tty`], [`with_tty`]) and a public utility
//! ([`use_shell_if_no_cmd`]) used by the crate root for the implicit code
//! path.  The free function [`connect_socket`] is public because it is used
//! by the session-scanning logic in `rm` and `list`.

use std::io::Read;
use std::os::linux::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::process;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::app_log::{self, AppLog, LogLevel};
use crate::protocol::*;
use crate::sock;
use std::ffi::CString;

use crate::session_daemon::session_daemon_main;
use crate::util::{cstr, expand_sockname, get_session_dir, save_term, session_shortname};

/// User-facing session configuration and operations.
///
/// Constructed by [`main`](crate::main) from CLI arguments and passed to the
/// various [`impl Session`](Session) methods for attach, push, kill, rm, list,
/// and replay-log.
pub struct Session {
    pub progname: String,
    pub app_log: AppLog,
    pub session_envvar: String,
    pub sockname: String,
    pub detach_char: i32,
    pub no_suspend: bool,
    pub redraw_method: RedrawMethod,
    pub clear_method: ClearMethod,
    pub no_ansiterm: bool,
    pub quiet: bool,
    pub log_max_size: usize,
    pub orig_term: libc::termios,
    pub dont_have_tty: bool,
}

static SHOULD_DIE: AtomicBool = AtomicBool::new(false);
static WIN_CHANGED: AtomicBool = AtomicBool::new(false);
static TERM_BACKUP: OnceLock<libc::termios> = OnceLock::new();
static TERM_BACKED_UP: AtomicBool = AtomicBool::new(false);
static HOOK_SET: AtomicBool = AtomicBool::new(false);

extern "C" fn restore_term() {
    if TERM_BACKED_UP.load(Ordering::Relaxed)
        && let Some(term) = TERM_BACKUP.get()
    {
        unsafe {
            libc::tcsetattr(0, libc::TCSADRAIN, term);
        }
    }
}

extern "C" fn handle_die(_sig: i32) {
    restore_term();
    SHOULD_DIE.store(true, Ordering::SeqCst);
}
extern "C" fn handle_winch(_sig: i32) {
    WIN_CHANGED.store(true, Ordering::SeqCst);
}

fn set_signal(sig: i32, handler: usize) {
    unsafe {
        libc::signal(sig, handler);
    }
}

fn is_socket(mode: u64) -> bool {
    (mode as libc::mode_t & libc::S_IFMT) == libc::S_IFSOCK
}

/// Return the best available timestamp for a file as seconds since Unix epoch.
///
/// Uses `st_mtime` (modification time) from the kernel `stat` result.  Falls
/// back to `st_ctime` (inode-change time) when `st_mtime` is zero, which
/// happens on some overlay/tmpfs configurations for socket files.
/// Returns `now` if neither field is non-zero, so callers always get a
/// meaningful age instead of the "56 years ago" sentinel.
fn meta_secs(meta: &std::fs::Metadata, now: u64) -> u64 {
    let mt = meta.st_mtime();
    if mt > 0 {
        return mt as u64;
    }
    let ct = meta.st_ctime();
    if ct > 0 {
        return ct as u64;
    }
    now
}

/// Connect to a Unix domain socket at `name`.
///
/// Returns the connected `UnixStream` on success. On `ECONNREFUSED`, checks
/// whether the socket file still exists — returns `ENOTSOCK` if the path
/// is not a socket, or `ECONNREFUSED` if it is (session died without cleanup).
pub fn connect_socket(name: &str) -> Result<UnixStream, i32> {
    match sock::connect_unix(name) {
        Err(libc::ECONNREFUSED) => match std::fs::metadata(name) {
            Ok(meta) => {
                if !is_socket(meta.st_mode() as u64) {
                    Err(libc::ENOTSOCK)
                } else {
                    Err(libc::ECONNREFUSED)
                }
            }
            Err(_) => Err(libc::ECONNREFUSED),
        },
        other => other,
    }
}

/// Read the terminal window size and serialise it into `data`.
///
/// The 8-byte layout is `ws_row | ws_col | ws_xpixel | ws_ypixel`, each
/// as a native-endian `u16`.  This matches what the master deserialises with
/// [`winsize_from_bytes`](crate::master) so no `transmute` is needed on
/// either side.  Native-endian is little-endian on all supported targets
/// (x86_64, aarch64, arm/aarch32 Linux), so the encoding is stable across
/// a mixed-arch cluster where client and master run on different machines.
fn get_winsize_raw(fd: RawFd, data: &mut [u8; 8]) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe {
        libc::ioctl(
            fd,
            libc::TIOCGWINSZ,
            &mut ws as *mut libc::winsize as *mut libc::c_void,
        );
    }
    data[0..2].copy_from_slice(&ws.ws_row.to_ne_bytes());
    data[2..4].copy_from_slice(&ws.ws_col.to_ne_bytes());
    data[4..6].copy_from_slice(&ws.ws_xpixel.to_ne_bytes());
    data[6..8].copy_from_slice(&ws.ws_ypixel.to_ne_bytes());
}

fn fmt_age(start: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format_age(now.saturating_sub(start))
}

fn restore_and_exit(orig_term: &libc::termios, code: i32) -> ! {
    unsafe {
        libc::tcsetattr(0, libc::TCSADRAIN, orig_term);
    }
    TERM_BACKED_UP.store(false, Ordering::Relaxed);
    process::exit(code);
}

fn raw_terminal(orig: &libc::termios) -> libc::termios {
    let mut cur = *orig;
    cur.c_iflag &= !(libc::IGNBRK
        | libc::BRKINT
        | libc::PARMRK
        | libc::ISTRIP
        | libc::INLCR
        | libc::IGNCR
        | libc::ICRNL);
    cur.c_iflag &= !(libc::IXON | libc::IXOFF);
    cur.c_oflag |= libc::OPOST;
    cur.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
    cur.c_cflag &= !(libc::CSIZE | libc::PARENB);
    cur.c_cflag |= libc::CS8;
    cur.c_cc[libc::VMIN] = 1;
    cur.c_cc[libc::VTIME] = 0;
    let _ = TERM_BACKUP.set(*orig);
    TERM_BACKED_UP.store(true, Ordering::Relaxed);
    unsafe {
        libc::tcsetattr(0, libc::TCSADRAIN, &cur);
        let _ = libc::atexit(restore_term);
        if HOOK_SET
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            std::panic::set_hook(Box::new(|_| {
                restore_term();
            }));
        }
    }
    cur
}

/// Returns `false` and prints an error when the session has no controlling
/// terminal — attach commands cannot proceed without one.
fn require_tty(sess: &Session) -> bool {
    if sess.dont_have_tty {
        sess.app_log.record(
            LogLevel::Error,
            "attaching to a session requires a terminal.",
        );
        eprintln!(
            "{}: attaching to a session requires a terminal.",
            sess.progname
        );
        false
    } else {
        true
    }
}

/// If `args` is non-empty, converts each into a `CString`.  Otherwise returns
/// the value of `$SHELL` (or `/bin/sh` as fallback).
pub(crate) fn use_shell_if_no_cmd(args: &[String]) -> Vec<CString> {
    if !args.is_empty() {
        return args.iter().map(|a| cstr(a)).collect();
    }
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string());
    vec![cstr(&shell)]
}

/// Snapshot the current terminal attributes into `sess.orig_term` and record
/// whether stdin is a terminal in `sess.dont_have_tty`, then run `action`.
///
/// This function does **not** modify or restore the terminal itself.  Callers
/// that enter raw mode (via [`attach`](Session::attach)) are responsible for
/// restoring it through [`restore_and_exit`] on the way out.  Commands that
/// only start a daemon (`cmd_start`, `cmd_run`) use this helper solely to
/// propagate the original terminal state into the `Session` so that the master
/// can inherit the correct window size.
fn with_tty<F: FnOnce(&mut Session) -> i32>(sess: &mut Session, action: F) -> i32 {
    let (ot, nt) = save_term();
    sess.orig_term = ot;
    sess.dont_have_tty = nt;
    action(sess)
}

// ---------------------------------------------------------------------------
// Session methods
// ---------------------------------------------------------------------------

impl Session {
    /// Print `"{progname}: {args}"` to stderr unless quiet mode is on.
    fn warn(&self, args: std::fmt::Arguments<'_>) {
        let msg = args.to_string();
        self.app_log.record(LogLevel::Warn, &msg);
        if !self.quiet {
            eprintln!("{}: {}", self.progname, msg);
        }
    }

    /// Print `"{progname}: {args}"` to stdout unless quiet mode is on.
    fn info(&self, args: std::fmt::Arguments<'_>) {
        let msg = args.to_string();
        self.app_log.record(LogLevel::Info, &msg);
        if !self.quiet {
            println!("{}: {}", self.progname, msg);
        }
    }

    fn status(&self, level: LogLevel, args: std::fmt::Arguments<'_>) {
        let msg = args.to_string();
        self.app_log.record(level, &msg);
        println!("{}: {}", self.progname, msg);
    }

    /// Remove a file, warning on any error other than `NotFound`.
    fn remove_file_if_exists(&self, path: &std::path::Path) {
        if let Err(e) = std::fs::remove_file(path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            self.warn(format_args!("could not remove '{}': {}", path.display(), e));
        }
    }

    /// Attach to a running session.
    ///
    /// Connects to the session socket, sets the terminal to raw mode, and
    /// enters the main select loop that forwards stdin to the master and
    /// master output to stdout.  Handles window resize signals, the detach
    /// character, and the suspend key.
    pub fn attach(&self, noerror: bool) -> i32 {
        let (sock, skip_ring) = match self.try_connect(noerror) {
            Ok(v) => v,
            Err(code) => return code,
        };
        let s = sock.as_raw_fd();
        let session_start = self.socket_start_time();

        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
        }
        set_signal(libc::SIGHUP, handle_die as *const () as usize);
        set_signal(libc::SIGTERM, handle_die as *const () as usize);
        set_signal(libc::SIGINT, handle_die as *const () as usize);
        set_signal(libc::SIGQUIT, handle_die as *const () as usize);
        set_signal(libc::SIGWINCH, handle_winch as *const () as usize);

        let cur_term = raw_terminal(&self.orig_term);

        if self.clear_method == ClearMethod::Move && !self.no_ansiterm {
            write_buf(1, b"\x1bc");
        } else if !self.quiet && !skip_ring {
            write_buf(1, b"\r\n");
        }

        let mut pkt = Packet::new(MsgType::Attach);
        pkt.len = if skip_ring { 1 } else { 0 };
        if pkt.write_to(s).is_err() {
            self.warn(format_args!(
                "failed to send attach request to '{}'",
                session_shortname(&self.sockname)
            ));
            return 1;
        }

        pkt = Packet::new(MsgType::Redraw);
        pkt.len = self.redraw_method as u8;
        get_winsize_raw(0, &mut pkt.data);
        if pkt.write_to(s).is_err() {
            self.warn(format_args!(
                "failed to send redraw request to '{}'",
                session_shortname(&self.sockname)
            ));
            return 1;
        }

        event_loop(s, session_start, &cur_term, self)
    }

    /// Pipe stdin into a running session.
    pub fn push(&self) -> i32 {
        let sock = match connect_socket(&self.sockname) {
            Ok(s) => s,
            Err(err) => {
                self.status(
                    LogLevel::Error,
                    format_args!(
                        "{}: {}",
                        self.sockname,
                        std::io::Error::from_raw_os_error(err)
                    ),
                );
                return 1;
            }
        };
        let s = sock.as_raw_fd();
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        }

        loop {
            let mut pkt = Packet::new(MsgType::Push);
            let len = unsafe { libc::read(0, pkt.data.as_mut_ptr() as *mut libc::c_void, 8) };
            if len == 0 {
                return 0;
            }
            if len < 0 {
                self.status(
                    LogLevel::Error,
                    format_args!("{}: {}", self.sockname, std::io::Error::last_os_error()),
                );
                return 1;
            }
            pkt.len = len as u8;
            if pkt.write_to(s).is_err() {
                self.status(
                    LogLevel::Error,
                    format_args!("{}: {}", self.sockname, std::io::Error::last_os_error()),
                );
                return 1;
            }
        }
    }

    /// Stop a running session.
    ///
    /// Normal stop: sends `SIGHUP` to the session and polls the socket file
    /// for up to 100 ms.  If the session is still alive, escalates to
    /// `SIGKILL` and waits a further 300 ms.
    ///
    /// Forced stop (`force = true`): sends `SIGKILL` directly and polls for
    /// up to 1 000 ms.  Does not escalate further.
    pub fn kill(&self, force: bool) -> i32 {
        let name = session_shortname(&self.sockname).to_string();
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        }

        let (first_sig, first_wait, first_msg) = if force {
            (libc::SIGKILL, 10u32, "killed")
        } else {
            (libc::SIGHUP, 1u32, "stopped")
        };

        if let Err(err) = self.send_kill(first_sig) {
            return match err {
                libc::ENOENT => {
                    self.status(
                        LogLevel::Error,
                        format_args!("session '{}' does not exist", name),
                    );
                    1
                }
                libc::ECONNREFUSED => {
                    self.status(
                        LogLevel::Error,
                        format_args!("session '{}' is not running", name),
                    );
                    1
                }
                _ => {
                    self.status(
                        LogLevel::Error,
                        format_args!(
                            "{}: {}",
                            self.sockname,
                            std::io::Error::from_raw_os_error(err)
                        ),
                    );
                    1
                }
            };
        }
        for _ in 0..first_wait {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if self.session_gone() {
                self.info(format_args!("session '{}' {}", name, first_msg));
                return 0;
            }
        }

        if !force {
            let _ = self.send_kill(libc::SIGKILL);
            for _ in 0..3u32 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if self.session_gone() {
                    self.status(LogLevel::Info, format_args!("session '{}' killed", name));
                    return 0;
                }
            }
        }
        self.status(
            LogLevel::Error,
            format_args!("session '{}' did not stop", name),
        );
        1
    }

    /// Query attached-client information from a running session.
    ///
    /// Sends an [`Info`](MsgType::Info) packet; the master replies with a
    /// plain-text summary (one `attached: N` header line followed by one
    /// `pid: P` line per attached client) then closes the connection.
    pub fn cmd_info(&self) -> i32 {
        let name = session_shortname(&self.sockname).to_string();
        let sock = match connect_socket(&self.sockname) {
            Ok(s) => s,
            Err(libc::ENOENT) => {
                self.status(
                    LogLevel::Error,
                    format_args!("session '{}' does not exist", name),
                );
                return 1;
            }
            Err(libc::ECONNREFUSED) => {
                self.status(
                    LogLevel::Error,
                    format_args!("session '{}' is not running", name),
                );
                return 1;
            }
            Err(err) => {
                self.status(
                    LogLevel::Error,
                    format_args!(
                        "{}: {}",
                        self.sockname,
                        std::io::Error::from_raw_os_error(err)
                    ),
                );
                return 1;
            }
        };
        let pkt = Packet::new(MsgType::Info);
        if pkt.write_to(sock.as_raw_fd()).is_err() {
            self.warn(format_args!("failed to query session '{}'", name));
            return 1;
        }
        // Read the text response until the master closes the connection.
        let mut buf = [0u8; BUFSIZE];
        let mut out = String::new();
        let mut sock = sock;
        loop {
            match sock.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        self.app_log
            .record(LogLevel::Info, &format!("'{}': {}", name, out.trim_end()));
        print!("{}: '{}': {}", self.progname, name, out);
        0
    }

    /// Detach all clients from a single session without stopping it.
    ///
    /// `self.sockname` must already be set.  Connects to the session socket
    /// and sends a [`Detach`](MsgType::Detach) packet; the session keeps
    /// running and clients can re-attach later.
    pub fn cmd_detach(&self) -> i32 {
        let name = session_shortname(&self.sockname).to_string();
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        }
        let sock = match connect_socket(&self.sockname) {
            Ok(s) => s,
            Err(libc::ENOENT) => {
                self.status(
                    LogLevel::Error,
                    format_args!("session '{}' does not exist", name),
                );
                return 1;
            }
            Err(libc::ECONNREFUSED) => {
                self.status(
                    LogLevel::Error,
                    format_args!("session '{}' is not running", name),
                );
                return 1;
            }
            Err(err) => {
                self.status(
                    LogLevel::Error,
                    format_args!(
                        "{}: {}",
                        self.sockname,
                        std::io::Error::from_raw_os_error(err)
                    ),
                );
                return 1;
            }
        };
        let pkt = Packet::new(MsgType::ForceDetach);
        if pkt.write_to(sock.as_raw_fd()).is_err() {
            self.warn(format_args!("failed to send detach request to '{}'", name));
            return 1;
        }
        self.info(format_args!("session '{}' detached", name));
        0
    }

    /// Send a [`Detach`](MsgType::Detach) packet to every live session.
    ///
    /// Scans the session directory, skips sockets that do not respond, and
    /// reports how many sessions were detached.
    pub fn cmd_detach_all(&self) -> i32 {
        let dir = get_session_dir(&self.progname);
        let mut count = 0;
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        }
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "log").unwrap_or(false) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                let meta = match path.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if !is_socket(meta.st_mode() as u64) {
                    continue;
                }
                let path_str = path.to_string_lossy();
                let sock = match connect_socket(&path_str) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let pkt = Packet::new(MsgType::ForceDetach);
                if pkt.write_to(sock.as_raw_fd()).is_ok() {
                    self.info(format_args!("session '{}' detached", name));
                    count += 1;
                }
            }
        }
        if count == 0 {
            self.info(format_args!("no active sessions to detach"));
        }
        0
    }

    fn send_kill(&self, sig: i32) -> Result<(), i32> {
        let sock = connect_socket(&self.sockname)?;
        let s = sock.as_raw_fd();
        let pkt = Packet::encode(MsgType::Kill, sig as u8);
        pkt.write_to(s).map_err(|e| e.as_errno())
    }

    fn session_gone(&self) -> bool {
        matches!(std::fs::metadata(&self.sockname), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    }

    fn rm_single(&self) -> i32 {
        let name = session_shortname(&self.sockname).to_string();
        let log_path = format!("{}.log", self.sockname);
        match connect_socket(&self.sockname) {
            Ok(_sock) => {
                self.status(
                    LogLevel::Error,
                    format_args!(
                        "session '{}' is running (use '{} kill {}' first)",
                        name, self.progname, name
                    ),
                );
                1
            }
            Err(err) => {
                if err == libc::ECONNREFUSED {
                    self.remove_file_if_exists(std::path::Path::new(&self.sockname));
                    self.remove_file_if_exists(std::path::Path::new(&log_path));
                    self.info(format_args!("session '{}' removed", name));
                    return 0;
                }
                if err == libc::ENOENT {
                    match std::fs::remove_file(&log_path) {
                        Ok(()) => {
                            self.info(format_args!("session '{}' removed", name));
                            return 0;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            self.warn(format_args!("session '{}' does not exist", name));
                        }
                        Err(e) => {
                            self.warn(format_args!("could not remove '{}': {}", log_path, e));
                        }
                    }
                    return 1;
                }
                self.warn(format_args!(
                    "{}: {}",
                    self.sockname,
                    std::io::Error::from_raw_os_error(err)
                ));
                1
            }
        }
    }

    fn rm_stale_sockets(&self) -> usize {
        let dir = get_session_dir(&self.progname);
        let mut count = 0;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "log").unwrap_or(false) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                let meta = match path.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if !is_socket(meta.st_mode() as u64) {
                    continue;
                }
                match connect_socket(path.to_str().unwrap_or("")) {
                    Ok(_sock) => continue,
                    Err(e) if e == libc::ECONNREFUSED => {}
                    Err(_) => continue,
                }
                self.remove_file_if_exists(&path);
                self.remove_file_if_exists(&dir.join(format!("{}.log", name)));
                self.info(format_args!("removed {}", name));
                count += 1;
            }
        }
        count
    }

    fn rm_orphaned_logs(&self) -> usize {
        let dir = get_session_dir(&self.progname);
        let mut count = 0;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e != "log").unwrap_or(true) {
                    continue;
                }
                if entry.file_name() == app_log::APP_LOG_FILE_NAME {
                    continue;
                }
                let stem = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if dir.join(&stem).exists() {
                    continue;
                }
                self.remove_file_if_exists(&path);
                self.info(format_args!("removed {}", stem));
                count += 1;
            }
        }
        count
    }

    fn try_connect(&self, noerror: bool) -> Result<(UnixStream, bool), i32> {
        if let Ok(chain) = std::env::var(&self.session_envvar)
            && !chain.is_empty()
        {
            for seg in chain.split(':') {
                if seg == self.sockname {
                    if !noerror {
                        self.app_log.record(
                            LogLevel::Error,
                            &format!(
                                "cannot attach to session '{}' from within itself",
                                session_shortname(&self.sockname)
                            ),
                        );
                        println!(
                            "{}: cannot attach to session '{}' from within itself",
                            self.progname,
                            session_shortname(&self.sockname)
                        );
                    }
                    return Err(1);
                }
            }
        }

        let sock = match connect_socket(&self.sockname) {
            Ok(s) => s,
            Err(err) => {
                if !noerror && !self.replay_log(err) {
                    let name = session_shortname(&self.sockname);
                    match err {
                        libc::ENOENT => self.status(
                            LogLevel::Error,
                            format_args!("session '{}' does not exist", name),
                        ),
                        libc::ECONNREFUSED => self.status(
                            LogLevel::Error,
                            format_args!("session '{}' is not running", name),
                        ),
                        libc::ENOTSOCK => self.status(
                            LogLevel::Error,
                            format_args!("'{}' is not a valid session", name),
                        ),
                        _ => self.status(
                            LogLevel::Error,
                            format_args!(
                                "{}: {}",
                                self.sockname,
                                std::io::Error::from_raw_os_error(err)
                            ),
                        ),
                    }
                }
                // Preserve the real errno so attach(true) callers (implicit_attach)
                // can distinguish ENOENT / ECONNREFUSED from other failures and
                // decide whether to create a new session.
                return Err(err);
            }
        };
        let skip_ring = if self.replay_log(0) { 1 } else { 0 };
        Ok((sock, skip_ring != 0))
    }

    fn socket_start_time(&self) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        std::fs::metadata(&self.sockname)
            .ok()
            .map(|m| meta_secs(&m, now))
            .unwrap_or(now)
    }

    /// Replay the on-disk session log to stdout, if it exists.
    ///
    /// Reads the `.log` file associated with this session socket and writes
    /// its full contents to stdout verbatim.  When `saved_errno` is
    /// `ECONNREFUSED` an extra line is printed noting that the session ended
    /// unexpectedly (the process crashed without writing a clean exit marker).
    ///
    /// Returns `true` when a log was found and replayed, `false` otherwise.
    pub fn replay_log(&self, saved_errno: i32) -> bool {
        let log_path = format!("{}.log", self.sockname);
        let mut file = match std::fs::File::open(&log_path) {
            Ok(f) => f,
            Err(_) => return false,
        };
        let mut buf = [0u8; BUFSIZE];
        loop {
            let n = match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    self.warn(format_args!(
                        "error reading log for '{}': {}",
                        session_shortname(&self.sockname),
                        e
                    ));
                    break;
                }
            };
            use std::io::Write;
            let _ = std::io::stdout().write_all(&buf[..n]);
        }
        if saved_errno == libc::ECONNREFUSED {
            println!(
                "\r\n[{}: session '{}' ended unexpectedly]\r\n",
                self.progname,
                session_shortname(&self.sockname)
            );
        }
        true
    }

    /// Remove a session's socket and log file.
    ///
    /// If `all` is false, removes a single named session (refusing if still
    /// running). If `all` is true, scans the session directory, removes every
    /// stale socket (ECONNREFUSED) and its orphaned `.log`, and removes any
    /// `.log` without a corresponding socket.
    pub fn rm(&self, all: bool) -> i32 {
        if !all {
            return self.rm_single();
        }
        let count = self.rm_stale_sockets() + self.rm_orphaned_logs();
        if count == 0 {
            self.info(format_args!("nothing to remove"));
        } else {
            self.info(format_args!("{} session(s) removed", count));
        }
        0
    }

    /// List sessions in the session directory.
    ///
    /// Each entry shows the session name, age since last modification, and
    /// status (attached, stale, or exited). If `show_all`, orphaned `.log`
    /// files are shown as `[exited]`.
    pub fn list(&self, show_all: bool) -> i32 {
        let dir = get_session_dir(&self.progname);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut count = 0;

        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "log").unwrap_or(false) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                let meta = match path.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if !is_socket(meta.st_mode() as u64) {
                    continue;
                }
                let age = fmt_age(now.saturating_sub(meta_secs(&meta, now)));
                match connect_socket(path.to_str().unwrap_or("")) {
                    Ok(_sock) => {
                        let attached = (meta.st_mode() & libc::S_IXUSR) != 0;
                        if attached {
                            println!("{:<24} since {} ago [attached]", name, age);
                        } else {
                            println!("{:<24} since {} ago", name, age);
                        }
                        count += 1;
                    }
                    Err(e) if e == libc::ECONNREFUSED => {
                        println!("{:<24} since {} ago [stale]", name, age);
                        count += 1;
                    }
                    _ => {}
                }
            }
        }

        if show_all && let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e != "log").unwrap_or(true) {
                    continue;
                }
                if entry.file_name() == app_log::APP_LOG_FILE_NAME {
                    continue;
                }
                let stem = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if dir.join(&stem).exists() {
                    continue;
                }
                let age = path
                    .metadata()
                    .ok()
                    .map(|m| fmt_age(now.saturating_sub(meta_secs(&m, now))))
                    .unwrap_or_else(|| "unknown".to_string());
                println!("{:<24} since {} ago [exited]", stem, age);
                count += 1;
            }
        }

        if count == 0 && !self.quiet {
            self.app_log.record(LogLevel::Info, "no sessions");
            println!("(no sessions)");
        }
        0
    }

    /// Print the current session name from `$ZTCH_SESSION` (or the configured
    /// environment variable).
    pub fn cmd_current(&self) -> i32 {
        let chain = match std::env::var(&self.session_envvar) {
            Ok(s) if !s.is_empty() => s,
            _ => {
                self.app_log.record(LogLevel::Error, "not in a session");
                eprintln!("{}: not in a session", self.progname);
                return 1;
            }
        };
        let current = chain.rsplit(':').next().unwrap_or("");
        let name = current.rsplit('/').next().unwrap_or(current);
        self.app_log
            .record(LogLevel::Info, &format!("current session '{}'", name));
        println!("{}", name);
        0
    }

    /// Attach to a named session: resolve its socket, set up the terminal,
    /// require a TTY, and enter the event loop.
    pub fn cmd_attach(&mut self, session: &str) -> i32 {
        self.sockname = expand_sockname(&self.progname, session);
        with_tty(self, |c| {
            if !require_tty(c) {
                return 1;
            }
            c.attach(false)
        })
    }

    /// Create a new session: fork the master daemon, then attach.
    pub fn cmd_new(&mut self, session: &str, command: &[String]) -> i32 {
        self.sockname = expand_sockname(&self.progname, session);
        let cmd = use_shell_if_no_cmd(command);
        with_tty(self, |c| {
            if session_daemon_main(c, &cmd, true, false) != 0 {
                return 1;
            }
            c.info(format_args!(
                "session '{}' created",
                session_shortname(&c.sockname)
            ));
            if !require_tty(c) {
                return 1;
            }
            c.attach(false)
        })
    }

    /// Start a session as a background daemon (no attach).
    pub fn cmd_start(&mut self, session: &str, command: &[String]) -> i32 {
        self.sockname = expand_sockname(&self.progname, session);
        let cmd = use_shell_if_no_cmd(command);
        with_tty(self, |c| {
            if session_daemon_main(c, &cmd, false, false) != 0 {
                return 1;
            }
            c.info(format_args!(
                "session '{}' started",
                session_shortname(&c.sockname)
            ));
            0
        })
    }

    /// Run a command in a fresh session (foreground, no attach).
    pub fn cmd_run(&mut self, session: &str, command: &[String]) -> i32 {
        self.sockname = expand_sockname(&self.progname, session);
        let cmd = use_shell_if_no_cmd(command);
        with_tty(self, |c| session_daemon_main(c, &cmd, false, true))
    }

    /// Attach to, or create on ECONNREFUSED/ENOENT, a session.
    ///
    /// Used when the CLI binary is invoked without a recognised subcommand
    /// (the "implicit" code path).
    pub fn implicit_attach(&mut self, cmd: &[CString]) -> i32 {
        with_tty(self, |c| {
            if !require_tty(c) {
                return 1;
            }
            let result = c.attach(true);
            if result != 0 {
                let saved = result;
                if saved == libc::ECONNREFUSED || saved == libc::ENOENT {
                    c.replay_log(saved);
                    if saved == libc::ECONNREFUSED {
                        let _ = std::fs::remove_file(&c.sockname);
                    }
                    if session_daemon_main(c, cmd, true, false) != 0 {
                        return 1;
                    }
                    c.info(format_args!(
                        "session '{}' created",
                        session_shortname(&c.sockname)
                    ));
                    c.attach(false)
                } else {
                    1
                }
            } else {
                0
            }
        })
    }
}

// ---------------------------------------------------------------------------
// attach event loop
// ---------------------------------------------------------------------------

fn event_loop(s: RawFd, session_start: u64, cur_term: &libc::termios, sess: &Session) -> ! {
    loop {
        if SHOULD_DIE.load(Ordering::SeqCst) {
            let age = fmt_age(session_start);
            sess.app_log.record(
                LogLevel::Info,
                &format!(
                    "session '{}' detached after {}",
                    session_shortname(&sess.sockname),
                    age
                ),
            );
            println!(
                "\r\n[{}: session '{}' detached after {}]\r\n",
                sess.progname,
                session_shortname(&sess.sockname),
                age
            );
            restore_and_exit(&sess.orig_term, 0);
        }

        let mut rfds: libc::fd_set = unsafe { std::mem::zeroed() };
        unsafe {
            libc::FD_ZERO(&mut rfds);
            libc::FD_SET(0, &mut rfds);
            libc::FD_SET(s, &mut rfds);
        }
        let n = unsafe {
            libc::select(
                s + 1,
                &mut rfds,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            sess.app_log.record(
                LogLevel::Error,
                &format!(
                    "session '{}' select failed",
                    session_shortname(&sess.sockname)
                ),
            );
            println!(
                "\r\n[{}: session '{}' select failed]\r\n",
                sess.progname,
                session_shortname(&sess.sockname)
            );
            restore_and_exit(&sess.orig_term, 1);
        }

        let s_rdy = unsafe { libc::FD_ISSET(s, &rfds) };
        let stdin_rdy = unsafe { libc::FD_ISSET(0, &rfds) };

        if s_rdy {
            handle_server_data(s, session_start, sess);
        }

        if stdin_rdy {
            handle_stdin_data(s, session_start, cur_term, sess);
        }

        if WIN_CHANGED.swap(false, Ordering::SeqCst) {
            let mut pkt_ws = Packet::new(MsgType::Winch);
            get_winsize_raw(0, &mut pkt_ws.data);
            let _ = pkt_ws.write_to(s);
        }
    }
}

fn handle_server_data(s: RawFd, session_start: u64, sess: &Session) {
    let mut buf = [0u8; BUFSIZE];
    let len = unsafe { libc::read(s, buf.as_mut_ptr() as *mut libc::c_void, BUFSIZE) };
    if len == 0 {
        if !sess.quiet {
            let age = fmt_age(session_start);
            sess.app_log.record(
                LogLevel::Info,
                &format!(
                    "session '{}' exited after {}",
                    session_shortname(&sess.sockname),
                    age
                ),
            );
            println!(
                "\r\n[{}: session '{}' exited after {}]\r\n",
                sess.progname,
                session_shortname(&sess.sockname),
                age
            );
        }
        restore_and_exit(&sess.orig_term, 0);
    } else if len < 0 {
        // EINTR / EAGAIN: select will re-fire; treat as no data this iteration.
        let kind = std::io::Error::last_os_error().kind();
        if kind == std::io::ErrorKind::Interrupted || kind == std::io::ErrorKind::WouldBlock {
            return;
        }
        let age = fmt_age(session_start);
        sess.app_log.record(
            LogLevel::Error,
            &format!(
                "session '{}' read error after {}",
                session_shortname(&sess.sockname),
                age
            ),
        );
        println!(
            "\r\n[{}: session '{}' read error after {}]\r\n",
            sess.progname,
            session_shortname(&sess.sockname),
            age
        );
        restore_and_exit(&sess.orig_term, 1);
    }
    write_buf(1, &buf[..len as usize]);
}

fn handle_stdin_data(s: RawFd, session_start: u64, cur_term: &libc::termios, sess: &Session) {
    let mut pkt_data = Packet::new(MsgType::Push);
    let len = unsafe { libc::read(0, pkt_data.data.as_mut_ptr() as *mut libc::c_void, 8) };
    if len < 0 {
        // EINTR / EAGAIN: transient; select loop will retry.
        let kind = std::io::Error::last_os_error().kind();
        if kind == std::io::ErrorKind::Interrupted || kind == std::io::ErrorKind::WouldBlock {
            return;
        }
        sess.app_log.record(
            LogLevel::Error,
            &format!(
                "stdin read failed for session '{}'",
                session_shortname(&sess.sockname)
            ),
        );
        restore_and_exit(&sess.orig_term, 1);
    }
    if len == 0 {
        sess.app_log.record(
            LogLevel::Info,
            &format!(
                "stdin closed for session '{}'",
                session_shortname(&sess.sockname)
            ),
        );
        restore_and_exit(&sess.orig_term, 1);
    }
    pkt_data.len = len as u8;

    if !sess.no_suspend && pkt_data.data[0] == cur_term.c_cc[libc::VSUSP] {
        pkt_data.msg_type = MsgType::Detach as u8;
        let _ = pkt_data.write_to(s);
        unsafe {
            libc::tcsetattr(0, libc::TCSADRAIN, &sess.orig_term);
            write_buf(1, b"\r\n");
            libc::kill(libc::getpid(), libc::SIGTSTP);
            libc::tcsetattr(0, libc::TCSADRAIN, cur_term);
        }
        pkt_data = Packet::new(MsgType::Attach);
        pkt_data.len = 0;
        let _ = pkt_data.write_to(s);
        pkt_data = Packet::new(MsgType::Redraw);
        pkt_data.len = sess.redraw_method as u8;
        get_winsize_raw(0, &mut pkt_data.data);
        let _ = pkt_data.write_to(s);
        return;
    }

    if sess.detach_char >= 0 && pkt_data.data[0] as i32 == sess.detach_char {
        let age = fmt_age(session_start);
        sess.app_log.record(
            LogLevel::Info,
            &format!(
                "session '{}' detached after {}",
                session_shortname(&sess.sockname),
                age
            ),
        );
        println!(
            "\r\n[{}: session '{}' detached after {}]\r\n",
            sess.progname,
            session_shortname(&sess.sockname),
            age
        );
        restore_and_exit(&sess.orig_term, 0);
    }

    if pkt_data.data[0] == b'\x0c' {
        WIN_CHANGED.store(true, Ordering::SeqCst);
    }
    let _ = pkt_data.write_to(s);
}
