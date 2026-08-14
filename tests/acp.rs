//! Parity with `prime-agent --mode acp` -- see `acp`'s own module doc
//! comment for the design (a bounded, spec-verified first slice: the
//! baseline `AgentCapabilities.session = {}` surface). Uses
//! `EchoProvider` throughout -- no real model needed.
//!
//! Unlike `tests/rpc.rs`'s own `run_rpc` (write everything, close
//! stdin, wait for exit), these tests need to read one response before
//! deciding what to send next (`session/new`'s reply carries the
//! `sessionId` every later message needs) -- so this file drives an
//! interactive child process instead: a background thread streams
//! stdout lines into a channel while the test thread writes to stdin
//! and pulls matching lines out of that channel with a timeout.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

struct AcpConn {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl AcpConn {
    fn spawn(state_dir: &std::path::Path) -> Self {
        let mut child = Command::new(common::bin())
            .arg("acp")
            .env("RUSTY_PRIME_AGENT_HOME", state_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn harness acp");
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines: rx,
        }
    }

    fn send(&mut self, value: serde_json::Value) {
        let mut text = serde_json::to_string(&value).expect("serialize acp message");
        text.push('\n');
        self.stdin
            .write_all(text.as_bytes())
            .expect("write acp message");
        self.stdin.flush().expect("flush acp message");
    }

    /// Reads lines until one satisfies `pred`, discarding non-matching
    /// lines along the way (other notifications interleaved from a
    /// concurrently-processed message, matching `acp`'s own per-message
    /// task-spawn design). Panics if none arrives within `timeout`.
    fn recv_matching(
        &self,
        mut pred: impl FnMut(&serde_json::Value) -> bool,
        timeout: Duration,
    ) -> serde_json::Value {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                panic!("timed out waiting for a matching acp message");
            }
            let line = self
                .lines
                .recv_timeout(remaining)
                .expect("acp connection closed before a matching message arrived");
            let value: serde_json::Value =
                serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad acp json {line}: {e}"));
            if pred(&value) {
                return value;
            }
        }
    }

    /// Closes stdin (EOF) and waits for the process to exit, returning
    /// its captured stderr for a test that wants to assert on it.
    fn close(self) -> std::process::Output {
        drop(self.stdin);
        self.child.wait_with_output().expect("wait for acp to exit")
    }
}

fn recv_response(conn: &AcpConn, id: i64) -> serde_json::Value {
    conn.recv_matching(
        |v| v.get("id").and_then(serde_json::Value::as_i64) == Some(id),
        Duration::from_secs(10),
    )
}

fn recv_notification(conn: &AcpConn, method: &str) -> serde_json::Value {
    conn.recv_matching(
        |v| v.get("method").and_then(serde_json::Value::as_str) == Some(method),
        Duration::from_secs(10),
    )
}

fn send_initialize(conn: &mut AcpConn) {
    conn.send(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": 2,
            "info": {"name": "test-client", "version": "1.0.0"},
        },
    }));
    let response = recv_response(conn, 0);
    let result = response.get("result").expect("initialize result");
    assert_eq!(result["protocolVersion"], 2);
    assert_eq!(result["capabilities"]["session"], serde_json::json!({}));
    assert_eq!(result["authMethods"], serde_json::json!([]));
}

fn new_session(conn: &mut AcpConn, id: i64) -> String {
    conn.send(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/new",
        "params": {"cwd": "/tmp"},
    }));
    let response = recv_response(conn, id);
    response["result"]["sessionId"]
        .as_str()
        .expect("sessionId in session/new response")
        .to_string()
}

#[test]
fn acp_initialize_reports_the_baseline_session_capability() {
    let state_dir = common::TempDir::new("acp-initialize");
    common::daemon_start(state_dir.path());
    let mut conn = AcpConn::spawn(state_dir.path());

    send_initialize(&mut conn);

    common::daemon_shutdown(state_dir.path());
    let _ = conn.close();
}

#[test]
fn acp_session_new_before_initialize_is_rejected() {
    let state_dir = common::TempDir::new("acp-uninitialized");
    common::daemon_start(state_dir.path());
    let mut conn = AcpConn::spawn(state_dir.path());

    conn.send(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/new",
        "params": {"cwd": "/tmp"},
    }));
    let response = recv_response(&conn, 1);
    assert_eq!(response["error"]["code"], -32600);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("initialize"),
        "got: {response}"
    );

    common::daemon_shutdown(state_dir.path());
    let _ = conn.close();
}

#[test]
fn acp_session_new_requires_cwd() {
    let state_dir = common::TempDir::new("acp-missing-cwd");
    common::daemon_start(state_dir.path());
    let mut conn = AcpConn::spawn(state_dir.path());
    send_initialize(&mut conn);

    conn.send(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/new",
        "params": {},
    }));
    let response = recv_response(&conn, 1);
    assert_eq!(response["error"]["code"], -32602);

    common::daemon_shutdown(state_dir.path());
    let _ = conn.close();
}

#[test]
fn acp_prompt_round_trip_emits_a_chunk_then_idle_then_the_response() {
    let state_dir = common::TempDir::new("acp-prompt");
    common::daemon_start(state_dir.path());
    let mut conn = AcpConn::spawn(state_dir.path());
    send_initialize(&mut conn);
    let session_id = new_session(&mut conn, 1);

    conn.send(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "hello acp"}],
        },
    }));

    let chunk = recv_notification(&conn, "session/update");
    assert_eq!(chunk["params"]["sessionId"], session_id);
    assert_eq!(
        chunk["params"]["update"]["sessionUpdate"],
        "agent_message_chunk"
    );
    assert!(
        chunk["params"]["update"]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("echo: hello acp"),
        "got: {chunk}"
    );

    let state_update = recv_notification(&conn, "session/update");
    assert_eq!(
        state_update["params"]["update"]["sessionUpdate"],
        "state_update"
    );
    assert_eq!(state_update["params"]["update"]["state"], "idle");
    assert_eq!(state_update["params"]["update"]["stopReason"], "end_turn");

    let response = recv_response(&conn, 2);
    assert_eq!(response["result"], serde_json::json!({}));

    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=2"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
    let _ = conn.close();
}

#[test]
fn acp_prompt_rejects_an_unknown_session_id() {
    let state_dir = common::TempDir::new("acp-unknown-session");
    common::daemon_start(state_dir.path());
    let mut conn = AcpConn::spawn(state_dir.path());
    send_initialize(&mut conn);

    conn.send(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/prompt",
        "params": {
            "sessionId": "not-a-real-session",
            "prompt": [{"type": "text", "text": "hi"}],
        },
    }));
    let response = recv_response(&conn, 1);
    assert_eq!(response["error"]["code"], -32602);

    common::daemon_shutdown(state_dir.path());
    let _ = conn.close();
}

#[test]
fn acp_unsupported_content_blocks_are_flagged_not_silently_dropped() {
    let state_dir = common::TempDir::new("acp-content-blocks");
    common::daemon_start(state_dir.path());
    let mut conn = AcpConn::spawn(state_dir.path());
    send_initialize(&mut conn);
    let session_id = new_session(&mut conn, 1);

    conn.send(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [
                {"type": "text", "text": "look at this:"},
                {"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"},
            ],
        },
    }));

    let chunk = recv_notification(&conn, "session/update");
    let text = chunk["params"]["update"]["content"]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(text.contains("look at this:"), "got: {text}");
    assert!(
        text.contains("[unsupported content block: image]"),
        "got: {text}"
    );

    common::daemon_shutdown(state_dir.path());
    let _ = conn.close();
}

#[test]
fn acp_session_close_stops_the_underlying_worker() {
    let state_dir = common::TempDir::new("acp-close");
    common::daemon_start(state_dir.path());
    let mut conn = AcpConn::spawn(state_dir.path());
    send_initialize(&mut conn);
    let session_id = new_session(&mut conn, 1);

    conn.send(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/close",
        "params": {"sessionId": session_id},
    }));
    let response = recv_response(&conn, 2);
    assert_eq!(response["result"], serde_json::json!({}));

    let ok = common::wait_until(
        || common::session_status(state_dir.path(), &session_id) == "stopped",
        Duration::from_secs(5),
    );
    assert!(
        ok,
        "session/close should stop the underlying worker, got status: {}",
        common::session_status(state_dir.path(), &session_id)
    );

    // A subsequent prompt against the now-closed session id is rejected
    // locally by this connection, without even reaching the daemon.
    conn.send(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "still there?"}],
        },
    }));
    let response = recv_response(&conn, 3);
    assert_eq!(response["error"]["code"], -32602);

    common::daemon_shutdown(state_dir.path());
    let _ = conn.close();
}

#[test]
fn acp_cancel_notification_gets_no_response_and_does_not_break_the_connection() {
    let state_dir = common::TempDir::new("acp-cancel");
    common::daemon_start(state_dir.path());
    let mut conn = AcpConn::spawn(state_dir.path());
    send_initialize(&mut conn);
    let session_id = new_session(&mut conn, 1);

    // Sent with no `id` -- a notification, so no response of any kind
    // (success or error) should ever arrive for this specific message.
    conn.send(serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": {"sessionId": session_id},
    }));

    // The connection still works normally afterwards -- proves the
    // notification didn't wedge or crash anything.
    conn.send(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "still working"}],
        },
    }));
    let response = recv_response(&conn, 2);
    assert_eq!(response["result"], serde_json::json!({}));

    common::daemon_shutdown(state_dir.path());
    let _ = conn.close();
}

#[test]
fn acp_unknown_method_reports_a_json_rpc_error() {
    let state_dir = common::TempDir::new("acp-unknown-method");
    common::daemon_start(state_dir.path());
    let mut conn = AcpConn::spawn(state_dir.path());
    send_initialize(&mut conn);

    conn.send(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/list",
        "params": {},
    }));
    let response = recv_response(&conn, 1);
    assert_eq!(response["error"]["code"], -32601);

    common::daemon_shutdown(state_dir.path());
    let _ = conn.close();
}
