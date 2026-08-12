//! Minimal, non-Python parity with `prime-agent`'s interactive TUI --
//! see `client::session_repl`'s own doc comment for exactly what it does
//! and doesn't cover. Uses `EchoProvider` throughout.

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

/// Runs `session repl <id>` with `input` piped to stdin and closed
/// (EOF) once written -- exercises the REPL's own "loop until EOF"
/// termination path without needing `/exit`.
fn run_repl(state_dir: &std::path::Path, session_id: &str, input: &str) -> std::process::Output {
    let mut child = Command::new(common::bin())
        .args(["session", "repl", session_id])
        .env("RUSTY_PRIME_AGENT_HOME", state_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn harness session repl");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(input.as_bytes())
        .expect("write repl input");
    child.wait_with_output().expect("wait for repl to exit")
}

#[test]
fn repl_sends_each_line_as_a_prompt_and_exits_on_eof() {
    let state_dir = common::TempDir::new("repl-eof");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_repl(state_dir.path(), &session_id, "hello\nworld\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("echo: hello"), "got: {stdout}");
    assert!(stdout.contains("echo: world"), "got: {stdout}");

    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=4"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_stops_at_an_explicit_exit_line_without_reaching_eof() {
    let state_dir = common::TempDir::new("repl-exit");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    // A line after `/exit` must never be sent.
    let out = run_repl(state_dir.path(), &session_id, "first\n/exit\nnever sent\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("echo: first"), "got: {stdout}");
    assert!(!stdout.contains("never sent"), "got: {stdout}");

    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=2"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}

/// Parity with `prime-agent`'s `/heartbeat` -- see `client::session_repl`'s
/// own doc comment for why it's an immediate `send_prompt`, not routed
/// through `session schedule` the way its kernel-callable sibling
/// (`rlm_heartbeat()`) has to be.
#[test]
fn repl_heartbeat_with_no_active_goal_sends_nothing() {
    let state_dir = common::TempDir::new("repl-heartbeat-no-goal");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_repl(state_dir.path(), &session_id, "/heartbeat\n/exit\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("no active goal"),
        "expected an explanation, got: {stdout}"
    );

    // Nothing was sent -- the transcript is still empty.
    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=0"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_heartbeat_with_an_active_goal_sends_a_continuation_prompt() {
    let state_dir = common::TempDir::new("repl-heartbeat-active-goal");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = common::run(
        state_dir.path(),
        &["session", "goal", "set", &session_id, "write", "a", "haiku"],
    );
    common::assert_success("session goal set", &out);

    let out = run_repl(state_dir.path(), &session_id, "/heartbeat\n/exit\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("echo: Continue working toward the goal: write a haiku"),
        "got: {stdout}"
    );

    let listing = common::session_list(state_dir.path());
    assert!(
        listing.contains("turns=2"),
        "one heartbeat should produce one user+assistant pair, got: {listing}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_replays_prior_transcript_before_reading_new_input() {
    let state_dir = common::TempDir::new("repl-replay");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "already said");

    let out = run_repl(state_dir.path(), &session_id, "");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("already said") && stdout.contains("echo: already said"),
        "got: {stdout}"
    );

    common::daemon_shutdown(state_dir.path());
}
