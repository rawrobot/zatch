use clap::{Parser, Subcommand};
use std::io::{Read, Seek};
use std::process;

mod client;
mod protocol;
use protocol::*;

mod app_log;
use app_log::{AppLog, LogLevel};
mod pty;
mod scrollback;
mod session_log;
mod sock;

mod session;
pub use crate::session::Session;

mod session_daemon;

mod util;
use util::*;
#[derive(Parser, Clone)]
#[command(name = "ztch", version = "dev", about = "Terminal session manager")]
pub struct GlobalArgs {
    #[arg(
        short = 'e',
        value_name = "CHAR",
        global = true,
        help = "Set detach character (default: ^\\)"
    )]
    pub escape: Option<String>,
    #[arg(short = 'E', global = true, help = "Disable detach character")]
    pub disable_escape: bool,
    #[arg(short = 'z', global = true, help = "Disable suspend key")]
    pub no_suspend: bool,
    #[arg(short = 'q', global = true, help = "Suppress messages")]
    pub quiet: bool,
    #[arg(short = 't', global = true, help = "Disable VT100 assumptions")]
    pub no_ansiterm: bool,
    #[arg(
        short = 'r',
        value_name = "METHOD",
        global = true,
        help = "Redraw method: none | ctrl_l | winch"
    )]
    pub redraw: Option<String>,
    #[arg(
        short = 'R',
        value_name = "METHOD",
        global = true,
        help = "Clear method: none | move"
    )]
    pub clear: Option<String>,
    #[arg(
        short = 'C',
        value_name = "SIZE",
        global = true,
        help = "Log cap: 0=disable, e.g. 128k, 4m (default 1m)"
    )]
    pub log_cap: Option<String>,
}

#[derive(Subcommand)]
pub enum Commands {
    #[command(aliases = &["l", "ls"], about = "List sessions")]
    List {
        #[arg(short = 'a', help = "Include exited sessions")]
        all: bool,
    },
    #[command(aliases = &["c"], about = "Print current session name")]
    Current,
    #[command(aliases = &["a", "at"], about = "Strict attach (fail if session missing)")]
    Attach {
        #[arg(help = "Session name")]
        session: String,
    },
    #[command(aliases = &["n", "create"], about = "Create session and attach")]
    New {
        #[arg(help = "Session name")]
        session: String,
        #[arg(trailing_var_arg = true, help = "Command to run")]
        command: Vec<String>,
    },
    #[command(aliases = &["s"], about = "Create session, detached")]
    Start {
        #[arg(help = "Session name")]
        session: String,
        #[arg(trailing_var_arg = true, help = "Command to run")]
        command: Vec<String>,
    },
    #[command(about = "Create session, master in foreground")]
    Run {
        #[arg(help = "Session name")]
        session: String,
        #[arg(trailing_var_arg = true, help = "Command to run")]
        command: Vec<String>,
    },
    #[command(aliases = &["p"], about = "Pipe stdin into session")]
    Push {
        #[arg(help = "Session name")]
        session: String,
    },
    #[command(aliases = &["i"], about = "Show attached clients for a session")]
    Info {
        #[arg(help = "Session name")]
        session: String,
    },
    #[command(aliases = &["d"], about = "Detach clients from session without stopping it")]
    Detach {
        #[arg(short = 'a', help = "Detach all active sessions")]
        all: bool,
        #[arg(help = "Session name (default: current session)")]
        session: Option<String>,
    },
    #[command(aliases = &["k"], about = "Stop session (SIGTERM then SIGKILL)")]
    Kill {
        #[arg(help = "Session name (default: current session)")]
        session: Option<String>,
        #[arg(
            short = 'f',
            long = "force",
            help = "Skip grace period, send SIGKILL immediately"
        )]
        force: bool,
    },
    #[command(about = "Truncate the session log")]
    Clear {
        #[arg(help = "Session name (default: current)")]
        session: Option<String>,
    },
    #[command(about = "Print last N lines of session log")]
    Tail {
        #[arg(help = "Session name")]
        session: String,
        #[arg(short = 'f', help = "Follow (tail -f)")]
        follow: bool,
        #[arg(short = 'n', default_value_t = 10, help = "Number of lines")]
        lines: i32,
    },
    #[command(name = "rm", about = "Remove stale/exited session(s)")]
    Rm {
        #[arg(short = 'a', help = "Remove all stale and exited sessions")]
        all: bool,
        #[arg(help = "Session name")]
        session: Option<String>,
    },
}

#[derive(Parser)]
#[command(name = "ztch", version = "dev")]
struct Cli {
    #[command(flatten)]
    global: GlobalArgs,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Parser)]
#[command(about = "Attach to an existing session")]
struct ImplicitArgs {
    #[command(flatten)]
    global: GlobalArgs,
    #[arg(help = "Session name")]
    session: String,
}

fn session_from_global(global: &GlobalArgs, progname: &str, session_envvar: &str) -> Session {
    Session {
        progname: progname.to_string(),
        app_log: AppLog::new(progname),
        session_envvar: session_envvar.to_string(),
        sockname: String::new(),
        detach_char: if global.disable_escape {
            -1
        } else {
            global
                .escape
                .as_ref()
                .map(|s| parse_escape_char(s))
                .unwrap_or(0x1c)
        },
        no_suspend: global.no_suspend,
        redraw_method: global
            .redraw
            .as_ref()
            .map_or(RedrawMethod::Unspec, |s| match s.as_str() {
                "none" => RedrawMethod::None,
                "ctrl_l" => RedrawMethod::CtrlL,
                "winch" => RedrawMethod::Winch,
                _ => RedrawMethod::Unspec,
            }),
        clear_method: global
            .clear
            .as_ref()
            .map_or(ClearMethod::Unspec, |s| match s.as_str() {
                "none" => ClearMethod::None,
                "move" => ClearMethod::Move,
                _ => ClearMethod::Unspec,
            }),
        no_ansiterm: global.no_ansiterm,
        quiet: global.quiet,
        log_max_size: global
            .log_cap
            .as_ref()
            .and_then(|s| parse_size(s))
            .unwrap_or(LOG_MAX_SIZE),
        orig_term: unsafe { std::mem::zeroed() },
        dont_have_tty: false,
    }
}

fn resolve_session_name(session: &Option<String>, envvar: &str) -> String {
    session.clone().unwrap_or_else(|| {
        std::env::var(envvar)
            .ok()
            .filter(|c| !c.is_empty())
            .map(|c| c.rsplit(':').next().unwrap_or(&c).to_string())
            .unwrap_or_default()
    })
}

fn log_println(log: &AppLog, level: LogLevel, args: std::fmt::Arguments<'_>) {
    let msg = args.to_string();
    log.record(level, &msg);
    println!("{}: {}", log.progname(), msg);
}

fn log_eprintln(log: &AppLog, level: LogLevel, args: std::fmt::Arguments<'_>) {
    let msg = args.to_string();
    log.record(level, &msg);
    eprintln!("{}: {}", log.progname(), msg);
}

fn cmd_push(sess: &mut Session, session: &str) -> i32 {
    sess.sockname = expand_sockname(&sess.progname, session);
    sess.push()
}

fn cmd_detach(sess: &mut Session, session: &Option<String>, all: bool) -> i32 {
    if all {
        return sess.cmd_detach_all();
    }
    let name = resolve_session_name(session, &sess.session_envvar);
    if name.is_empty() {
        log_eprintln(
            &sess.app_log,
            LogLevel::Error,
            format_args!(
                "no session specified and {} is not set",
                sess.session_envvar
            ),
        );
        return 1;
    }
    sess.sockname = if name.contains('/') {
        name
    } else {
        expand_sockname(&sess.progname, &name)
    };
    sess.cmd_detach()
}

fn cmd_kill(sess: &mut Session, session: &Option<String>, force: bool) -> i32 {
    let name = resolve_session_name(session, &sess.session_envvar);
    if name.is_empty() {
        log_eprintln(
            &sess.app_log,
            LogLevel::Error,
            format_args!(
                "no session specified and {} is not set",
                sess.session_envvar
            ),
        );
        return 1;
    }
    sess.sockname = if name.contains('/') {
        name
    } else {
        expand_sockname(&sess.progname, &name)
    };
    sess.kill(force)
}

fn cmd_clear(sess: &mut Session, session: &Option<String>) -> i32 {
    let name = resolve_session_name(session, &sess.session_envvar);
    sess.sockname = if name.contains('/') {
        name
    } else {
        expand_sockname(&sess.progname, &name)
    };
    let log_path = format!("{}.log", sess.sockname);
    match std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&log_path)
    {
        Ok(_) => {
            sess.app_log.record(
                LogLevel::Info,
                &format!(
                    "session '{}' log cleared",
                    session_shortname(&sess.sockname)
                ),
            );
            if !sess.quiet {
                println!(
                    "{}: session '{}' log cleared",
                    sess.progname,
                    session_shortname(&sess.sockname)
                );
            }
            0
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => {
            log_eprintln(
                &sess.app_log,
                LogLevel::Error,
                format_args!("{}: {}", log_path, e),
            );
            1
        }
    }
}

fn cmd_tail(sess: &Session, session: &str, follow: bool, lines: i32) -> i32 {
    let sock = expand_sockname(&sess.progname, session);
    let log_path = format!("{}.log", sock);
    let mut file = match std::fs::File::open(&log_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log_println(
                &sess.app_log,
                LogLevel::Error,
                format_args!("no log for session '{}'", session_shortname(&sock)),
            );
            return 1;
        }
        Err(e) => {
            log_eprintln(
                &sess.app_log,
                LogLevel::Error,
                format_args!("{}: {}", log_path, e),
            );
            return 1;
        }
    };
    let n = if lines < 1 { 1 } else { lines };
    let size = file.seek(std::io::SeekFrom::End(0)).unwrap_or(0);
    if size > 0 {
        let start = find_tail_start(&mut file, size, n);
        let _ = file.seek(std::io::SeekFrom::Start(start));
        let mut rbuf = [0u8; BUFSIZE];
        loop {
            let nread = file.read(&mut rbuf).unwrap_or(0);
            if nread == 0 {
                break;
            }
            write_buf(1, &rbuf[..nread]);
        }
    }
    if follow {
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }

        let ifd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        let use_inotify = if ifd >= 0 {
            match std::ffi::CString::new(log_path.as_str()) {
                Ok(cpath) => {
                    let wd =
                        unsafe { libc::inotify_add_watch(ifd, cpath.as_ptr(), libc::IN_MODIFY) };
                    if wd >= 0 {
                        true
                    } else {
                        unsafe {
                            libc::close(ifd);
                        }
                        false
                    }
                }
                Err(_) => {
                    unsafe {
                        libc::close(ifd);
                    }
                    false
                }
            }
        } else {
            false
        };

        let mut rbuf = [0u8; BUFSIZE];
        loop {
            loop {
                let nread = file.read(&mut rbuf).unwrap_or(0);
                if nread == 0 {
                    break;
                }
                write_buf(1, &rbuf[..nread]);
            }

            if use_inotify {
                let mut rfds: libc::fd_set = unsafe { std::mem::zeroed() };
                unsafe {
                    libc::FD_ZERO(&mut rfds);
                    libc::FD_SET(ifd, &mut rfds);
                }
                let ret = unsafe {
                    libc::select(
                        ifd + 1,
                        &mut rfds,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                if ret > 0 {
                    let mut evbuf = [0u8; 64];
                    let _ = unsafe { libc::read(ifd, evbuf.as_mut_ptr() as *mut libc::c_void, 64) };
                }
            } else {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
    }
    0
}

fn cmd_rm(sess: &mut Session, session: &Option<String>, all: bool) -> i32 {
    if all {
        // -a always means "remove every stale / exited session".  A positional
        // session name alongside -a is ambiguous and almost certainly a mistake,
        // so warn rather than silently ignoring one of the two arguments.
        if session.is_some() {
            log_eprintln(
                &sess.app_log,
                LogLevel::Warn,
                format_args!(
                    "warning: session name is ignored when -a is used; \
                     removing all stale sessions"
                ),
            );
        }
        return sess.rm(true);
    }
    match session {
        Some(s) => {
            sess.sockname = expand_sockname(&sess.progname, s);
            sess.rm(false)
        }
        None => sess.rm(true),
    }
}

fn main() {
    let raw: Vec<String> = std::env::args().collect();
    let progname = raw[0].clone();
    let app_log = AppLog::new(&progname);
    let session_envvar = session_envvar_name(&progname);

    let make_sess = |global: &GlobalArgs| -> Session {
        session_from_global(global, &progname, &session_envvar)
    };

    if raw.len() > 1 && (raw[1] == "--version") {
        app_log.record(LogLevel::Info, "version requested");
        println!("{} - version dev", progname);
        process::exit(0);
    }
    if raw.len() > 1 && raw[1] == "?" {
        if let Err(e) = Cli::try_parse_from(["", "--help"]) {
            e.exit();
        }
        process::exit(0);
    }

    match Cli::try_parse() {
        Ok(cli) => {
            let mut sess = make_sess(&cli.global);
            let exit_code = match cli.command {
                Commands::List { all } => sess.list(all),
                Commands::Current => sess.cmd_current(),
                Commands::Attach { ref session } => sess.cmd_attach(session),
                Commands::New {
                    ref session,
                    ref command,
                } => sess.cmd_new(session, command),
                Commands::Start {
                    ref session,
                    ref command,
                } => sess.cmd_start(session, command),
                Commands::Run {
                    ref session,
                    ref command,
                } => sess.cmd_run(session, command),
                Commands::Push { ref session } => cmd_push(&mut sess, session),
                Commands::Info { ref session } => {
                    sess.sockname = expand_sockname(&sess.progname, session);
                    sess.cmd_info()
                }
                Commands::Detach { ref session, all } => cmd_detach(&mut sess, session, all),
                Commands::Kill { ref session, force } => cmd_kill(&mut sess, session, force),
                Commands::Clear { ref session } => cmd_clear(&mut sess, session),
                Commands::Tail {
                    ref session,
                    follow,
                    lines,
                } => cmd_tail(&sess, session, follow, lines),
                Commands::Rm { ref session, all } => cmd_rm(&mut sess, session, all),
            };
            process::exit(exit_code);
        }
        Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelp => e.exit(),
        _ => {}
    }

    if let Ok(imp) = ImplicitArgs::try_parse() {
        let mut sess = make_sess(&imp.global);
        process::exit(sess.cmd_attach(&imp.session));
    }

    app_log.record(LogLevel::Error, "invalid command line; usage suggested");
    eprintln!("Try '{} --help' for usage", progname);
    process::exit(1);
}
