//! Parity with `prime-agent --goal`/`/goal` -- see `protocol::GoalState`'s
//! own doc comment for what this durable state is (and isn't: no
//! autonomous continuation policy reads it yet). Uses `EchoProvider`
//! throughout; goal mutation never touches the model.

mod common;

#[test]
fn goal_set_show_and_clear_round_trip() {
    let state_dir = common::TempDir::new("goal-set-show-clear");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);

    let out = common::run(
        state_dir.path(),
        &["session", "goal", "show", &session_id],
    );
    common::assert_success("session goal show (none yet)", &out);
    assert_eq!(common::stdout_string(&out), "no goal");

    let out = common::run(
        state_dir.path(),
        &[
            "session", "goal", "set", &session_id, "ship", "the", "thing",
        ],
    );
    common::assert_success("session goal set", &out);
    assert_eq!(common::stdout_string(&out), "active\tship the thing");

    let out = common::run(
        state_dir.path(),
        &["session", "goal", "show", &session_id],
    );
    common::assert_success("session goal show", &out);
    assert_eq!(common::stdout_string(&out), "active\tship the thing");

    let out = common::run(
        state_dir.path(),
        &["session", "goal", "clear", &session_id],
    );
    common::assert_success("session goal clear", &out);
    assert_eq!(common::stdout_string(&out), "no goal");

    let out = common::run(
        state_dir.path(),
        &["session", "goal", "show", &session_id],
    );
    common::assert_success("session goal show (after clear)", &out);
    assert_eq!(common::stdout_string(&out), "no goal");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn goal_pause_resume_and_complete_transition_status() {
    let state_dir = common::TempDir::new("goal-transitions");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);

    let out = common::run(
        state_dir.path(),
        &["session", "goal", "set", &session_id, "write", "tests"],
    );
    common::assert_success("session goal set", &out);
    assert_eq!(common::stdout_string(&out), "active\twrite tests");

    let out = common::run(
        state_dir.path(),
        &["session", "goal", "pause", &session_id],
    );
    common::assert_success("session goal pause", &out);
    assert_eq!(common::stdout_string(&out), "paused\twrite tests");

    let out = common::run(
        state_dir.path(),
        &["session", "goal", "resume", &session_id],
    );
    common::assert_success("session goal resume", &out);
    assert_eq!(common::stdout_string(&out), "active\twrite tests");

    let out = common::run(
        state_dir.path(),
        &["session", "goal", "complete", &session_id],
    );
    common::assert_success("session goal complete", &out);
    assert_eq!(common::stdout_string(&out), "completed\twrite tests");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn goal_pause_resume_complete_are_noops_without_a_current_goal() {
    let state_dir = common::TempDir::new("goal-noop-transitions");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);

    for verb in ["pause", "resume", "complete"] {
        let out = common::run(state_dir.path(), &["session", "goal", verb, &session_id]);
        common::assert_success(&format!("session goal {verb}"), &out);
        assert_eq!(common::stdout_string(&out), "no goal");
    }

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn session_new_seeds_a_goal_via_flag() {
    let state_dir = common::TempDir::new("goal-seed-at-new");
    common::daemon_start(state_dir.path());

    let out = common::run(
        state_dir.path(),
        &["session", "new", "--goal", "seeded goal"],
    );
    common::assert_success("session new --goal", &out);
    let session_id = common::stdout_string(&out);
    assert!(!session_id.is_empty());

    let out = common::run(
        state_dir.path(),
        &["session", "goal", "show", &session_id],
    );
    common::assert_success("session goal show", &out);
    assert_eq!(common::stdout_string(&out), "active\tseeded goal");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn setting_a_new_goal_replaces_the_old_one() {
    let state_dir = common::TempDir::new("goal-replace");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);

    let out = common::run(
        state_dir.path(),
        &["session", "goal", "set", &session_id, "first", "goal"],
    );
    common::assert_success("session goal set (first)", &out);
    assert_eq!(common::stdout_string(&out), "active\tfirst goal");

    let out = common::run(
        state_dir.path(),
        &["session", "goal", "complete", &session_id],
    );
    common::assert_success("session goal complete", &out);
    assert_eq!(common::stdout_string(&out), "completed\tfirst goal");

    let out = common::run(
        state_dir.path(),
        &["session", "goal", "set", &session_id, "second", "goal"],
    );
    common::assert_success("session goal set (second)", &out);
    assert_eq!(
        common::stdout_string(&out),
        "active\tsecond goal",
        "setting a fresh goal always starts `Active`, even over a completed one"
    );

    common::daemon_shutdown(state_dir.path());
}
