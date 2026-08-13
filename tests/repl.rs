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

/// Parity with `prime-agent /heartbeat every <duration>` -- unlike plain
/// `/heartbeat` above, this registers a real recurring `session
/// schedule` entry rather than sending anything immediately (see
/// `client::session_repl`'s own doc comment for why).
#[test]
fn repl_heartbeat_every_with_no_active_goal_sends_nothing() {
    let state_dir = common::TempDir::new("repl-heartbeat-every-no-goal");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_repl(
        state_dir.path(),
        &session_id,
        "/heartbeat every 10m\n/exit\n",
    );
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("no active goal"),
        "expected an explanation, got: {stdout}"
    );

    let listing = common::run(
        state_dir.path(),
        &["session", "schedule", "list", &session_id],
    );
    common::assert_success("session schedule list", &listing);
    assert_eq!(common::stdout_string(&listing), "no schedules");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_heartbeat_every_with_an_active_goal_creates_a_recurring_schedule() {
    let state_dir = common::TempDir::new("repl-heartbeat-every-active-goal");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = common::run(
        state_dir.path(),
        &["session", "goal", "set", &session_id, "write", "a", "haiku"],
    );
    common::assert_success("session goal set", &out);

    let out = run_repl(
        state_dir.path(),
        &session_id,
        "/heartbeat every 1h\n/exit\n",
    );
    common::assert_success("session repl", &out);
    let schedule_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(!schedule_id.is_empty(), "expected a printed schedule id");

    // Nothing sent immediately -- it's a standing recurring schedule,
    // not a one-shot send.
    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=0"), "got: {listing}");

    let schedule_listing = common::run(
        state_dir.path(),
        &["session", "schedule", "list", &session_id],
    );
    common::assert_success("session schedule list", &schedule_listing);
    let schedule_listing = common::stdout_string(&schedule_listing);
    assert!(
        schedule_listing.contains(&schedule_id),
        "got: {schedule_listing}"
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

/// Bounded parity with `prime-agent`'s TUI-side file-reference feature
/// -- see `session_repl`'s own `pending_file_content` doc comment.
#[test]
fn repl_file_command_queues_content_into_the_next_prompt() {
    let state_dir = common::TempDir::new("repl-file");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let file_path = state_dir.path().join("notes.txt");
    std::fs::write(&file_path, "the secret ingredient is basil").unwrap();

    let out = run_repl(
        state_dir.path(),
        &session_id,
        &format!("/file {}\nwhat's the secret?\n", file_path.display()),
    );
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("queued"), "got: {stdout}");
    assert!(
        stdout.contains("the secret ingredient is basil") && stdout.contains("what's the secret?"),
        "got: {stdout}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_file_command_on_a_missing_file_reports_an_error_and_sends_nothing() {
    let state_dir = common::TempDir::new("repl-file-missing");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_repl(state_dir.path(), &session_id, "/file does-not-exist.txt\n");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("failed to read"), "got: {stdout}");

    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=0"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}

/// Bounded parity with `prime-agent`'s TUI-side `/fork` -- wires the
/// already-existing `session fork` client call into the REPL loop.
#[test]
fn repl_fork_command_creates_a_new_session_from_the_current_one() {
    let state_dir = common::TempDir::new("repl-fork");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "hello");

    let mut child = Command::new(common::bin())
        .args(["--mode", "json", "session", "repl", &session_id])
        .env("RUSTY_PRIME_AGENT_HOME", state_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn harness session repl");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(b"/fork --name my-fork\n")
        .expect("write repl input");
    let out = child.wait_with_output().expect("wait for repl to exit");
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let forked_line = stdout
        .lines()
        .find(|l| l.contains("\"type\":\"session_new\""))
        .unwrap_or_else(|| panic!("expected a session_new line, got: {stdout}"));
    let value: serde_json::Value = serde_json::from_str(forked_line).unwrap();
    let forked_id = value["session_id"].as_str().unwrap().to_string();
    assert_ne!(forked_id, session_id);

    let listing = common::session_list(state_dir.path());
    assert!(
        listing.contains(&format!("{forked_id}\tactive\tmy-fork")),
        "got: {listing}"
    );

    common::daemon_shutdown(state_dir.path());
}

/// Bounded parity with `prime-agent`'s TUI-side `/export` -- writes the
/// current transcript to a local file as pretty-printed JSON.
#[test]
fn repl_export_command_writes_the_transcript_to_a_file() {
    let state_dir = common::TempDir::new("repl-export");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "hello");

    let export_path = state_dir.path().join("exported.json");
    let out = run_repl(
        state_dir.path(),
        &session_id,
        &format!("/export {}\n", export_path.display()),
    );
    common::assert_success("session repl", &out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("exported 2 turn(s)"), "got: {stdout}");

    let exported = std::fs::read_to_string(&export_path).expect("exported file exists");
    let value: serde_json::Value = serde_json::from_str(&exported).expect("valid JSON");
    let entries = value.as_array().expect("exported transcript is an array");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["text"], "hello");
    assert_eq!(entries[1]["text"], "echo: hello");

    common::daemon_shutdown(state_dir.path());
}
