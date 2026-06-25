@RTK.md

# ztch — Agent Briefing

ztch is a lightweight terminal session manager for Linux — a Rust port of dtach. It runs a command in a background PTY session, detach/reattach from any terminal, supports multiple simultaneous clients, scrollback replay, rolling logs, and tail viewing.

## Project State

- **Language**: Rust 2021 edition
- **Deps**: `libc` (raw POSIX), `clap` (derive API)
- **Lines**: 3798 across 10 modules
- **Tests**: 58 unit tests + 13 CLI smoke tests (`bash tests/cli_tests.sh`)
- **Build**: `cargo build --release` produces a stripped, LTO-optimized binary

## Architecture

| File | Lines | Role |
|------|-------|------|
| `main.rs` | 408 | Thin dispatch (44 lines of `main()`), `GlobalArgs`, `Commands` enum, `ImplicitArgs`, remaining free functions: `cmd_push`, `cmd_kill`, `cmd_clear`, `cmd_tail`, `cmd_rm`, `resolve_session_name`, `session_from_global` |
| `session.rs` | 1098 | `Session` struct + all user-facing command methods (`attach`, `push`, `kill`, `list`, `rm`, `cmd_current`, `cmd_attach`, `cmd_new`, `cmd_start`, `cmd_run`, `implicit_attach`), event loop, terminal raw-mode helpers, `connect_socket`, private helpers (`require_tty`, `with_tty`) |
| `master.rs` | 888 | Daemon loop (`master_main`): `select()` on PTY + client sockets, packet dispatch, session cleanup, signals |
| `client.rs` | 12 | `Client` struct (fd, scrollback replay state) |
| `pty.rs` | 92 | `Pty` struct: `forkpty`, winsize, kill |
| `scrollback.rs` | 246 | 128 KB ring buffer, `ReplayState` drain |
| `protocol.rs` | 375 | `Packet` (10-byte binary), `MsgType` enum, `write_buf` |
| `log.rs` | 210 | Rolling file log with size cap |
| `sock.rs` | 138 | Unix domain socket bind/connect/listen |
| `util.rs` | 331 | `cstr()`, `expand_sockname`, `save_term`, `session_shortname`, `find_tail_start`, `format_age`, `parse_size`, `parse_escape_char`, etc. |

## Completed Refactoring

### Session methods (free functions → `impl Session`)
All CLI commands that take `&Session` or `&mut Session` as first arg are now methods on `Session`:
- `cmd_current`, `cmd_attach`, `cmd_new`, `cmd_start`, `cmd_run`, `implicit_attach`
- Private helpers `require_tty`, `with_tty`, `use_shell_if_no_cmd` also live in `session.rs`
- `main()` dispatches as `sess.cmd_attach(session)` etc.

### Zero unsafe-errno pattern
All `unsafe { *libc::__errno_location() }` replaced (8 occurrences across 5 files):
- `libc::open()` errors → `std::io::Error::last_os_error()` + `ErrorKind::NotFound`
- `libc::read/write` EINTR/EAGAIN → `ErrorKind::Interrupted` / `WouldBlock`
- `execvp` failure → `std::io::Error::last_os_error()` after `unsafe` block
- `attach()` return value → used directly (it IS the errno from `connect_socket`)
- `read_from`/`write_to`/`write_buf` → `last_os_error()` + `raw_os_error().unwrap_or(libc::EIO)`

### Zero CStr references
All `CStr::from_ptr(libc::strerror(...))` → `std::io::Error::from_raw_os_error` / `last_os_error`.

### Terminal safety
Global `TERM_BACKUP`, `restore_term()` atexit handler, panic hook, signal handlers restore original termios on crash/detach.

### PTY & daemon
- `Pty` struct with methods (`new`, `get_winsize`, `set_winsize`, `kill`) — no free functions.
- PTY output read even when no clients attached.
- `send_to_clients` uses plain `for` loop (close bad fds, reaped by `handle_client_packets`).

### Eliminated libc::open/lseek/read/close from cmd_clear / cmd_tail
- `cmd_clear` → `std::fs::OpenOptions::new().write(true).truncate(true).open()`
- `cmd_tail` → `std::fs::File::open()` + `Read`/`Seek` trait methods
- `find_tail_start` signature: `(RawFd, i64, i32) -> i64` → `(&mut (impl Read + Seek), u64, i32) -> u64`

### `master.rs` kept under 800 lines
Large types extracted: `client.rs` (Client), `pty.rs` (Pty), `scrollback.rs` (Scrollback + ReplayState). master.rs is 781 lines.

## Design Constraints

1. Match C original (`atch-main`) terminal and PTY settings exactly
2. `main()` must be thin dispatch (<50 lines), command logic in named functions/methods
3. `master.rs` should stay under ~800 lines
4. Avoid `std::ffi::CStr` unless calling C directly
5. Prefer `std::io::Error` over `libc::strerror` / `__errno_location`
6. `./ztch c` shows current session name from `$ZTCH_SESSION`
7. `./ztch kill` with no arg kills the current session (must complete <500ms)
8. `deploy.sh` must not kill interactive attach clients

## Key Conventions

- **Imports**: explicit local imports, no glob `use` except `use protocol::*` in main.rs
- **Error handling**: `std::io::Error::last_os_error()` for syscall errors, `std::io::Error::from_raw_os_error(err)` when errno is captured as a value
- **Tests**: doc comments on every `pub fn`, unit tests in each module's `#[cfg(test)] mod tests`
- **PTY**: `forkpty` passes NULL termios/winsize — system defaults match C
- **Terminal raw mode**: `c_oflag |= OPOST` (preserves flags, no `ONLCR`) — matches C
- **Kill escalation**: SIGHUP (100ms) then SIGKILL (300ms). Bash ignores SIGHUP in `waitpid`.

## Files to Review

Priority order for a code review:
1. `session.rs` — Largest module, contains most of the client-side logic. Check method cohesion and doc quality.
2. `master.rs` — Daemon loop, signal handling, multi-client select loop.
3. `main.rs` — CLI dispatch, remaining free functions. Check if anything else should be a Session method or extracted.
4. `protocol.rs` — Binary packet read/write, `write_buf`.
5. `pty.rs` — Forkpty abstraction, winsize.
6. `scrollback.rs` — Ring buffer with 8 tests.
7. `util.rs` — Shared utilities.
8. `sock.rs` — Unix socket wrappers.
9. `log.rs` — Rolling file log.
10. `client.rs` — Small struct, likely fine.

## What to Look For

- Any remaining `libc::` calls that could be replaced with std equivalents
- `master.rs` module size — nearing 800-line limit
- Error handling gaps (unwrapped `Result`s, unhandled edge cases)
- Thread-safety and signal-safety (`AtomicBool` usage, async-signal-safe calls in handlers)
- Doc completeness — every `pub fn` should have a doc comment
- Test coverage — high-risk areas like `implicit_attach`, `event_loop`, `master_main`
