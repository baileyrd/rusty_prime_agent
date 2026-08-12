//! Happy-path coverage of the Required Behavior surface: `daemon
//! start/status/shutdown`, `session new/attach/list`, plus `session
//! prompt` (the pragmatic addition needed to exercise the fake echo
//! provider and give the transcript real content -- see `cli.rs`'s
//! doc comment).

mod common;

use std::time::Duration;

#[test]
fn daemon_start_is_idempotent_and_reports_status() {
    let state_dir = common::TempDir::new("daemon-idempotent");
    common::daemon_start(state_dir.path());
    // Second `daemon start` must recognize the running daemon rather
    // than erroring or spawning a duplicate supervisor.
    common::daemon_start(state_dir.path());

    let status = common::daemon_status(state_dir.path());
    assert!(
        status.contains("sessions_active=0"),
        "expected no active sessions yet, got: {status}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn session_new_prompt_attach_list_round_trip() {
    let state_dir = common::TempDir::new("session-roundtrip");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), Some("integration-test"));
    assert!(
        !session_id.is_empty(),
        "session new must print a non-empty session id"
    );

    let status = common::daemon_status(state_dir.path());
    assert!(
        status.contains("sessions_active=1"),
        "expected one active session, got: {status}"
    );

    let ack = common::session_prompt(state_dir.path(), &session_id, "hello harness");
    assert!(
        ack.contains("echo: hello harness"),
        "prompt ack should contain the fake provider's echo, got: {ack}"
    );

    let listing = common::session_list(state_dir.path());
    assert!(
        listing.contains(&session_id),
        "session list should include {session_id}, got: {listing}"
    );
    assert!(
        listing.contains("active"),
        "listed session should be active, got: {listing}"
    );
    assert!(
        listing.contains("integration-test"),
        "listed session should show its name, got: {listing}"
    );

    let lines = common::attach_lines(state_dir.path(), &session_id, 4, Duration::from_secs(5));
    let joined = lines.join("\n");
    assert!(
        joined.contains("snapshot"),
        "attach should start with a snapshot line, got: {joined}"
    );
    assert!(
        joined.contains("user: hello harness"),
        "attach snapshot should replay the user turn, got: {joined}"
    );
    assert!(
        joined.contains("assistant: echo: hello harness"),
        "attach snapshot should replay the assistant turn, got: {joined}"
    );

    common::daemon_shutdown(state_dir.path());

    // A clean shutdown must mark the session Stopped, not leave it
    // looking crashed.
    assert!(
        common::wait_until(
            || common::session_status(state_dir.path(), &session_id) == "stopped",
            Duration::from_secs(5)
        ),
        "session should be marked stopped after a graceful daemon shutdown"
    );
}

#[test]
fn unknown_session_attach_reports_a_conflict_not_a_crash() {
    let state_dir = common::TempDir::new("unknown-session");
    common::daemon_start(state_dir.path());

    let out = common::run(
        state_dir.path(),
        &["session", "attach", "sess-does-not-exist"],
    );
    assert!(
        !out.status.success(),
        "attaching an unknown session must fail"
    );
    assert_eq!(
        out.status.code(),
        Some(3),
        "unknown-session errors are reported as the conflict exit code"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown session"),
        "expected an 'unknown session' message, got: {stderr}"
    );

    common::daemon_shutdown(state_dir.path());
}
