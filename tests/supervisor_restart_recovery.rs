//! Supervisor restart recovery (Required Behavior: "supervisor restart
//! recovers in-flight session state from disk"). Distinct from the
//! worker-crash tests: here the *worker* keeps running the whole
//! time (workers are `detach()`ed specifically so a supervisor crash
//! doesn't take them down with it) -- only the supervisor is killed and
//! replaced, and recovery means the new supervisor adopts the still-live
//! worker rather than needlessly respawning it.

mod common;

use std::time::Duration;

#[test]
fn restarted_supervisor_adopts_a_still_running_worker() {
    let state_dir = common::TempDir::new("supervisor-restart");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "before supervisor restart");
    let worker_pid_before = common::worker_pid(state_dir.path(), &session_id);

    let supervisor_pid = common::daemon_pid(state_dir.path());
    common::force_kill(supervisor_pid);

    // The worker is a detached, independent process -- it must still be
    // alive even though its supervisor just died.
    assert!(
        common::wait_until(
            || common::run(state_dir.path(), &["daemon", "status"])
                .status
                .code()
                != Some(0),
            Duration::from_secs(5)
        ),
        "the old daemon socket should stop answering once its supervisor is killed"
    );

    // A fresh `daemon start` must come up cleanly even though the old
    // supervisor died mid-flight (stale socket file, stale pid file).
    common::daemon_start(state_dir.path());
    let new_supervisor_pid = common::daemon_pid(state_dir.path());
    assert_ne!(
        supervisor_pid, new_supervisor_pid,
        "daemon start after a crash must launch a genuinely new supervisor"
    );

    // The startup recovery scan (`Supervisor::recover_on_startup`) runs
    // synchronously before the new supervisor's socket is ready, so this
    // is already true by the time `daemon_start` returns -- but a small
    // grace window keeps this test robust if that ever becomes
    // asynchronous.
    assert!(
        common::wait_until(
            || common::session_list(state_dir.path()).contains("active"),
            Duration::from_secs(5)
        ),
        "the session should read back as active under the new supervisor without needing a respawn"
    );

    let worker_pid_after = common::worker_pid(state_dir.path(), &session_id);
    assert_eq!(
        worker_pid_before, worker_pid_after,
        "adopting a live worker must not spawn a replacement -- the pid should be unchanged"
    );

    let ack = common::session_prompt(state_dir.path(), &session_id, "after supervisor restart");
    assert!(
        ack.contains("echo: after supervisor restart"),
        "the adopted worker must still serve prompts, got: {ack}"
    );

    let lines = common::attach_lines(state_dir.path(), &session_id, 6, Duration::from_secs(5));
    let joined = lines.join("\n");
    assert!(
        joined.contains("user: before supervisor restart"),
        "transcript from before the restart must still be present, got: {joined}"
    );
    assert!(
        joined.contains("user: after supervisor restart"),
        "transcript from after the restart must be present too, got: {joined}"
    );
    assert!(
        !joined.contains("-- recovered"),
        "adopting a live worker is not a crash recovery -- no recovery marker should be emitted, got: {joined}"
    );

    common::daemon_shutdown(state_dir.path());
}
