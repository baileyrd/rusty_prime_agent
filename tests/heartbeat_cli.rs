//! `harness session heartbeat <id> [--every DURATION]` -- a top-level
//! CLI entry point into the same re-entry mechanism `session_repl`'s own
//! `/heartbeat`/`/heartbeat every <duration>` lines cover (see
//! `tests/repl.rs`'s own heartbeat tests for those). CI-safe throughout:
//! `EchoProvider`, no real model needed.

mod common;

#[test]
fn heartbeat_with_no_active_goal_sends_nothing() {
    let state_dir = common::TempDir::new("heartbeat-cli-no-goal");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = common::run(state_dir.path(), &["session", "heartbeat", &session_id]);
    common::assert_success("session heartbeat", &out);
    let stdout = common::stdout_string(&out);
    assert!(stdout.contains("no active goal"), "got: {stdout}");

    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=0"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn heartbeat_with_an_active_goal_sends_a_continuation_prompt_immediately() {
    let state_dir = common::TempDir::new("heartbeat-cli-active-goal");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::assert_success(
        "session goal set",
        &common::run(
            state_dir.path(),
            &[
                "session",
                "goal",
                "set",
                &session_id,
                "ship",
                "the",
                "feature",
            ],
        ),
    );

    let out = common::run(state_dir.path(), &["session", "heartbeat", &session_id]);
    common::assert_success("session heartbeat", &out);
    let stdout = common::stdout_string(&out);
    assert!(
        stdout.contains("echo: Continue working toward the goal: ship the feature"),
        "got: {stdout}"
    );

    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=2"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn heartbeat_every_with_an_active_goal_registers_a_recurring_schedule() {
    let state_dir = common::TempDir::new("heartbeat-cli-every");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::assert_success(
        "session goal set",
        &common::run(
            state_dir.path(),
            &["session", "goal", "set", &session_id, "ship it"],
        ),
    );

    let out = common::run(
        state_dir.path(),
        &["session", "heartbeat", &session_id, "--every", "10m"],
    );
    common::assert_success("session heartbeat --every", &out);

    // Registers a real schedule rather than sending anything immediately
    // -- listed the same way any other schedule is, no bespoke surface.
    let listing = common::stdout_string(&common::run(
        state_dir.path(),
        &["session", "schedule", "list", &session_id],
    ));
    assert!(listing.contains("every=600000ms"), "got: {listing}");
    // Nothing sent immediately -- still 0 turns.
    let sessions = common::session_list(state_dir.path());
    assert!(sessions.contains("turns=0"), "got: {sessions}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn heartbeat_every_rejects_a_malformed_duration_at_parse_time() {
    let state_dir = common::TempDir::new("heartbeat-cli-bad-duration");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = common::run(
        state_dir.path(),
        &[
            "session",
            "heartbeat",
            &session_id,
            "--every",
            "not-a-duration",
        ],
    );
    assert!(
        !out.status.success(),
        "expected a bad --every value to fail loudly"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn interrupt_on_an_idle_session_still_acks_cleanly() {
    let state_dir = common::TempDir::new("interrupt-idle");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = common::run(state_dir.path(), &["session", "interrupt", &session_id]);
    common::assert_success("session interrupt", &out);
    let stdout = common::stdout_string(&out);
    assert!(stdout.contains("interrupt requested"), "got: {stdout}");

    // The session is still fully usable afterward -- a no-op interrupt
    // doesn't leave anything broken.
    let ack = common::session_prompt(state_dir.path(), &session_id, "hello");
    assert!(ack.contains("echo: hello"), "got: {ack}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn interrupt_on_an_unknown_session_reports_an_error() {
    let state_dir = common::TempDir::new("interrupt-unknown");
    common::daemon_start(state_dir.path());

    let out = common::run(
        state_dir.path(),
        &["session", "interrupt", "sess-does-not-exist"],
    );
    assert!(
        !out.status.success(),
        "expected interrupting an unknown session id to fail"
    );

    common::daemon_shutdown(state_dir.path());
}
