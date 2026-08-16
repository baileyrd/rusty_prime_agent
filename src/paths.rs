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
//!       worker-fence.json  supervisor generation fence + worker token
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

/// Where the supervisor's own stderr is redirected to (`client::
/// daemon_start`'s spawn -- a detached process has nothing else to send
/// it to). An ordinary file path, not a socket, so it carries none of
/// `worker_socket_path`'s length constraint. A plain crash/panic is the
/// only thing this project's own recovery paths can't already explain
/// from `session list`/`daemon status` alone, so this exists purely as a
/// "why won't it come up" diagnostic -- `tests/common::daemon_start`
/// reads it back on a failed startup.
pub fn daemon_log_path(state: &std::path::Path) -> PathBuf {
    state.join("daemon.log")
}

/// The worker counterpart of [`daemon_log_path`] -- nested under the
/// readable `session_dir`, same as `transcript_path`/`state_file_path`.
pub fn worker_log_path(session_dir: &std::path::Path) -> PathBuf {
    session_dir.join("worker.log")
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

/// The worker's generation fence (`crate::fence::WorkerFence`) -- which
/// supervisor process is currently authorized to command this session's
/// worker, plus the per-worker token required to change that answer.
///
/// Nested under the readable `session_dir` alongside `state.json` rather
/// than flattened the way `worker_socket_path` has to be: this is an
/// ordinary file, so it carries none of AF_UNIX's `sun_path` length
/// limit. Written owner-only (`fence::WorkerFence::write`), matching
/// `daemon.md`'s own "worker descriptors, auth tokens ... are written
/// with owner-only permissions".
pub fn worker_fence_path(session_dir: &std::path::Path) -> PathBuf {
    session_dir.join("worker-fence.json")
}

/// Parity with `prime-agent schedule` -- see `crate::schedule`'s own
/// module doc comment. A JSON array of `protocol::ScheduleEntry`,
/// separate from `state_file_path`'s pointer file since schedules churn
/// on their own cadence (added/canceled/fired) independent of ordinary
/// session activity.
pub fn schedules_path(session_dir: &std::path::Path) -> PathBuf {
    session_dir.join("schedules.json")
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
///
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

/// Where `rp_server::ensure_running` records the sidecar's chosen port
/// and pid, so a worker process (started separately from the supervisor
/// that spawned the sidecar) can find it without a lookup service of its
/// own -- same "small JSON file, read by whichever process needs it"
/// shape as `state_file_path`.
pub fn provider_state_path(state: &std::path::Path) -> PathBuf {
    state.join("provider.json")
}

/// The generated `config.toml` `rp_server::ensure_running` writes and
/// launches `rp-server` against -- see that function's own doc comment.
pub fn provider_config_path(state: &std::path::Path) -> PathBuf {
    state.join("provider-config.toml")
}

/// `rp-server`'s own stderr, same reasoning as `daemon_log_path`.
pub fn provider_log_path(state: &std::path::Path) -> PathBuf {
    state.join("provider.log")
}

/// Global tier of prompt-template discovery (`prompt_template::
/// discover`) -- parity with `prime-agent`'s own `~/.prime/agent/
/// prompts/*.md`, but nested under this project's own `state_dir()`
/// rather than inventing a fourth "where does user data go" directory
/// concept just for this (this project's own "no extra crate for that"
/// stance, see this module's own doc comment).
pub fn global_prompts_dir(state: &std::path::Path) -> PathBuf {
    state.join("prompts")
}

/// Project tier of prompt-template discovery -- parity with
/// `prime-agent`'s own project-local `.prime/agent/prompts/*.md`, under
/// this project's own name instead. Resolved against the current
/// working directory, not `state_dir()` -- it's meant to live alongside
/// a project's own source tree, checked in like any other project file.
pub fn project_prompts_dir(cwd: &std::path::Path) -> PathBuf {
    cwd.join(".rusty-prime-agent").join("prompts")
}

/// Where `skills::discover` looks for installed skills -- see that
/// module's own doc comment for why this is global-only (no
/// project-local tier, unlike `project_prompts_dir`): skill loading runs
/// inside the worker process, which has no access to the CLI caller's
/// own cwd the way `prompt_template::discover`'s always-client-side
/// callers do.
pub fn global_skills_dir(state: &std::path::Path) -> PathBuf {
    state.join("skills")
}

/// `<state_dir>/extensions/` -- parity with a bounded slice of
/// `prime-agent`'s own extension system, see `extensions.rs`'s own
/// module doc comment. Same global-only reasoning as
/// [`global_skills_dir`] just above: extension loading runs inside the
/// worker process, which has no access to the CLI caller's own cwd.
pub fn global_extensions_dir(state: &std::path::Path) -> PathBuf {
    state.join("extensions")
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
