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
    // Parity with prime-agent's `agents`/`list` surface, which shows
    // each agent's worker process and generation.
    assert!(
        listing.contains("generation=1"),
        "a freshly created session's worker is generation 1, got: {listing}"
    );
    let worker_pid = common::worker_pid(state_dir.path(), &session_id);
    assert!(
        listing.contains(&format!("worker_pid={worker_pid}")),
        "session list should show the live worker's pid ({worker_pid}), got: {listing}"
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
fn session_stop_shuts_down_one_worker_without_touching_others() {
    // Parity with `prime-agent stop <agent>`: stopping one session must
    // not require (or trigger) a full `daemon shutdown`, and must leave
    // every other session's worker untouched.
    let state_dir = common::TempDir::new("session-stop");
    common::daemon_start(state_dir.path());

    let stopped_id = common::session_new(state_dir.path(), Some("to-be-stopped"));
    let survivor_id = common::session_new(state_dir.path(), Some("survivor"));
    common::session_prompt(state_dir.path(), &stopped_id, "before stop");

    let status = common::daemon_status(state_dir.path());
    assert!(
        status.contains("sessions_active=2"),
        "expected two active sessions before stopping either, got: {status}"
    );

    let ack = common::session_stop(state_dir.path(), &stopped_id);
    assert!(
        ack.contains("session stopped"),
        "stopping a live session should not report it as already stopped, got: {ack}"
    );

    assert!(
        common::wait_until(
            || common::session_status(state_dir.path(), &stopped_id) == "stopped",
            Duration::from_secs(5)
        ),
        "session should read back as stopped in state.json after `session stop`"
    );

    let status = common::daemon_status(state_dir.path());
    assert!(
        status.contains("sessions_active=1"),
        "the survivor session must still be counted active, got: {status}"
    );
    assert_eq!(
        common::session_status(state_dir.path(), &survivor_id),
        "active",
        "stopping one session must not affect a different session's status"
    );

    // Idempotent: stopping an already-stopped session is still a
    // success, just reported as a no-op rather than a fresh shutdown.
    let ack = common::session_stop(state_dir.path(), &stopped_id);
    assert!(
        ack.contains("already stopped"),
        "stopping an already-stopped session should say so, got: {ack}"
    );

    // A prompt against a stopped session must still work -- it resumes
    // (not recovers/crash-replays) a fresh worker, the same as any other
    // on-demand respawn.
    let ack = common::session_prompt(state_dir.path(), &stopped_id, "after stop");
    assert!(
        ack.contains("echo: after stop"),
        "prompting a stopped session should transparently resume it, got: {ack}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn session_rename_updates_the_listed_name_and_survives_a_respawn() {
    // Parity with `prime-agent rename <agent> <name>`.
    let state_dir = common::TempDir::new("session-rename");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), Some("original-name"));
    let listing = common::session_list(state_dir.path());
    assert!(
        listing.contains("original-name"),
        "session list should show the initial name, got: {listing}"
    );

    let out = common::run(
        state_dir.path(),
        &["session", "rename", &session_id, "renamed"],
    );
    common::assert_success("session rename", &out);
    assert!(
        common::stdout_string(&out).contains("renamed to renamed"),
        "rename should echo the new name, got: {}",
        common::stdout_string(&out)
    );

    let listing = common::session_list(state_dir.path());
    assert!(
        listing.contains("renamed") && !listing.contains("original-name"),
        "session list should reflect the new name, got: {listing}"
    );

    // The rename must be durable, not just an in-memory change on the
    // still-running worker: stop the session (tearing the worker down),
    // then resume it via a prompt and confirm the name survived the
    // respawn from `state.json`.
    common::session_stop(state_dir.path(), &session_id);
    common::session_prompt(state_dir.path(), &session_id, "after rename and respawn");
    let listing = common::session_list(state_dir.path());
    assert!(
        listing.contains("renamed"),
        "renamed name should survive a worker stop/respawn, got: {listing}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn mode_json_emits_raw_response_and_event_lines() {
    // Parity with `prime-agent --mode json`.
    let state_dir = common::TempDir::new("mode-json");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), Some("json-mode-test"));
    common::session_prompt(state_dir.path(), &session_id, "hello json");

    let out = common::run(state_dir.path(), &["--mode", "json", "session", "list"]);
    common::assert_success("session list --mode json", &out);
    let stdout = common::stdout_string(&out);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("not valid JSON: {e}, got: {stdout}"));
    assert_eq!(
        parsed["type"], "session_list",
        "session list --mode json should emit the raw Response, got: {stdout}"
    );
    assert_eq!(
        parsed["sessions"][0]["session_id"],
        serde_json::Value::String(session_id.clone()),
        "got: {stdout}"
    );

    let out = common::run(state_dir.path(), &["--mode", "json", "daemon", "status"]);
    common::assert_success("daemon status --mode json", &out);
    let stdout = common::stdout_string(&out);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("not valid JSON: {e}, got: {stdout}"));
    assert_eq!(parsed["type"], "daemon_status", "got: {stdout}");
    assert_eq!(parsed["sessions_active"], 1, "got: {stdout}");

    // Attach in JSON mode: every line, including the initial
    // `SessionAttachStarted` response, should be a standalone JSON
    // object -- not this project's own human-readable rendering.
    let lines = common::attach_lines_with_args(
        state_dir.path(),
        &["--mode", "json", "session", "attach", &session_id],
        2,
        Duration::from_secs(5),
    );
    assert_eq!(lines.len(), 2, "expected 2 JSON lines, got: {lines:?}");
    let started: serde_json::Value = serde_json::from_str(&lines[0])
        .unwrap_or_else(|e| panic!("line 0 not valid JSON: {e}, got: {}", lines[0]));
    assert_eq!(started["type"], "session_attach_started", "got: {lines:?}");
    let snapshot: serde_json::Value = serde_json::from_str(&lines[1])
        .unwrap_or_else(|e| panic!("line 1 not valid JSON: {e}, got: {}", lines[1]));
    assert_eq!(snapshot["kind"], "snapshot", "got: {lines:?}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn print_mode_starts_a_daemon_and_prints_just_the_reply() {
    // Parity with `prime-agent -p`: unlike every other subcommand, this
    // must work without a prior `daemon start` -- it starts one
    // transparently -- and its output is just the reply text, no
    // session id, no daemon-startup noise, no `[seq] role:` prefix.
    let state_dir = common::TempDir::new("print-mode");

    let out = common::run(state_dir.path(), &["-p", "hello", "from", "print", "mode"]);
    common::assert_success("-p", &out);
    assert_eq!(
        common::stdout_string(&out),
        "echo: hello from print mode",
        "print mode should output only the reply text"
    );

    // The session it created is not ephemeral -- it stays listed, same
    // as a `session new`-created one.
    let listing = common::session_list(state_dir.path());
    assert!(
        listing.contains("active"),
        "the session print mode created should still be listed, got: {listing}"
    );

    // A daemon is now running (print mode started it); a second
    // invocation must reuse it rather than erroring or double-spawning.
    let out = common::run(state_dir.path(), &["--print", "second", "call"]);
    common::assert_success("--print", &out);
    assert_eq!(common::stdout_string(&out), "echo: second call");

    common::daemon_shutdown(state_dir.path());
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
