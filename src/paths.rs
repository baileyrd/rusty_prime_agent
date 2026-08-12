//! Cross-platform on-disk layout.
//!
//! ```text
//! <state_dir>/
//!   daemon.sock            public supervisor socket (rustils AF_UNIX)
//!   daemon.pid             supervisor pid + generation (recovery/status)
//!   sessions/
//!     <session-id>/
//!       transcript.jsonl   append-only event log (source of truth)
//!       state.json         small recovery-pointer snapshot
//!       worker.sock        private worker socket (supervisor <-> worker)
//! ```
//!
//! No extra "where does user data go" crate: the two env vars below cover
//! every target this project ships on, and the fallback is one join, not
//! worth a dependency for.

use std::path::PathBuf;

use crate::error::{Context, HarnessError, Result};

/// Root directory for all harness state. `RUSTY_PRIME_AGENT_HOME`
/// overrides everything (used by the test suite to isolate runs).
pub fn state_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("RUSTY_PRIME_AGENT_HOME") {
        return Ok(PathBuf::from(dir));
    }
    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA").ok_or_else(|| {
            HarnessError::io(
                Context::Daemon,
                None,
                std::io::Error::new(std::io::ErrorKind::NotFound, "%LOCALAPPDATA% is not set"),
            )
        })?;
        Ok(PathBuf::from(base).join("rusty-prime-agent"))
    }
    #[cfg(not(windows))]
    {
        if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
            return Ok(PathBuf::from(dir).join("rusty-prime-agent"));
        }
        let home = std::env::var_os("HOME").ok_or_else(|| {
            HarnessError::io(
                Context::Daemon,
                None,
                std::io::Error::new(std::io::ErrorKind::NotFound, "$HOME is not set"),
            )
        })?;
        Ok(PathBuf::from(home).join(".local/state/rusty-prime-agent"))
    }
}

pub fn daemon_socket_path(state: &std::path::Path) -> PathBuf {
    state.join("daemon.sock")
}

pub fn daemon_pid_path(state: &std::path::Path) -> PathBuf {
    state.join("daemon.pid")
}

pub fn sessions_dir(state: &std::path::Path) -> PathBuf {
    state.join("sessions")
}

pub fn session_dir(state: &std::path::Path, session_id: &str) -> PathBuf {
    sessions_dir(state).join(session_id)
}

pub fn transcript_path(session_dir: &std::path::Path) -> PathBuf {
    session_dir.join("transcript.jsonl")
}

pub fn state_file_path(session_dir: &std::path::Path) -> PathBuf {
    session_dir.join("state.json")
}

/// A short, flat path for the private worker socket -- deliberately
/// **not** nested under `session_dir` (`sessions/<id>/worker.sock`):
/// Windows AF_UNIX's `sun_path` has a hard 107-usable-byte cap (rustils'
/// own `platform-windows` doc: "`UNIX_PATH_CAP = 108`, 107 usable bytes
/// plus NUL"), and `<state_root>/sessions/<session-id>/worker.sock` blows
/// through that the moment `state_root` is a real per-user profile path
/// (`%LOCALAPPDATA%\...`) or, worse, a test's own long temp-directory
/// name -- caught by this project's own `tests/session_lifecycle.rs`
/// failing with `ErrorKind::InvalidInput` ("AF_UNIX path exceeds
/// sun_path's 107-byte usable capacity") before this function existed.
/// A flat `<state_root>/sock/<16-hex-char-hash-of-session-id>.sock`
/// stays short (~22 bytes past `state_root`) regardless of how long
/// `state_root` or the session id happens to be, and is still a pure,
/// deterministic function of `session_id` -- both the worker (binding)
/// and the supervisor (connecting) compute the identical path
/// independently, no lookup table needed. `transcript.jsonl`/
/// `state.json` stay under the readable, nested `session_dir` --
/// ordinary file paths have no such limit.
pub fn worker_socket_path(state_root: &std::path::Path, session_id: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    session_id.hash(&mut hasher);
    state_root
        .join("sock")
        .join(format!("{:016x}.sock", hasher.finish()))
}

/// Create `dir` (and parents) if missing.
pub fn ensure_dir(context: Context, dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| HarnessError::io(context, Some(dir.to_path_buf()), e))
}

/// Milliseconds since the Unix epoch, saturating rather than panicking on
/// a clock before 1970 (never expected, but this value only ever feeds
/// display/ordering, not safety, so a saturated `0` is a fine answer).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
