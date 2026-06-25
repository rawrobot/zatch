//! Shared utility functions used across modules.
//!
//! Extracted from `main.rs` to avoid code duplication between `main.rs`,
//! `attach.rs`, and `master.rs`. Contains path helpers, string formatting
//! for sessions, terminal save/restore, size parsing, and CString conversion.

use std::ffi::CString;
use std::io::{Read, Seek};
use std::path::PathBuf;
use std::process;

/// Convert a `&str` to a `CString`.
///
/// Prints an error and exits with status 1 if the string contains interior
/// null bytes.
pub fn cstr(s: &str) -> CString {
    match CString::new(s) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("fatal: string contains null byte");
            process::exit(1);
        }
    }
}

/// Extract the session name from a socket path.
///
/// Returns the last path component (after the final `/`), or the whole
/// string if there is no `/`.
pub fn session_shortname(sockname: &str) -> &str {
    sockname.rsplit('/').next().unwrap_or(sockname)
}

/// Build the session environment variable name from the program name.
///
/// Replaces non-alphanumeric characters with `_`, uppercases letters, and
/// appends `_SESSION`.  E.g. `"ztch"` → `"ZTCH_SESSION"`,
/// `"my-term"` → `"MY_TERM_SESSION"`.
pub fn session_envvar_name(progname: &str) -> String {
    let base = progname.rsplit('/').next().unwrap_or(progname);
    let mut name = String::with_capacity(base.len() + 8);
    for c in base.chars() {
        if c.is_ascii_alphanumeric() {
            for u in c.to_uppercase() {
                name.push(u);
            }
        } else {
            name.push('_');
        }
    }
    name.push_str("_SESSION");
    name
}

/// Return the session directory for a given program name.
///
/// Uses `$HOME/.cache/<progname>` when `HOME` is set and non-empty (and
/// not `/`).  Falls back to `/tmp/.<progname>-<pid>` otherwise.  Creates the
/// directory if it does not exist.
// TODO: return Result<PathBuf, io::Error> so callers can handle dir-creation failures properly
pub fn get_session_dir(progname: &str) -> PathBuf {
    let base = progname.rsplit('/').next().unwrap_or(progname);
    match std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty() && h != "/")
    {
        Some(h) => {
            let p = PathBuf::from(h).join(".cache").join(base);
            if let Err(e) = std::fs::create_dir_all(&p) {
                eprintln!(
                    "{}: could not create session directory '{}': {}",
                    base,
                    p.display(),
                    e
                );
            }
            p
        }
        _ => {
            let p = PathBuf::from(format!("/tmp/.{}-{}", base, process::id()));
            if let Err(e) = std::fs::create_dir_all(&p) {
                eprintln!(
                    "{}: could not create session directory '{}': {}",
                    base,
                    p.display(),
                    e
                );
            }
            p
        }
    }
}

/// If `name` contains a `/`, return it as-is.  Otherwise join
/// [`get_session_dir`] with `name` (creating the directory if necessary).
// TODO: return Result<String, io::Error> so callers can handle dir-creation failures properly
pub fn expand_sockname(progname: &str, name: &str) -> String {
    if name.contains('/') {
        return name.to_string();
    }
    let dir = get_session_dir(progname);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        let base = progname.rsplit('/').next().unwrap_or(progname);
        eprintln!(
            "{}: could not create session directory '{}': {}",
            base,
            dir.display(),
            e
        );
    }
    dir.join(name).to_string_lossy().to_string()
}

/// Save the current terminal attributes for fd 0 (stdin).
///
/// Returns a tuple of `(termios, failed)` where `failed` is true when
/// `tcgetattr` returned an error (i.e. stdin is not a terminal).
pub fn save_term() -> (libc::termios, bool) {
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    // tcgetattr must run before term is copied into the tuple — tuple fields
    // evaluate left-to-right, so capturing term first yields an all-zeros copy.
    let no_tty = unsafe { libc::tcgetattr(0, &mut term) } < 0;
    (term, no_tty)
}

/// Parse a human-readable size string into bytes.
///
/// Supports optional `k`/`K` (kibibytes) and `m`/`M` (mebibytes) suffixes.
/// Returns `None` when the numeric part cannot be parsed.  Examples:
/// `"128k"` → `Some(131072)`, `"4m"` → `Some(4194304)`, `"1024"` →
/// `Some(1024)`, `""` → `None`.
pub fn parse_size(s: &str) -> Option<usize> {
    let s = s.trim();
    let (num, mul) = if let Some(r) = s.strip_suffix(|c: char| c == 'k' || c == 'K') {
        (r, 1024)
    } else if let Some(r) = s.strip_suffix(|c: char| c == 'm' || c == 'M') {
        (r, 1024 * 1024)
    } else {
        (s, 1)
    };
    num.parse::<usize>().ok().map(|v| v * mul)
}

/// Parse a detach character specification.
///
/// Supports `^<char>` (control character, e.g. `^\` → `0x1c`, `^?` →
/// `0x7f`).  Falls back to the byte value of the first character, or 0
/// on empty input.
pub fn parse_escape_char(s: &str) -> i32 {
    if s.len() >= 2 && s.as_bytes()[0] == b'^' {
        if s.as_bytes()[1] == b'?' {
            return 0x7f;
        }
        return (s.as_bytes()[1] & 0x1f) as i32;
    }
    s.as_bytes().first().copied().unwrap_or(0) as i32
}

/// Find the byte offset in a file that is `nlines` newlines from the end.
///
/// Used by the `tail` command to locate where to start reading.  Scans
/// backwards from `size` bytes, reading `BUFSIZE` chunks.  Returns the
/// offset just after the `nlines`-th newline (or 0 when there are fewer
/// than `nlines` lines).
pub fn find_tail_start(file: &mut (impl Read + Seek), size: u64, nlines: i32) -> u64 {
    let mut buf = [0u8; crate::protocol::BUFSIZE];
    let mut count = 0i32;
    let mut pos = size as i64;
    while pos > 0 && count <= nlines {
        let chunk = if pos > crate::protocol::BUFSIZE as i64 {
            crate::protocol::BUFSIZE as i64
        } else {
            pos
        };
        pos -= chunk;
        let _ = file.seek(std::io::SeekFrom::Start(pos as u64));
        let n = file.read(&mut buf[..chunk as usize]).unwrap_or(0);
        if n == 0 {
            break;
        }
        for i in (0..n).rev() {
            if buf[i] == b'\n' {
                count += 1;
                if count > nlines {
                    return (pos + i as i64 + 1) as u64;
                }
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cstr_normal() {
        let c = cstr("hello");
        assert_eq!(c.to_bytes(), b"hello");
    }

    #[test]
    fn cstr_empty() {
        let c = cstr("");
        assert_eq!(c.to_bytes(), b"");
    }

    #[test]
    fn shortname_basename() {
        assert_eq!(session_shortname("/a/b/mysession"), "mysession");
    }

    #[test]
    fn shortname_no_slash() {
        assert_eq!(session_shortname("mysession"), "mysession");
    }

    #[test]
    fn shortname_trailing_slash() {
        assert_eq!(session_shortname("dir/"), "");
    }

    #[test]
    fn shortname_root() {
        assert_eq!(session_shortname("/"), "");
    }

    #[test]
    fn envvar_name_simple() {
        assert_eq!(session_envvar_name("ztch"), "ZTCH_SESSION");
    }

    #[test]
    fn envvar_name_with_hyphen() {
        assert_eq!(session_envvar_name("my-term"), "MY_TERM_SESSION");
    }

    #[test]
    fn envvar_name_with_path() {
        assert_eq!(session_envvar_name("/usr/local/bin/ztch"), "ZTCH_SESSION");
    }

    #[test]
    fn envvar_name_numbers() {
        assert_eq!(session_envvar_name("ztch2"), "ZTCH2_SESSION");
    }

    #[test]
    fn parse_size_plain_number() {
        assert_eq!(parse_size("1024"), Some(1024));
    }

    #[test]
    fn parse_size_kibibyte() {
        assert_eq!(parse_size("1k"), Some(1024));
        assert_eq!(parse_size("128K"), Some(131072));
    }

    #[test]
    fn parse_size_mebibyte() {
        assert_eq!(parse_size("1m"), Some(1048576));
        assert_eq!(parse_size("4M"), Some(4194304));
    }

    #[test]
    fn parse_size_trimmed() {
        assert_eq!(parse_size("  2k  "), Some(2048));
    }

    #[test]
    fn parse_size_empty() {
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn parse_size_bad_number() {
        assert_eq!(parse_size("abc"), None);
    }

    #[test]
    fn parse_escape_char_caret_backslash() {
        assert_eq!(parse_escape_char("^\\"), 0x1c);
    }

    #[test]
    fn parse_escape_char_caret_question() {
        assert_eq!(parse_escape_char("^?"), 0x7f);
    }

    #[test]
    fn parse_escape_char_caret_c() {
        assert_eq!(parse_escape_char("^C"), 3);
    }

    #[test]
    fn parse_escape_char_literal() {
        assert_eq!(parse_escape_char("x"), b'x' as i32);
    }

    #[test]
    fn parse_escape_char_empty() {
        assert_eq!(parse_escape_char(""), 0);
    }

    #[test]
    fn find_tail_start_empty_file() {
        let mut file = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(find_tail_start(&mut file, 0, 10), 0);
    }

    #[test]
    fn find_tail_start_fewer_lines() {
        let tmpdir = std::env::temp_dir();
        let path = tmpdir.join("tail_test_fewer.txt");
        std::fs::write(&path, b"line1\n").unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        assert_eq!(find_tail_start(&mut file, 6, 10), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn find_tail_start_exact_lines() {
        let tmpdir = std::env::temp_dir();
        let path = tmpdir.join("tail_test_exact.txt");
        std::fs::write(&path, b"a\nb\nc\n").unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        assert_eq!(find_tail_start(&mut file, 6, 2), 2); // start of "b\nc\n"
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn get_session_dir_uses_home() {
        let orig = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", "/tmp/test_session_home");
        };
        let d = get_session_dir("testprog");
        assert!(
            d.to_string_lossy()
                .contains("/tmp/test_session_home/.cache/testprog")
        );

        unsafe {
            if let Some(h) = orig {
                std::env::set_var("HOME", h);
            } else {
                std::env::remove_var("HOME");
            }
        };
    }

    #[test]
    fn expand_sockname_absolute_passthrough() {
        assert_eq!(expand_sockname("ztch", "/absolute/path"), "/absolute/path");
    }
}
