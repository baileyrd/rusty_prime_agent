//! End-to-end coverage for the opt-in, local-only telemetry stub -- see
//! `telemetry`'s own module doc comment for the full design. Unlike
//! `src/telemetry.rs`'s own unit tests (which call `telemetry::record`
//! directly), this proves the real call sites (`AgentSession::create`/
//! `prompt_with_images`) actually fire, through a real daemon/worker,
//! with `EchoProvider` -- no real model needed.

mod common;

#[test]
fn telemetry_is_off_by_default_and_writes_nothing() {
    let state_dir = common::TempDir::new("telemetry-default-off");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "hello");

    assert!(
        !state_dir.path().join("telemetry.jsonl").exists(),
        "no telemetry.json should be written without an explicit opt-in"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn telemetry_explicitly_disabled_writes_nothing() {
    let state_dir = common::TempDir::new("telemetry-explicit-off");
    std::fs::write(
        state_dir.path().join("settings.json"),
        r#"{"telemetry_enabled": false}"#,
    )
    .unwrap();
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "hello");

    assert!(!state_dir.path().join("telemetry.jsonl").exists());

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn telemetry_enabled_records_session_created_and_prompt_events() {
    let state_dir = common::TempDir::new("telemetry-enabled");
    std::fs::write(
        state_dir.path().join("settings.json"),
        r#"{"telemetry_enabled": true}"#,
    )
    .unwrap();
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "hello");

    let contents = std::fs::read_to_string(state_dir.path().join("telemetry.jsonl"))
        .expect("telemetry.jsonl should have been written");
    let events: Vec<serde_json::Value> = contents
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();

    let created = events
        .iter()
        .find(|e| e["event"] == "session_created")
        .expect("a session_created event should have been recorded");
    assert_eq!(created["session_id"], session_id);
    assert!(created["ts_ms"].is_u64());

    let prompt = events
        .iter()
        .find(|e| e["event"] == "prompt")
        .expect("a prompt event should have been recorded");
    assert_eq!(prompt["session_id"], session_id);
    assert_eq!(prompt["ok"], true);
    assert_eq!(prompt["tool_rounds"], 1);

    common::daemon_shutdown(state_dir.path());
}
