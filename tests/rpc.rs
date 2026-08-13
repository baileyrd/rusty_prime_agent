//! Parity with `prime-agent --mode rpc` -- see `client::session_rpc`'s
//! own doc comment for the design (reuses `Request`/`Response`/
//! `SessionEvent` directly as the RPC vocabulary, two concurrent lanes
//! sharing one stdout). Uses `EchoProvider` throughout -- no real model
//! needed, since these tests exercise the plumbing, not a provider.

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

/// Pipes `input` to `harness session rpc <id>`'s stdin, closes it (EOF),
/// and waits for the process to exit on its own -- same shape as
/// `tests/repl.rs`/`tests/compaction.rs`'s own local `run_repl` helpers.
fn run_rpc(state_dir: &std::path::Path, session_id: &str, input: &str) -> std::process::Output {
    let mut child = Command::new(common::bin())
        .args(["session", "rpc", session_id])
        .env("RUSTY_PRIME_AGENT_HOME", state_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn harness session rpc");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(input.as_bytes())
        .expect("write rpc input");
    child.wait_with_output().expect("wait for rpc to exit")
}

#[test]
fn rpc_prompt_command_gets_a_response_and_streams_events() {
    let state_dir = common::TempDir::new("rpc-prompt");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let cmd =
        format!(r#"{{"type":"session_prompt","session_id":"{session_id}","text":"hello rpc"}}"#);
    let out = run_rpc(state_dir.path(), &session_id, &format!("{cmd}\n"));
    common::assert_success("session rpc", &out);
    let stdout = common::stdout_string(&out);

    // The command's own response.
    assert!(
        stdout.contains(r#""type":"session_prompt_ack""#),
        "got: {stdout}"
    );
    assert!(stdout.contains("echo: hello rpc"), "got: {stdout}");

    // The same turn, also delivered via the concurrent event-forwarding
    // lane -- both are expected, not a duplicate bug (prime-agent's own
    // rpc.md: "Async events -- responses confirm acceptance; actual work
    // streams as separate events").
    assert!(stdout.contains(r#""kind":"turn""#), "got: {stdout}");
    assert!(
        stdout.contains(r#""type":"session_attach_started""#),
        "got: {stdout}"
    );

    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=2"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn rpc_rejects_a_session_attach_command_locally() {
    let state_dir = common::TempDir::new("rpc-reject-attach");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let cmd = format!(r#"{{"type":"session_attach","session_id":"{session_id}"}}"#);
    let out = run_rpc(state_dir.path(), &session_id, &format!("{cmd}\n"));
    common::assert_success("session rpc", &out);
    let stdout = common::stdout_string(&out);
    assert!(stdout.contains("redundant in rpc mode"), "got: {stdout}");

    // No prompt was ever sent, so the transcript is still empty.
    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=0"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn rpc_reports_an_error_for_invalid_command_json() {
    let state_dir = common::TempDir::new("rpc-bad-json");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_rpc(state_dir.path(), &session_id, "not valid json\n");
    common::assert_success("session rpc", &out);
    let stdout = common::stdout_string(&out);
    assert!(
        stdout.contains(r#""type":"error""#) && stdout.contains("invalid command JSON"),
        "got: {stdout}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn rpc_ends_at_stdin_eof_with_no_commands() {
    let state_dir = common::TempDir::new("rpc-eof-only");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = run_rpc(state_dir.path(), &session_id, "");
    common::assert_success("session rpc", &out);
    let stdout = common::stdout_string(&out);
    // Still gets the initial attach snapshot from the background
    // event-forwarding lane even with no commands sent.
    assert!(
        stdout.contains(r#""type":"session_attach_started""#),
        "got: {stdout}"
    );

    common::daemon_shutdown(state_dir.path());
}
