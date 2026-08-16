//! PID-reuse-safe worker liveness (`procutil::is_same_process`) --
//! parity with `prime-agent`'s own lease-owner check (`R-WRK-14`):
//! "lease-owner liveness combines a raw process-existence check with a
//! process-start-time fingerprint match, so a reused PID from a dead
//! owner is never mistaken for the same live process."
//!
//! The gap this closes, from `COMPARISON.md` §5: a bare `kill(pid, 0)`
//! answers "does *a* process with this number exist", not "is this still
//! the worker that wrote `state.json`". A supervisor that trusts the
//! former declines to respawn a genuinely dead worker whose pid an
//! unrelated process now holds, leaving that session wedged with no live
//! worker and nothing to notice.
//!
//! Real pid reuse cannot be provoked on demand -- it needs the pid space
//! to wrap -- so these tests forge the *observable* end state instead:
//! `state.json` naming a live pid that some other process owns. That is
//! precisely what the supervisor would see after a genuine reuse, which
//! is the thing whose handling is under test.

mod common;

use std::path::Path;
use std::time::Duration;

fn state_path(state_dir: &Path, session_id: &str) -> std::path::PathBuf {
    state_dir
        .join("sessions")
        .join(session_id)
        .join("state.json")
}

fn read_state(state_dir: &Path, session_id: &str) -> serde_json::Value {
    let p = state_path(state_dir, session_id);
    serde_json::from_str(
        &std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("reading {}: {e}", p.display())),
    )
    .expect("state.json is valid JSON")
}

fn write_state(state_dir: &Path, session_id: &str, state: &serde_json::Value) {
    std::fs::write(
        state_path(state_dir, session_id),
        serde_json::to_string_pretty(state).unwrap(),
    )
    .unwrap();
}

#[test]
fn a_worker_records_its_own_start_fingerprint_alongside_its_pid() {
    let state_dir = common::TempDir::new("pid-reuse-recorded");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "hello");

    let state = read_state(state_dir.path(), &session_id);
    let pid = state["worker_pid"]
        .as_u64()
        .expect("worker_pid is recorded");
    let fingerprint = state["worker_start_fingerprint"]
        .as_str()
        .expect("worker_start_fingerprint is recorded next to worker_pid");
    assert!(
        !fingerprint.is_empty(),
        "an empty fingerprint would silently degrade back to the bare pid check"
    );
    assert_eq!(
        pid,
        common::worker_pid(state_dir.path(), &session_id) as u64,
        "the fingerprint must belong to the pid recorded beside it"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn a_live_pid_with_a_foreign_fingerprint_reads_as_crashed_not_healthy() {
    let state_dir = common::TempDir::new("pid-reuse-detected");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "before");

    // Stop the daemon first, and note that this is a *graceful* stop
    // rather than a kill: `state.json` is worker-owned while a worker is
    // running, so a live worker would overwrite the edit below on its
    // next periodic write. (An earlier version of this test also
    // force-killed the worker afterwards, which just panicked -- by then
    // there was nothing left to kill.)
    common::daemon_shutdown(state_dir.path());

    // Forge the aftermath of pid reuse: the recorded pid is alive (it is
    // this test process), but it is not the process that recorded it.
    let mut state = read_state(state_dir.path(), &session_id);
    state["worker_pid"] = serde_json::json!(std::process::id());
    state["status"] = serde_json::json!("active");
    state["worker_start_fingerprint"] = serde_json::json!("not-the-fingerprint-of-this-process");
    write_state(state_dir.path(), &session_id, &state);

    common::daemon_start(state_dir.path());

    // Before this change the bare `kill(pid, 0)` would have said "alive"
    // and reported the session healthy forever.
    let listing = common::session_list(state_dir.path());
    let line = listing
        .lines()
        .find(|l| l.contains(&session_id))
        .unwrap_or_else(|| panic!("session {session_id} missing from listing:\n{listing}"));
    assert!(
        line.to_lowercase().contains("crashed"),
        "a live pid that is not the recorded process must not read as healthy; got: {line}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn a_mismatched_fingerprint_lets_the_session_recover_instead_of_wedging() {
    let state_dir = common::TempDir::new("pid-reuse-recovers");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "before the reuse");

    // Graceful stop, same reasoning as the test above.
    common::daemon_shutdown(state_dir.path());

    let mut state = read_state(state_dir.path(), &session_id);
    state["worker_pid"] = serde_json::json!(std::process::id());
    state["status"] = serde_json::json!("active");
    state["worker_start_fingerprint"] = serde_json::json!("not-the-fingerprint-of-this-process");
    write_state(state_dir.path(), &session_id, &state);

    common::daemon_start(state_dir.path());

    // The point of detecting it: `ensure_worker_running` respawns rather
    // than connecting to a socket the impostor pid never owned. Without
    // the fingerprint this prompt would hang or fail, because the
    // supervisor would believe a worker was already serving this session.
    assert!(
        common::wait_until(
            || common::run(
                state_dir.path(),
                &["session", "prompt", &session_id, "after the reuse"],
            )
            .status
            .success(),
            Duration::from_secs(60)
        ),
        "a session whose recorded pid was reused must recover, not wedge"
    );

    let state = read_state(state_dir.path(), &session_id);
    assert_ne!(
        state["worker_pid"].as_u64(),
        Some(std::process::id() as u64),
        "the respawned worker must own the session, not the impostor pid"
    );
    assert!(
        state["worker_start_fingerprint"].as_str().is_some(),
        "the respawned worker must record its own fingerprint"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn a_state_file_without_a_fingerprint_still_works() {
    // Back-compat: a `state.json` written before this field existed
    // parses as `None` and falls back to the bare pid check, rather than
    // failing recovery or declaring a live worker dead.
    let state_dir = common::TempDir::new("pid-reuse-legacy");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "before");

    common::daemon_shutdown(state_dir.path());

    let mut state = read_state(state_dir.path(), &session_id);
    state
        .as_object_mut()
        .unwrap()
        .remove("worker_start_fingerprint");
    write_state(state_dir.path(), &session_id, &state);
    assert!(
        read_state(state_dir.path(), &session_id)
            .get("worker_start_fingerprint")
            .is_none(),
        "the field must actually be absent for this to test anything"
    );

    common::daemon_start(state_dir.path());
    assert!(
        !common::session_prompt(state_dir.path(), &session_id, "after")
            .trim()
            .is_empty(),
        "a pre-upgrade state.json must still drive a working session"
    );
    // And the worker that took over records one going forward.
    let state = read_state(state_dir.path(), &session_id);
    assert!(state["worker_start_fingerprint"].as_str().is_some());

    common::daemon_shutdown(state_dir.path());
}
