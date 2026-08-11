//! Worker crash handling (Required Behavior: "Worker crash ... recovers
//! in-flight session state from disk (transcript + a small state file)
//! without treating the terminal client as the owner of that state").
//!
//! The crash is simulated with an external, unowned-pid kill (see
//! `common::force_kill`'s doc comment for why that's the honest
//! simulation rather than this project's own graceful-shutdown path),
//! and recovery is triggered by an ordinary client request -- nothing
//! about the recovering client is special or was told about the crash
//! in advance, matching the "without treating the terminal client as
//! the owner" requirement.

mod common;

use std::time::Duration;

#[test]
fn crashed_worker_is_recovered_with_full_transcript_replay() {
    let state_dir = common::TempDir::new("worker-crash");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "turn before the crash");

    let pid_before = common::worker_pid(state_dir.path(), &session_id);
    common::force_kill(pid_before);

    // The kill is asynchronous from this process's point of view --
    // give the OS a moment to actually reap/report it, then confirm the
    // catalog's own liveness cross-check (crate::catalog::effective_status)
    // notices before asserting anything about recovery.
    assert!(
        common::wait_until(
            || common::session_list(state_dir.path()).contains("crashed"),
            Duration::from_secs(10)
        ),
        "session list should report the session as crashed after its worker is killed"
    );

    // An ordinary client request -- not a special "recover" command --
    // is what triggers recovery.
    let ack = common::session_prompt(state_dir.path(), &session_id, "turn after the crash");
    assert!(ack.contains("echo: turn after the crash"), "post-recovery prompt should still work, got: {ack}");

    let pid_after = common::worker_pid(state_dir.path(), &session_id);
    assert_ne!(pid_before, pid_after, "recovery must spawn a genuinely new worker process");

    let lines = common::attach_lines(state_dir.path(), &session_id, 6, Duration::from_secs(5));
    let joined = lines.join("\n");
    assert!(joined.contains("recovered"), "attach stream should surface the visible recovery marker, got: {joined}");
    assert!(
        joined.contains("user: turn before the crash"),
        "recovery must full-replay pre-crash transcript entries, got: {joined}"
    );
    assert!(
        joined.contains("user: turn after the crash"),
        "post-recovery turns must also be present, got: {joined}"
    );
    assert!(joined.contains("generation=2"), "a recovered worker is generation 2 for this session, got: {joined}");

    let status = common::session_status(state_dir.path(), &session_id);
    assert_eq!(status, "active", "recovered session should read back as active, not crashed, in state.json");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn crashed_worker_is_recovered_on_attach_alone() {
    // Same crash, but the recovering request is `session attach`
    // (read-only) rather than `session prompt` (a mutation) -- recovery
    // must not depend on which kind of request happens to notice the
    // dead worker first.
    let state_dir = common::TempDir::new("worker-crash-attach");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "only turn before the crash");

    let pid_before = common::worker_pid(state_dir.path(), &session_id);
    common::force_kill(pid_before);
    assert!(
        common::wait_until(
            || common::session_list(state_dir.path()).contains("crashed"),
            Duration::from_secs(10)
        ),
        "session list should report crashed before attaching"
    );

    let lines = common::attach_lines(state_dir.path(), &session_id, 6, Duration::from_secs(5));
    let joined = lines.join("\n");
    assert!(joined.contains("attached to"), "attach should still succeed against a crashed session, got: {joined}");
    assert!(joined.contains("recovered"), "attach itself should trigger and surface recovery, got: {joined}");
    assert!(
        joined.contains("user: only turn before the crash"),
        "pre-crash transcript must survive recovery triggered by attach, got: {joined}"
    );

    common::daemon_shutdown(state_dir.path());
}
