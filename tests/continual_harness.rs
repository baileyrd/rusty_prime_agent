//! Parity with `prime-agent`'s Continual Harness (`/refine`) -- see
//! `client::session_refine`'s own doc comment for why the review prompt
//! is a regular, visible `SessionPrompt` turn rather than a hidden
//! side-channel call. Uses `EchoProvider` throughout, so a refine's
//! "proposal" is just `echo: <review prompt>` -- the mechanics under
//! test are storage/history/rollback, not semantic quality.

mod common;

#[test]
fn harness_list_is_empty_initially() {
    let state_dir = common::TempDir::new("harness-empty");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = common::run(
        state_dir.path(),
        &["session", "harness", "list", &session_id],
    );
    common::assert_success("session harness list", &out);
    assert_eq!(common::stdout_string(&out), "no harness notes\nhistory=0");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn harness_add_records_notes_and_history() {
    let state_dir = common::TempDir::new("harness-add");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = common::run(
        state_dir.path(),
        &[
            "session",
            "harness",
            "add",
            &session_id,
            "memory",
            "remember",
            "this",
        ],
    );
    common::assert_success("session harness add (memory)", &out);
    let stdout = common::stdout_string(&out);
    assert!(stdout.contains("memory\tremember this"), "got: {stdout}");
    assert!(stdout.ends_with("history=1"), "got: {stdout}");

    let out = common::run(
        state_dir.path(),
        &[
            "session",
            "harness",
            "add",
            &session_id,
            "skill",
            "how to do the thing",
        ],
    );
    common::assert_success("session harness add (skill)", &out);
    let stdout = common::stdout_string(&out);
    assert!(stdout.contains("memory\tremember this"), "got: {stdout}");
    assert!(
        stdout.contains("skill\thow to do the thing"),
        "got: {stdout}"
    );
    assert!(stdout.ends_with("history=2"), "got: {stdout}");

    let out = common::run(
        state_dir.path(),
        &["session", "harness", "list", &session_id],
    );
    common::assert_success("session harness list", &out);
    assert_eq!(common::stdout_string(&out), stdout);

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn harness_rollback_restores_an_earlier_recorded_version() {
    let state_dir = common::TempDir::new("harness-rollback");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    common::assert_success(
        "session harness add (first)",
        &common::run(
            state_dir.path(),
            &["session", "harness", "add", &session_id, "memory", "first"],
        ),
    );
    common::assert_success(
        "session harness add (second)",
        &common::run(
            state_dir.path(),
            &["session", "harness", "add", &session_id, "memory", "second"],
        ),
    );

    let out = common::run(
        state_dir.path(),
        &["session", "harness", "list", &session_id],
    );
    common::assert_success("session harness list", &out);
    let stdout = common::stdout_string(&out);
    assert!(stdout.contains("first"), "got: {stdout}");
    assert!(stdout.contains("second"), "got: {stdout}");
    assert!(stdout.ends_with("history=2"), "got: {stdout}");

    // history[0] is the state right after adding "first" -- rolling back
    // to it must remove "second" again, while still recording the
    // rollback itself as a fresh history entry.
    let out = common::run(
        state_dir.path(),
        &["session", "harness", "rollback", &session_id, "0"],
    );
    common::assert_success("session harness rollback", &out);
    let stdout = common::stdout_string(&out);
    assert!(stdout.contains("first"), "got: {stdout}");
    assert!(!stdout.contains("second"), "got: {stdout}");
    assert!(stdout.ends_with("history=3"), "got: {stdout}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn harness_rollback_to_an_unknown_index_is_a_conflict() {
    let state_dir = common::TempDir::new("harness-rollback-unknown");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);

    let out = common::run(
        state_dir.path(),
        &["session", "harness", "rollback", &session_id, "5"],
    );
    assert!(!out.status.success());
    assert_eq!(
        out.status.code(),
        Some(3),
        "an out-of-range history index is a conflict, not a usage error"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no history entry at index 5"),
        "got: {stderr}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn session_refine_reviews_the_trajectory_and_records_a_memory_note() {
    let state_dir = common::TempDir::new("harness-refine");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session_id, "hello there");

    let out = common::run(state_dir.path(), &["session", "refine", &session_id]);
    common::assert_success("session refine", &out);
    let stdout = common::stdout_string(&out);
    assert_eq!(stdout, "refine: added memory note (notes=1, history=1)");

    let out = common::run(
        state_dir.path(),
        &["session", "harness", "list", &session_id],
    );
    common::assert_success("session harness list", &out);
    let stdout = common::stdout_string(&out);
    assert!(
        stdout.contains("memory\techo: Review the following trajectory"),
        "got: {stdout}"
    );
    assert!(stdout.ends_with("history=1"), "got: {stdout}");

    // The review prompt itself went through as an ordinary SessionPrompt:
    // 1 turn from "hello there" + 1 turn from the refine review = 4
    // transcript entries (2 user+assistant pairs).
    let listing = common::session_list(state_dir.path());
    assert!(listing.contains("turns=4"), "got: {listing}");

    common::daemon_shutdown(state_dir.path());
}
