//! Session catalog scan (Required Behavior: "`harness session list` --
//! catalog scan of active + saved sessions").
//!
//! The reference architecture runs this as a separate "catalog
//! subprocess" so a scan failure can't interrupt active workers. This
//! project runs it in-process inside the supervisor instead: Phase 1's
//! Architecture Constraints call for a modular monolith ("one binary,
//! internal module boundaries, not separate services, unless a concrete
//! forcing function shows up"), and a plain directory scan over small
//! JSON files is not a forcing function -- it can't block the supervisor
//! for long enough to matter, and a scan error here is caught and
//! reported per-entry rather than propagated, so one unreadable session
//! directory still can't take down `session list` for every other
//! session, which is the actual property the reference design is
//! protecting.

use std::path::Path;

use crate::error::{Context, HarnessError, Result};
use crate::paths;
use crate::procutil;
use crate::protocol::{SessionState, SessionStatus, SessionSummary};

/// Read `state.json` for one session directory. Shared with
/// `crate::session` (which writes/recovers it) and `crate::daemon`
/// (which needs the raw `worker_pid` to forward a request).
pub fn read_session_state(context: Context, session_dir: &Path) -> Result<SessionState> {
    let path = paths::state_file_path(session_dir);
    let text = std::fs::read_to_string(&path).map_err(|e| HarnessError::io(context, Some(path.clone()), e))?;
    serde_json::from_str(&text).map_err(|e| HarnessError::json(context, Some(path), e))
}

/// Cross-checks each `Active`-recorded session's `worker_pid` against
/// `platform::process::Spawner::is_alive` and reports `Crashed` instead
/// when the pid is gone -- `state.json` itself only ever reflects what
/// the worker last wrote about *itself*, so a dead worker's file still
/// says `Active` until something recovers it; this is that something,
/// for display purposes (`session list`/`daemon status`). It does not
/// itself trigger recovery -- that's `daemon::Supervisor::ensure_worker_running`,
/// invoked on demand by `attach`/`prompt`.
pub fn scan(state_root: &Path) -> Result<Vec<SessionSummary>> {
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
                eprintln!("catalog: skipping unreadable entry in {}: {err}", sessions_dir.display());
                continue;
            }
        };
        let session_dir = entry.path();
        if !session_dir.is_dir() {
            continue;
        }
        let state = match read_session_state(Context::Catalog, &session_dir) {
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
        });
    }
    summaries.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
    Ok(summaries)
}

fn effective_status(state: &SessionState) -> SessionStatus {
    if state.status != SessionStatus::Active {
        return state.status;
    }
    match state.worker_pid {
        None => SessionStatus::Crashed,
        Some(pid) => match procutil::is_alive(pid) {
            Ok(true) => SessionStatus::Active,
            Ok(false) => SessionStatus::Crashed,
            Err(err) => {
                eprintln!("catalog: is_alive({pid}) failed for session {}: {err}", state.session_id);
                SessionStatus::Crashed
            }
        },
    }
}
