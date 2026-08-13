//! Raw-mode terminal control -- the foundation `session_repl`'s future
//! rich-editor/live-rendering surface (multiline input, `@` fuzzy
//! search, tab completion, image paste, steering -- see `PARITY.md`'s
//! "Needs a new subsystem" section) builds on.
//!
//! Hand-rolled direct OS primitives via the `libc`/`windows-sys` FFI
//! this project already depends on (`procutil.rs`'s own precedent: a
//! handful of small, direct syscalls, not a protocol or a parser), not
//! a new terminal-UI dependency -- the same "hand-roll a narrowly
//! scoped OS/protocol concern, don't hand-roll everything" reasoning
//! that chose to hand-roll SHA-256/HMAC/a ZMTP client for RLM (see
//! `PARITY.md`'s "5a"/"5b" entries) while still using `serde_json`
//! rather than a hand-rolled JSON parser. Raw-mode terminal control is
//! squarely the former: a handful of `termios`/console-mode syscalls,
//! not a UI framework.
//!
//! Deliberately does not include terminal-size querying
//! (`TIOCGWINSZ`/`GetConsoleScreenBufferInfo`) -- nothing in this first
//! increment needs it (no line-wrapping, no multi-line rendering yet);
//! it can be added when a later increment (the rich editor, Increment 2
//! of the TUI arc) actually needs to know the terminal's width.

use std::io;

/// True only when both stdin and stdout are connected to a real
/// interactive terminal. `session_repl` uses this to decide whether to
/// engage raw-mode input at all -- every one of this project's own
/// tests pipes stdin/stdout (`Stdio::piped()`, see `tests/repl.rs`'s own
/// `run_repl` helper), so this reports `false` under test and the
/// existing blocking-line-read behavior stays exactly what it always
/// was: raw mode is additive for a real interactive caller, never a
/// requirement a non-interactive one has to satisfy.
pub fn is_tty() -> bool {
    stdin_is_tty() && stdout_is_tty()
}

#[cfg(unix)]
fn stdin_is_tty() -> bool {
    // SAFETY: `isatty` takes a valid fd and returns a plain `c_int`;
    // `STDIN_FILENO` is always a valid (if possibly closed/non-tty) fd
    // number for this process.
    unsafe { libc::isatty(libc::STDIN_FILENO) != 0 }
}

#[cfg(unix)]
fn stdout_is_tty() -> bool {
    // SAFETY: same as `stdin_is_tty`, for `STDOUT_FILENO`.
    unsafe { libc::isatty(libc::STDOUT_FILENO) != 0 }
}

#[cfg(windows)]
fn stdin_is_tty() -> bool {
    console_mode(windows_sys::Win32::System::Console::STD_INPUT_HANDLE).is_some()
}

#[cfg(windows)]
fn stdout_is_tty() -> bool {
    console_mode(windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE).is_some()
}

/// `GetConsoleMode` succeeding is the standard Windows test for "this
/// handle is a real console, not a pipe/file redirection" -- a
/// redirected/piped handle (exactly what this project's own tests use)
/// fails it.
#[cfg(windows)]
fn console_mode(which: windows_sys::Win32::System::Console::STD_HANDLE) -> Option<u32> {
    use windows_sys::Win32::System::Console::{GetConsoleMode, GetStdHandle};
    // SAFETY: `which` is one of the three documented standard-handle
    // identifiers; `GetStdHandle` returning an invalid/null handle is
    // itself a documented, checkable outcome (`GetConsoleMode` then just
    // fails, handled below), not undefined behavior.
    unsafe {
        let handle = GetStdHandle(which);
        let mut mode = 0u32;
        if GetConsoleMode(handle, &mut mode) != 0 {
            Some(mode)
        } else {
            None
        }
    }
}

/// RAII guard: puts the terminal into raw mode on [`enable`](Self::enable)
/// (no line buffering, no local echo, `Ctrl-C`/`Ctrl-\` delivered as
/// plain bytes rather than signals -- `session_repl`'s own raw-mode read
/// loop handles those explicitly instead), restores the original mode
/// on `Drop` -- including on an early return or panic unwind, so a
/// crashed or `?`-short-circuited REPL never leaves the user's shell
/// stuck in raw mode.
pub struct RawModeGuard {
    #[cfg(unix)]
    original: libc::termios,
    #[cfg(windows)]
    original_input_mode: u32,
}

/// The standard "raw mode" recipe (the same flag set every
/// `cfmakeraw`-equivalent implementation clears): no canonical
/// (line-buffered) input, no local echo (`session_repl`'s own raw-mode
/// read loop echoes each byte itself instead), no signal-generating
/// control characters, no implementation-defined input processing, no
/// start/stop flow control, no CR-to-NL translation, no parity checking
/// or 8th-bit stripping, no output post-processing. `VMIN=1`/`VTIME=0`:
/// a `read()` blocks for at least one byte and returns as soon as it has
/// one, no inter-byte timer. A pure function (no fd, no syscall) so it's
/// unit-testable in isolation from `enable`'s own real `tcgetattr`/
/// `tcsetattr` calls, which need a real terminal to succeed against.
#[cfg(unix)]
fn make_raw(original: libc::termios) -> libc::termios {
    let mut raw = original;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
    raw.c_iflag &= !(libc::IXON | libc::ICRNL | libc::BRKINT | libc::INPCK | libc::ISTRIP);
    raw.c_oflag &= !libc::OPOST;
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;
    raw
}

impl RawModeGuard {
    #[cfg(unix)]
    pub fn enable() -> io::Result<Self> {
        // SAFETY: `tcgetattr`/`tcsetattr` on a valid fd with a
        // fully-initialized `termios` (zeroed, then populated by
        // `tcgetattr` itself before any field is read) is the ordinary,
        // documented POSIX usage.
        unsafe {
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut original) != 0 {
                return Err(io::Error::last_os_error());
            }
            let raw = make_raw(original);
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(RawModeGuard { original })
        }
    }

    #[cfg(windows)]
    pub fn enable() -> io::Result<Self> {
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
            ENABLE_PROCESSED_INPUT, STD_INPUT_HANDLE,
        };
        // SAFETY: same standard-handle usage as `console_mode` above;
        // `SetConsoleMode`'s success/failure is checked, not assumed.
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            let mut original_input_mode = 0u32;
            if GetConsoleMode(handle, &mut original_input_mode) == 0 {
                return Err(io::Error::last_os_error());
            }
            // Windows Console API equivalent of the unix recipe above:
            // no line buffering, no local echo, no `Ctrl-C`-as-signal
            // processing (delivered as a plain byte instead, same as
            // `ISIG` cleared on unix).
            let raw_mode = original_input_mode
                & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT);
            if SetConsoleMode(handle, raw_mode) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(RawModeGuard {
                original_input_mode,
            })
        }
    }
}

impl Drop for RawModeGuard {
    #[cfg(unix)]
    fn drop(&mut self) {
        // Best-effort: a `Drop` impl can't meaningfully propagate an
        // error, and leaving the terminal in raw mode is worse than
        // silently failing to restore it here.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original);
        }
    }

    #[cfg(windows)]
    fn drop(&mut self) {
        use windows_sys::Win32::System::Console::{GetStdHandle, SetConsoleMode, STD_INPUT_HANDLE};
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            SetConsoleMode(handle, self.original_input_mode);
        }
    }
}

// `is_tty`/`RawModeGuard::enable` themselves aren't unit-tested directly
// -- their actual behavior depends on this test process's own real
// stdin/stdout, which `cargo test` doesn't give a consistent (or
// necessarily non-tty) answer for across environments, unlike this
// project's other environment-coupled real-thing tests (real Ollama, a
// real kernel), which are `#[ignore]`d instead of asserted on in CI. The
// deterministic, always-CI-safe part -- the raw-mode flag computation
// itself -- is tested directly below.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn all_flags_set() -> libc::termios {
        // SAFETY: every field of `termios` is a plain integer type (no
        // pointers, no validity invariant beyond "some bit pattern") --
        // an all-ones bit pattern is a valid (if not realistic) value
        // for every one of them.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            t.c_iflag = !0;
            t.c_oflag = !0;
            t.c_cflag = !0;
            t.c_lflag = !0;
            t
        }
    }

    #[test]
    fn make_raw_clears_canonical_echo_signal_and_extended_input_flags() {
        let raw = make_raw(all_flags_set());
        assert_eq!(raw.c_lflag & libc::ICANON, 0);
        assert_eq!(raw.c_lflag & libc::ECHO, 0);
        assert_eq!(raw.c_lflag & libc::ISIG, 0);
        assert_eq!(raw.c_lflag & libc::IEXTEN, 0);
    }

    #[test]
    fn make_raw_clears_flow_control_and_cr_to_nl_translation() {
        let raw = make_raw(all_flags_set());
        assert_eq!(raw.c_iflag & libc::IXON, 0);
        assert_eq!(raw.c_iflag & libc::ICRNL, 0);
        assert_eq!(raw.c_iflag & libc::BRKINT, 0);
        assert_eq!(raw.c_iflag & libc::INPCK, 0);
        assert_eq!(raw.c_iflag & libc::ISTRIP, 0);
    }

    #[test]
    fn make_raw_clears_output_post_processing() {
        let raw = make_raw(all_flags_set());
        assert_eq!(raw.c_oflag & libc::OPOST, 0);
    }

    #[test]
    fn make_raw_sets_vmin_one_and_vtime_zero() {
        let raw = make_raw(all_flags_set());
        assert_eq!(raw.c_cc[libc::VMIN], 1);
        assert_eq!(raw.c_cc[libc::VTIME], 0);
    }

    #[test]
    fn make_raw_does_not_touch_cflag() {
        // `c_cflag` (baud rate, parity, character size, ...) is
        // hardware/line-level config unrelated to input echo/buffering
        // -- raw mode has no reason to touch it, unlike `cfmakeraw`'s
        // own historical `CSIZE`/`PARENB`/`CS8` tweaks, which this
        // project's own recipe deliberately leaves alone (this project
        // isn't driving a real serial line).
        let original = all_flags_set();
        let raw = make_raw(original);
        assert_eq!(raw.c_cflag, original.c_cflag);
    }
}
