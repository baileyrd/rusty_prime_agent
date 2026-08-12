//! Bounded, non-Python parity with `prime-agent`'s recursive subagents
//! (`rlm(...)`, `receiver_role="parent"/"child"`) -- see
//! `client::session_spawn`'s own doc comment for exactly how this maps
//! onto plain `session new` + `session schedule` instead of a Python/
//! IPython kernel. Uses `EchoProvider` throughout.

mod common;

use std::time::Duration;

#[test]
fn session_spawn_creates_a_child_and_processes_its_task_asynchronously() {
    let state_dir = common::TempDir::new("subagents-spawn");
    common::daemon_start(state_dir.path());

    let parent_id = common::session_new(state_dir.path(), None);

    let out = common::run(
        state_dir.path(),
        &["session", "spawn", &parent_id, "do", "the", "thing"],
    );
    common::assert_success("session spawn", &out);
    let child_id = common::stdout_string(&out);
    assert!(!child_id.is_empty());
    assert_ne!(child_id, parent_id);

    // "Returns immediately after task admission... never waits for or
    // returns the child's answer" -- the child's own turn count is still
    // 0 right after spawn returns, and only becomes 2 once the
    // background schedule-firing loop (up to SCHEDULE_POLL_INTERVAL,
    // 5s) has actually delivered the task.
    assert!(
        common::wait_until(
            || {
                let listing = common::session_list(state_dir.path());
                listing.contains(&child_id) && listing.contains("turns=2")
            },
            Duration::from_secs(15)
        ),
        "the spawned child's task should have been processed within 15s"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn session_children_lists_only_direct_children() {
    let state_dir = common::TempDir::new("subagents-children");
    common::daemon_start(state_dir.path());

    let parent_id = common::session_new(state_dir.path(), None);
    let unrelated_id = common::session_new(state_dir.path(), None);

    let out = common::run(state_dir.path(), &["session", "children", &parent_id]);
    common::assert_success("session children (none yet)", &out);
    assert_eq!(common::stdout_string(&out), "no children");

    let out = common::run(
        state_dir.path(),
        &["session", "spawn", &parent_id, "first", "child", "task"],
    );
    common::assert_success("session spawn (first)", &out);
    let child1 = common::stdout_string(&out);

    let out = common::run(
        state_dir.path(),
        &["session", "spawn", &parent_id, "second", "child", "task"],
    );
    common::assert_success("session spawn (second)", &out);
    let child2 = common::stdout_string(&out);

    let out = common::run(state_dir.path(), &["session", "children", &parent_id]);
    common::assert_success("session children", &out);
    let stdout = common::stdout_string(&out);
    assert!(stdout.contains(&child1), "got: {stdout}");
    assert!(stdout.contains(&child2), "got: {stdout}");
    assert!(!stdout.contains(&unrelated_id), "got: {stdout}");

    // A grandchild spawned from child1 is not child1's parent's own
    // direct child.
    let out = common::run(
        state_dir.path(),
        &["session", "spawn", &child1, "grandchild", "task"],
    );
    common::assert_success("session spawn (grandchild)", &out);
    let grandchild = common::stdout_string(&out);

    let out = common::run(state_dir.path(), &["session", "children", &parent_id]);
    common::assert_success("session children (after grandchild)", &out);
    assert!(
        !common::stdout_string(&out).contains(&grandchild),
        "got: {}",
        common::stdout_string(&out)
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn session_spawn_with_an_unknown_parent_is_a_conflict() {
    let state_dir = common::TempDir::new("subagents-unknown-parent");
    common::daemon_start(state_dir.path());

    let out = common::run(
        state_dir.path(),
        &["session", "spawn", "sess-does-not-exist", "some", "task"],
    );
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown parent session"), "got: {stderr}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn session_message_delivers_between_parent_and_child_but_not_unrelated_sessions() {
    let state_dir = common::TempDir::new("subagents-message");
    common::daemon_start(state_dir.path());

    let parent_id = common::session_new(state_dir.path(), None);
    let out = common::run(
        state_dir.path(),
        &["session", "spawn", &parent_id, "child", "task"],
    );
    common::assert_success("session spawn", &out);
    let child_id = common::stdout_string(&out);

    let unrelated_id = common::session_new(state_dir.path(), None);

    // parent -> child.
    let out = common::run(
        state_dir.path(),
        &[
            "session", "message", &parent_id, &child_id, "status", "update", "please",
        ],
    );
    common::assert_success("session message (parent -> child)", &out);
    assert!(
        common::stdout_string(&out).contains("echo: [from ")
            && common::stdout_string(&out).contains("status update please"),
        "got: {}",
        common::stdout_string(&out)
    );

    // child -> parent.
    let out = common::run(
        state_dir.path(),
        &["session", "message", &child_id, &parent_id, "done"],
    );
    common::assert_success("session message (child -> parent)", &out);

    // Unrelated sessions must not be able to message each other.
    let out = common::run(
        state_dir.path(),
        &["session", "message", &parent_id, &unrelated_id, "hello"],
    );
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("is neither the parent nor a child"),
        "got: {stderr}"
    );

    common::daemon_shutdown(state_dir.path());
}
