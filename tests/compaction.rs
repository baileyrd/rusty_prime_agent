//! Parity with `prime-agent`'s automatic context compaction
//! (`packages/coding-agent/docs/compaction.md`) -- see
//! `session::AgentSession::compact_now`'s own doc comment for the design.
//!
//! CI-safe coverage only: a real compaction round trip needs a real
//! model to summarize with (`self.provider.respond`), so it's covered by
//! an `#[ignore]`d test in `tests/ollama_provider.rs` instead, the same
//! split every other real-model-requiring feature in this project uses.
//! What's covered here is the honest, always-true-in-CI no-op path: a
//! session with no `--model` set (`EchoProvider`) has nothing to
//! summarize with, so `session compact`/`/compact` must report that
//! plainly rather than error or silently do nothing.

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

/// Same shape as `tests/repl.rs`'s own private `run_repl` helper --
/// each `tests/*.rs` file compiles as its own crate, so it isn't shared
/// directly.
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
fn compact_on_a_session_with_no_model_is_a_no_op() {
    let state_dir = common::TempDir::new("compact-no-model");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "hello");

    let out = common::run(state_dir.path(), &["session", "compact", &session_id]);
    common::assert_success("session compact", &out);
    assert_eq!(
        common::stdout_string(&out),
        "nothing to compact (no model configured, or nothing old enough yet)"
    );

    // Nothing was appended to the transcript -- still just the one
    // user+assistant pair from the prompt above.
    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=2"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn compact_with_instructions_on_a_session_with_no_model_is_still_a_no_op() {
    let state_dir = common::TempDir::new("compact-no-model-instructions");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = common::run(
        state_dir.path(),
        &[
            "session",
            "compact",
            &session_id,
            "focus",
            "on",
            "the",
            "auth",
            "refactor",
        ],
    );
    common::assert_success("session compact with instructions", &out);
    assert_eq!(
        common::stdout_string(&out),
        "nothing to compact (no model configured, or nothing old enough yet)"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn compact_json_mode_reports_compacted_false_and_no_summary() {
    let state_dir = common::TempDir::new("compact-json");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = common::run(
        state_dir.path(),
        &["--mode", "json", "session", "compact", &session_id],
    );
    common::assert_success("session compact --mode json", &out);
    let stdout = common::stdout_string(&out);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON line");
    assert_eq!(value["type"], "session_compact_ack");
    assert_eq!(value["compacted"], false);
    assert!(value["summary"].is_null());

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn repl_compact_with_no_model_is_a_no_op() {
    let state_dir = common::TempDir::new("repl-compact-no-model");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_repl(state_dir.path(), &session_id, "/compact\n/exit\n");
    common::assert_success("session repl", &out);
    let stdout = common::stdout_string(&out);
    assert!(
        stdout.contains("nothing to compact"),
        "expected the no-op message, got: {stdout}"
    );

    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=0"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}
