//! `session fork <id> [--at N]` -- bounded parity with a slice of
//! `prime-agent`'s `/fork`, see `protocol::Request::SessionFork`'s own
//! doc comment for exactly what's implemented (session-level forking,
//! reusing this project's existing session-creation machinery) and what
//! isn't (`/clone`'s live-state duplication -- `/tree` visualization and
//! active-leaf switching are covered by `tests/session_tree.rs` instead).

mod common;

use std::process::Command;

fn session_fork(state_dir: &std::path::Path, session_id: &str, extra: &[&str]) -> String {
    let mut args = vec!["session", "fork", session_id];
    args.extend_from_slice(extra);
    let out = common::run(state_dir, &args);
    common::assert_success("session fork", &out);
    common::stdout_string(&out)
}

#[test]
fn fork_copies_the_transcript_up_to_the_given_sequence() {
    let state_dir = common::TempDir::new("fork-truncate");
    common::daemon_start(state_dir.path());
    let source = common::session_new(state_dir.path(), Some("source"));
    common::session_prompt(state_dir.path(), &source, "first");
    common::session_prompt(state_dir.path(), &source, "second");
    // 4 turns now: first(user)+first(assistant)+second(user)+second(assistant).

    let forked = session_fork(state_dir.path(), &source, &["--at", "2"]);
    assert_ne!(forked, source);

    let listing = common::session_list(state_dir.path());
    assert!(
        listing.contains(&format!("{forked}\tactive")) && listing.contains("turns=2"),
        "got: {listing}"
    );

    let lines = common::attach_lines_with_args(
        state_dir.path(),
        &["--mode", "json", "session", "attach", &forked],
        2,
        std::time::Duration::from_secs(5),
    );
    let snapshot = lines.join("\n");
    assert!(snapshot.contains("first"), "got: {snapshot}");
    assert!(!snapshot.contains("second"), "got: {snapshot}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn fork_with_no_at_flag_copies_the_whole_transcript() {
    let state_dir = common::TempDir::new("fork-whole");
    common::daemon_start(state_dir.path());
    let source = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &source, "hello");

    let forked = session_fork(state_dir.path(), &source, &[]);
    let listing = common::session_list(state_dir.path());
    assert!(
        listing.contains(&format!("{forked}\tactive")),
        "got: {listing}"
    );
    assert!(listing.contains("turns=2"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn fork_supports_a_display_name() {
    let state_dir = common::TempDir::new("fork-name");
    common::daemon_start(state_dir.path());
    let source = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &source, "hello");

    let forked = session_fork(state_dir.path(), &source, &["--name", "my-fork"]);
    let listing = common::session_list(state_dir.path());
    assert!(
        listing.contains(&format!("{forked}\tactive\tmy-fork")),
        "got: {listing}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn forking_at_a_sequence_past_the_end_is_a_conflict() {
    let state_dir = common::TempDir::new("fork-past-end");
    common::daemon_start(state_dir.path());
    let source = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &source, "hello");

    let out = common::run(
        state_dir.path(),
        &["session", "fork", &source, "--at", "999"],
    );
    assert!(
        !out.status.success(),
        "expected a failure exit code, got success: {}",
        common::stdout_string(&out)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("999"), "got: {stderr}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn forking_an_unknown_session_is_a_conflict() {
    let state_dir = common::TempDir::new("fork-unknown");
    common::daemon_start(state_dir.path());

    let out = common::run(state_dir.path(), &["session", "fork", "does-not-exist"]);
    assert!(
        !out.status.success(),
        "expected a failure exit code, got success: {}",
        common::stdout_string(&out)
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn prompting_the_fork_does_not_affect_the_source_session() {
    let state_dir = common::TempDir::new("fork-independent");
    common::daemon_start(state_dir.path());
    let source = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &source, "hello");

    let forked = session_fork(state_dir.path(), &source, &[]);
    common::session_prompt(state_dir.path(), &forked, "only on the fork");

    let listing = common::session_list(state_dir.path());
    // Find the source's own line and confirm it still shows exactly 2
    // turns -- the fork's own extra prompt must not leak back onto it.
    let source_line = listing
        .lines()
        .find(|l| l.starts_with(&source))
        .expect("source line present");
    assert!(source_line.contains("turns=2"), "got: {source_line}");
    let forked_line = listing
        .lines()
        .find(|l| l.starts_with(&forked))
        .expect("forked line present");
    assert!(forked_line.contains("turns=4"), "got: {forked_line}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn json_mode_reports_forked_from_provenance() {
    let state_dir = common::TempDir::new("fork-json-provenance");
    common::daemon_start(state_dir.path());
    let source = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &source, "hello");

    let forked = session_fork(state_dir.path(), &source, &[]);

    let mut cmd = Command::new(common::bin());
    cmd.args(["--mode", "json", "session", "list"])
        .env("RUSTY_PRIME_AGENT_HOME", state_dir.path());
    let out = cmd.output().expect("failed to run harness session list");
    common::assert_success("session list --mode json", &out);
    let stdout = common::stdout_string(&out);
    let sessions: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let sessions = sessions["sessions"].as_array().expect("sessions array");
    let forked_entry = sessions
        .iter()
        .find(|s| s["session_id"] == forked)
        .expect("forked session present");
    assert_eq!(forked_entry["forked_from"]["session_id"], source);
    assert_eq!(forked_entry["forked_from"]["at_sequence"], 2);

    let source_entry = sessions
        .iter()
        .find(|s| s["session_id"] == source)
        .expect("source session present");
    assert!(source_entry["forked_from"].is_null());

    common::daemon_shutdown(state_dir.path());
}
