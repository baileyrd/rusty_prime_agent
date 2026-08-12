//! Bounded parity with `prime-agent /autonomous` -- see `client::
//! session_autonomous`'s own doc comment for why this is a one-shot
//! foreground CLI loop rather than a background daemon-side one. Uses
//! `EchoProvider` throughout, plus a portable `exit 0`/`exit 1` shell
//! command for the quality-gate cases (valid under both `sh -c` and
//! `cmd /C`, so it works across this project's `ubuntu`/`macos`/
//! `windows-latest` CI matrix).

mod common;

fn turns(state_dir: &std::path::Path) -> u64 {
    let listing = common::session_list(state_dir);
    listing
        .split('\t')
        .find_map(|f| f.strip_prefix("turns="))
        .and_then(|n| n.parse::<u64>().ok())
        .expect("session list should report turns=")
}

#[test]
fn autonomous_requires_an_active_goal() {
    let state_dir = common::TempDir::new("autonomous-no-goal");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);

    let out = common::run(
        state_dir.path(),
        &["session", "autonomous", &session_id, "--max-turns", "3"],
    );
    assert!(
        !out.status.success(),
        "autonomous run without a goal should fail"
    );
    assert_eq!(
        out.status.code(),
        Some(3),
        "a missing goal is a conflict, not a usage error"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("requires an active goal"), "got: {stderr}");

    // A paused goal is not active either.
    let out = common::run(
        state_dir.path(),
        &["session", "goal", "set", &session_id, "get", "it", "done"],
    );
    common::assert_success("session goal set", &out);
    let out = common::run(state_dir.path(), &["session", "goal", "pause", &session_id]);
    common::assert_success("session goal pause", &out);

    let out = common::run(
        state_dir.path(),
        &["session", "autonomous", &session_id, "--max-turns", "3"],
    );
    assert!(
        !out.status.success(),
        "autonomous run against a paused goal should fail"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn autonomous_stops_at_max_turns_and_leaves_the_goal_active() {
    let state_dir = common::TempDir::new("autonomous-max-turns");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);
    let out = common::run(
        state_dir.path(),
        &["session", "goal", "set", &session_id, "get", "it", "done"],
    );
    common::assert_success("session goal set", &out);

    let out = common::run(
        state_dir.path(),
        &["session", "autonomous", &session_id, "--max-turns", "3"],
    );
    common::assert_success("session autonomous", &out);
    let stdout = common::stdout_string(&out);
    assert_eq!(
        stdout,
        "autonomous run stopped: max turns reached (turns=3)"
    );

    // 3 continuation prompts = 3 user+assistant pairs = 6 transcript turns.
    assert_eq!(turns(state_dir.path()), 6);

    let out = common::run(state_dir.path(), &["session", "goal", "show", &session_id]);
    common::assert_success("session goal show", &out);
    assert_eq!(
        common::stdout_string(&out),
        "active\tget it done",
        "exhausting the turn budget must not itself complete the goal"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn autonomous_quality_gate_passing_immediately_completes_the_goal_with_zero_turns() {
    let state_dir = common::TempDir::new("autonomous-gate-immediate");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);
    let out = common::run(
        state_dir.path(),
        &["session", "goal", "set", &session_id, "already", "done"],
    );
    common::assert_success("session goal set", &out);

    let out = common::run(
        state_dir.path(),
        &[
            "session",
            "autonomous",
            &session_id,
            "--max-turns",
            "5",
            "--quality-gate",
            "exit 0",
        ],
    );
    common::assert_success("session autonomous", &out);
    assert_eq!(
        common::stdout_string(&out),
        "autonomous run stopped: quality gate passed (turns=0)"
    );

    // No continuation prompt was ever sent.
    assert_eq!(turns(state_dir.path()), 0);

    let out = common::run(state_dir.path(), &["session", "goal", "show", &session_id]);
    common::assert_success("session goal show", &out);
    assert_eq!(common::stdout_string(&out), "completed\talready done");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn autonomous_with_a_never_passing_gate_still_stops_at_max_turns() {
    let state_dir = common::TempDir::new("autonomous-gate-never");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);
    let out = common::run(
        state_dir.path(),
        &["session", "goal", "set", &session_id, "keep", "trying"],
    );
    common::assert_success("session goal set", &out);

    let out = common::run(
        state_dir.path(),
        &[
            "session",
            "autonomous",
            &session_id,
            "--max-turns",
            "2",
            "--quality-gate",
            "exit 1",
        ],
    );
    common::assert_success("session autonomous", &out);
    assert_eq!(
        common::stdout_string(&out),
        "autonomous run stopped: max turns reached (turns=2)"
    );
    assert_eq!(turns(state_dir.path()), 4);

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn autonomous_stops_at_max_time_before_exhausting_a_large_turn_budget() {
    let state_dir = common::TempDir::new("autonomous-max-time");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);
    let out = common::run(
        state_dir.path(),
        &["session", "goal", "set", &session_id, "keep", "going"],
    );
    common::assert_success("session goal set", &out);

    let started = std::time::Instant::now();
    let out = common::run(
        state_dir.path(),
        &[
            "session",
            "autonomous",
            &session_id,
            "--max-turns",
            "1000000",
            "--max-time",
            "1s",
        ],
    );
    common::assert_success("session autonomous", &out);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "the 1s time budget should have cut the run short, took {:?}",
        started.elapsed()
    );
    assert!(
        common::stdout_string(&out).starts_with("autonomous run stopped: max time reached"),
        "got: {}",
        common::stdout_string(&out)
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn autonomous_json_mode_emits_a_structured_summary() {
    let state_dir = common::TempDir::new("autonomous-json");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);
    let out = common::run(
        state_dir.path(),
        &["session", "goal", "set", &session_id, "ship", "it"],
    );
    common::assert_success("session goal set", &out);

    let out = common::run(
        state_dir.path(),
        &[
            "--mode",
            "json",
            "session",
            "autonomous",
            &session_id,
            "--max-turns",
            "1",
        ],
    );
    common::assert_success("session autonomous --mode json", &out);
    let stdout = common::stdout_string(&out);
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("expected one JSON line, got {stdout:?}: {e}"));
    assert_eq!(value["type"], "autonomous_stopped");
    assert_eq!(value["reason"], "max turns reached");
    assert_eq!(value["turns_used"], 1);

    common::daemon_shutdown(state_dir.path());
}
