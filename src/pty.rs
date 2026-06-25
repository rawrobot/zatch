use std::io::Write;
use std::os::unix::io::RawFd;

use crate::Session;
use crate::app_log::LogLevel;

/// A pseudo-terminal master fd and the PID of the child process.
pub(crate) struct Pty {
    pub(crate) fd: RawFd,
    pub(crate) pid: libc::pid_t,
    #[allow(dead_code)]
    pub(crate) ws: libc::winsize,
}

impl Pty {
    /// Fork a child process in a new PTY.
    ///
    /// The child runs `argv[0]` with the given arguments.  The parent receives
    /// the PTY master fd and child PID.  Returns an error when the system is out
    /// of PTYs.
    pub(crate) fn new(
        argv: &[std::ffi::CString],
        sess: &Session,
        _ws: &libc::winsize,
    ) -> Result<Self, String> {
        let mut master: libc::c_int = 0;
        let ret = unsafe {
            libc::forkpty(
                &mut master,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if ret < 0 {
            return Err("Could not find a pty.".to_string());
        }
        if ret == 0 {
            let prev = std::env::var(&sess.session_envvar).ok();
            let chain = match prev {
                Some(p) if !p.is_empty() => format!("{}:{}", p, sess.sockname),
                _ => sess.sockname.clone(),
            };

            // Safe: we are in the single-threaded child after fork, before exec.
            let key = std::ffi::CString::new(sess.session_envvar.as_bytes()).unwrap();
            let val = std::ffi::CString::new(chain.as_bytes()).unwrap();
            unsafe {
                libc::setenv(key.as_ptr(), val.as_ptr(), 1);
            };

            let mut c_args: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
            c_args.push(std::ptr::null());
            unsafe {
                libc::execvp(argv[0].as_ptr(), c_args.as_ptr());
            }
            let msg = std::io::Error::last_os_error();
            sess.app_log.record(
                LogLevel::Error,
                &format!("could not execute {}: {}", argv[0].to_string_lossy(), msg),
            );
            println!(
                "{}: could not execute {}: {}",
                sess.progname,
                argv[0].to_string_lossy(),
                msg
            );
            let _ = std::io::stdout().flush();
            unsafe {
                libc::_exit(127);
            }
        }
        Ok(Pty {
            fd: master,
            pid: ret,
            ws: unsafe { std::mem::zeroed() },
        })
    }

    /// Read the terminal window size via `TIOCGWINSZ`.
    ///
    /// Returns a zeroed `winsize` on error.
    #[allow(dead_code)]
    pub(crate) fn get_winsize(&self) -> libc::winsize {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        unsafe {
            libc::ioctl(
                self.fd,
                libc::TIOCGWINSZ,
                &mut ws as *mut _ as *mut libc::c_void,
            );
        }
        ws
    }

    /// Set the terminal window size via `TIOCSWINSZ`.
    pub(crate) fn set_winsize(&self, ws: &libc::winsize) {
        unsafe {
            libc::ioctl(
                self.fd,
                libc::TIOCSWINSZ,
                ws as *const _ as *mut libc::c_void,
            );
        }
    }

    /// Send signal `sig` to the child process group.
    pub(crate) fn kill(&self, sig: i32) {
        unsafe {
            libc::kill(-self.pid, sig);
        }
    }
}
