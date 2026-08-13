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

/// `daemon::handle_session_new`'s recursion-depth computation, parity
/// with `rlm-runtime.md`: a root session gets `RLM_DEPTH=0` and
/// `RLM_MAX_DEPTH` from `RUSTY_PRIME_AGENT_RLM_MAX_DEPTH` (default `1`
/// otherwise); a child gets `parent.RLM_DEPTH + 1` and *inherits* the
/// parent's own `RLM_MAX_DEPTH` unchanged, not a freshly-resolved one --
/// proven here by setting the env var only on the root's own daemon and
/// spawning two generations of children from it. `session spawn` reuses
/// the exact same `SessionNew` daemon round trip the kernel-side
/// `rlm(...)` does (`session::handle_rlm_run`'s own doc comment), so this
/// proves the shared daemon-side computation without needing a real
/// kernel.
#[test]
fn session_spawn_inherits_the_parents_max_depth_and_increments_depth_by_one() {
    let state_dir = common::TempDir::new("subagents-rlm-depth");
    common::daemon_start_with_env(
        state_dir.path(),
        &[("RUSTY_PRIME_AGENT_RLM_MAX_DEPTH", "3")],
    );

    let root_id = common::session_new(state_dir.path(), None);
    let (root_depth, root_max_depth) = common::session_rlm_depth(state_dir.path(), &root_id);
    assert_eq!(root_depth, 0, "a root session starts at depth 0");
    assert_eq!(
        root_max_depth, 3,
        "a root session's max depth comes from RUSTY_PRIME_AGENT_RLM_MAX_DEPTH"
    );

    let out = common::run(
        state_dir.path(),
        &["session", "spawn", &root_id, "child", "task"],
    );
    common::assert_success("session spawn (child)", &out);
    let child_id = common::stdout_string(&out);
    let (child_depth, child_max_depth) = common::session_rlm_depth(state_dir.path(), &child_id);
    assert_eq!(child_depth, 1, "a child is one deeper than its parent");
    assert_eq!(
        child_max_depth, 3,
        "a child inherits the parent's max depth unchanged"
    );

    let out = common::run(
        state_dir.path(),
        &["session", "spawn", &child_id, "grandchild", "task"],
    );
    common::assert_success("session spawn (grandchild)", &out);
    let grandchild_id = common::stdout_string(&out);
    let (grandchild_depth, grandchild_max_depth) =
        common::session_rlm_depth(state_dir.path(), &grandchild_id);
    assert_eq!(
        grandchild_depth, 2,
        "depth keeps incrementing per generation"
    );
    assert_eq!(
        grandchild_max_depth, 3,
        "max depth keeps propagating unchanged, not re-resolved"
    );

    common::daemon_shutdown(state_dir.path());
}

/// No `RUSTY_PRIME_AGENT_RLM_MAX_DEPTH` set at all: a root session falls
/// back to `session::DEFAULT_RLM_MAX_DEPTH` (`1`), matching
/// `rlm-runtime.md`'s own stated default of "root sessions may create
/// children; children may not create grandchildren unless configured
/// higher".
#[test]
fn session_new_defaults_to_max_depth_one_with_no_env_var_set() {
    let state_dir = common::TempDir::new("subagents-rlm-depth-default");
    common::daemon_start(state_dir.path());

    let root_id = common::session_new(state_dir.path(), None);
    let (root_depth, root_max_depth) = common::session_rlm_depth(state_dir.path(), &root_id);
    assert_eq!(root_depth, 0);
    assert_eq!(root_max_depth, 1);

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
