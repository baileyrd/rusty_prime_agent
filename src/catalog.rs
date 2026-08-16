//! Session catalog scan (Required Behavior: "`harness session list` --
//! catalog scan of active + saved sessions").
//!
//! The reference architecture runs this as a separate "catalog
//! subprocess" so a scan failure can't interrupt active workers. This
//! project runs it in-process inside the supervisor instead: Phase 1's
//! Architecture Constraints call for a modular monolith ("one binary,
//! internal module boundaries, not separate services, unless a concrete
//! forcing function shows up"). A plain directory scan over small JSON
//! files was once assumed cheap enough to run inline on the supervisor's
//! own async executor without cost -- see [`scan`]/[`read_session_state`]'s
//! own doc comments for why that assumption didn't hold and what changed.
//! A scan error here is caught and reported per-entry rather than
//! propagated, so one unreadable session
//! directory still can't take down `session list` for every other
//! session, which is the actual property the reference design is
//! protecting.

use std::path::Path;

use crate::error::{Context, HarnessError, Result};
use crate::paths;
use crate::procutil;
use crate::protocol::{SessionState, SessionStatus, SessionSummary};

/// The actual blocking read, shared by [`read_session_state`] (which
/// runs it via `spawn_blocking`) and [`scan_sync`]'s own per-entry loop
/// (already running inside `spawn_blocking` itself, so calling straight
/// through here costs nothing extra).
fn read_session_state_sync(context: Context, session_dir: &Path) -> Result<SessionState> {
    let path = paths::state_file_path(session_dir);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| HarnessError::io(context, Some(path.clone()), e))?;
    serde_json::from_str(&text).map_err(|e| HarnessError::json(context, Some(path), e))
}

/// Read `state.json` for one session directory. Shared with
/// `crate::session` (which writes/recovers it) and `crate::daemon`
/// (which needs the raw `worker_pid` to forward a request).
///
/// Runs the actual read on the blocking thread pool (`spawn_blocking`),
/// not on the caller's own async worker thread. Every caller here is a
/// live request handler or background-loop tick sharing this process's
/// small (`available_parallelism()`-sized) `rusty_tokio` executor -- and
/// a synchronous `std::fs` call blocks the OS thread it runs on for the
/// syscall's full duration; there is no cooperative preemption for that,
/// only for CPU-bound work between `.await` points. A transient
/// disk-latency spike (this project's own full test suite is a live
/// example: dozens of test binaries' worth of daemons/workers doing real
/// file I/O at once) landing while every one of this daemon's worker
/// threads happens to be similarly occupied starves the whole executor:
/// no task -- including an already-open client connection's own
/// response -- can run until one frees up. That is the actual mechanism
/// behind `client::RESPONSE_TIMEOUT`'s "daemon did not respond in time":
/// the daemon was never dead, its executor was just fully blocked in
/// synchronous I/O this call used to do inline. Every *write* path
/// (`session::write_state`, `request_journal`) already went through
/// `spawn_blocking` for exactly this reason; this and [`scan`] were the
/// two read paths that hadn't caught up.
pub async fn read_session_state(context: Context, session_dir: &Path) -> Result<SessionState> {
    let session_dir = session_dir.to_path_buf();
    rusty_tokio::spawn_blocking(move || read_session_state_sync(context, &session_dir))
        .await
        .map_err(|_| HarnessError::protocol(context, "session state read task panicked"))?
}

/// Cross-checks each `Active`-recorded session's `worker_pid` against
/// `platform::process::Spawner::is_alive` and reports `Crashed` instead
/// when the pid is gone -- `state.json` itself only ever reflects what
/// the worker last wrote about *itself*, so a dead worker's file still
/// says `Active` until something recovers it; this is that something,
/// for display purposes (`session list`/`daemon status`). It does not
/// itself trigger recovery -- that's `daemon::Supervisor::ensure_worker_running`,
/// invoked on demand by `attach`/`prompt`.
///
/// Async for the same reason [`read_session_state`] is -- see that
/// function's own doc comment. The whole directory walk plus every
/// per-entry read runs inside one `spawn_blocking` call, not one hop per
/// file, so a multi-session scan still costs a single thread-pool
/// round trip.
pub async fn scan(state_root: &Path) -> Result<Vec<SessionSummary>> {
    let state_root = state_root.to_path_buf();
    rusty_tokio::spawn_blocking(move || scan_sync(&state_root))
        .await
        .map_err(|_| HarnessError::protocol(Context::Catalog, "catalog scan task panicked"))?
}

fn scan_sync(state_root: &Path) -> Result<Vec<SessionSummary>> {
    let sessions_dir = paths::sessions_dir(state_root);
    if !sessions_dir.exists() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(&sessions_dir)
        .map_err(|e| HarnessError::io(Context::Catalog, Some(sessions_dir.clone()), e))?;

    let mut summaries = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                eprintln!(
                    "catalog: skipping unreadable entry in {}: {err}",
                    sessions_dir.display()
                );
                continue;
            }
        };
        let session_dir = entry.path();
        if !session_dir.is_dir() {
            continue;
        }
        let state = match read_session_state_sync(Context::Catalog, &session_dir) {
            Ok(s) => s,
            Err(err) => {
                // A directory mid-creation (state.json not written yet)
                // or genuinely corrupt -- either way, one bad session
                // must not fail the whole listing.
                eprintln!("catalog: skipping {}: {err}", session_dir.display());
                continue;
            }
        };
        let effective_status = effective_status(&state);
        summaries.push(SessionSummary {
            session_id: state.session_id,
            name: state.name,
            status: effective_status,
            last_sequence: state.last_sequence,
            updated_at_ms: state.updated_at_ms,
            worker_pid: state.worker_pid,
            generation: state.generation,
            model: state.model,
            goal: state.goal,
            parent_id: state.parent_id,
            thinking: state.thinking,
            tools: state.tools,
            runtime: state.runtime,
            forked_from: state.forked_from,
        });
    }
    summaries.sort_by_key(|s| std::cmp::Reverse(s.updated_at_ms));
    Ok(summaries)
}

fn effective_status(state: &SessionState) -> SessionStatus {
    if state.status != SessionStatus::Active {
        return state.status;
    }
    match state.worker_pid {
        None => SessionStatus::Crashed,
        Some(pid) => {
            match procutil::is_same_process(pid, state.worker_start_fingerprint.as_deref()) {
                Ok(true) => SessionStatus::Active,
                Ok(false) => SessionStatus::Crashed,
                Err(err) => {
                    eprintln!(
                        "catalog: is_same_process({pid}) failed for session {}: {err}",
                        state.session_id
                    );
                    SessionStatus::Crashed
                }
            }
        }
    }
}
