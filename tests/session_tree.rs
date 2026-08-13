//! `session tree <id>` / `session set-active-leaf <id> <sequence>` --
//! bounded parity with `prime-agent`'s `/tree`: the display and
//! navigation halves that sit on top of the active-leaf transcript model
//! (`protocol::TranscriptEntry::parent_sequence`/`SessionState::
//! active_leaf_sequence`, see that increment's own `PARITY.md` entry).
//! Also covers `session branch-summary <id> <sequence>` (parity with
//! `session-format.md`'s `BranchSummaryEntry`) -- CI-safe no-op coverage
//! only here (no model configured, unknown sequence, sequence already on
//! the active chain); the real-summarization half needs a real model, so
//! it's an `#[ignore]`d test in `tests/ollama_provider.rs` instead, the
//! same split `compact_now`'s own coverage uses. `/clone`'s live-state
//! duplication stays out of scope, see `tests/session_fork.rs`'s own
//! doc comment.

mod common;

fn session_tree_json(state_dir: &std::path::Path, session_id: &str) -> serde_json::Value {
    let out = common::run(
        state_dir,
        &["--mode", "json", "session", "tree", session_id],
    );
    common::assert_success("session tree --mode json", &out);
    let stdout = common::stdout_string(&out);
    serde_json::from_str(&stdout).expect("session tree --mode json should emit valid JSON")
}

fn set_active_leaf(state_dir: &std::path::Path, session_id: &str, sequence: u64) -> String {
    let sequence_str = sequence.to_string();
    let out = common::run(
        state_dir,
        &["session", "set-active-leaf", session_id, &sequence_str],
    );
    common::assert_success("session set-active-leaf", &out);
    common::stdout_string(&out)
}

fn entry_parent_sequence(tree: &serde_json::Value, sequence: u64) -> Option<u64> {
    tree["transcript"]
        .as_array()
        .expect("transcript array")
        .iter()
        .find(|e| e["sequence"] == sequence)
        .expect("entry present")["parent_sequence"]
        .as_u64()
}

#[test]
fn ordinary_prompting_reports_a_linear_chain_and_the_current_active_leaf() {
    let state_dir = common::TempDir::new("tree-linear");
    common::daemon_start(state_dir.path());
    let session = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session, "hello");
    // Two turns now: 1 (user), 2 (assistant).

    let tree = session_tree_json(state_dir.path(), &session);
    assert_eq!(tree["active_leaf_sequence"], 2);
    assert_eq!(entry_parent_sequence(&tree, 1), None);
    assert_eq!(entry_parent_sequence(&tree, 2), Some(1));

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn text_mode_marks_the_active_leaf() {
    let state_dir = common::TempDir::new("tree-text-marker");
    common::daemon_start(state_dir.path());
    let session = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session, "hello");

    let out = common::run(state_dir.path(), &["session", "tree", &session]);
    common::assert_success("session tree", &out);
    let stdout = common::stdout_string(&out);
    assert!(
        stdout.contains("[2]") && stdout.contains("(active)"),
        "got: {stdout}"
    );
    assert!(
        !stdout
            .lines()
            .any(|l| l.contains("[1]") && l.contains("(active)")),
        "got: {stdout}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn set_active_leaf_followed_by_a_prompt_creates_a_real_fork() {
    let state_dir = common::TempDir::new("tree-fork");
    common::daemon_start(state_dir.path());
    let session = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session, "hello");
    // 1 (user), 2 (assistant) -- the original branch.

    let ack = set_active_leaf(state_dir.path(), &session, 1);
    assert!(ack.contains('1'), "got: {ack}");

    common::session_prompt(state_dir.path(), &session, "again");
    // The next append (3) should now be a second child of 1, not a
    // continuation of 2 -- a genuine fork, not a linear extension.

    let tree = session_tree_json(state_dir.path(), &session);
    assert_eq!(entry_parent_sequence(&tree, 2), Some(1));
    assert_eq!(entry_parent_sequence(&tree, 3), Some(1));
    // The active chain now resolves through the new branch (3, 4), not
    // the old one (2) -- `active_leaf_sequence` points at the newest
    // entry appended down the redirected path.
    assert_eq!(tree["active_leaf_sequence"], 4);
    // Both branches still show up in the full transcript -- the fork
    // doesn't delete or hide the old one.
    assert_eq!(tree["transcript"].as_array().unwrap().len(), 4);

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn set_active_leaf_with_an_unknown_sequence_is_a_conflict() {
    let state_dir = common::TempDir::new("tree-unknown-leaf");
    common::daemon_start(state_dir.path());
    let session = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session, "hello");

    let out = common::run(
        state_dir.path(),
        &["session", "set-active-leaf", &session, "999"],
    );
    assert!(
        !out.status.success(),
        "expected a failure exit code, got success: {}",
        common::stdout_string(&out)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("999"), "got: {stderr}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn tree_on_an_empty_session_reports_no_turns_yet() {
    let state_dir = common::TempDir::new("tree-empty");
    common::daemon_start(state_dir.path());
    let session = common::session_new(state_dir.path(), None);

    let out = common::run(state_dir.path(), &["session", "tree", &session]);
    common::assert_success("session tree", &out);
    let stdout = common::stdout_string(&out);
    assert!(stdout.contains("no turns yet"), "got: {stdout}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn branch_summary_on_a_session_with_no_model_is_a_no_op() {
    let state_dir = common::TempDir::new("branch-summary-no-model");
    common::daemon_start(state_dir.path());
    let session = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session, "hello");
    set_active_leaf(state_dir.path(), &session, 1);
    common::session_prompt(state_dir.path(), &session, "again");

    let out = common::run(
        state_dir.path(),
        &["session", "branch-summary", &session, "2"],
    );
    common::assert_success("session branch-summary", &out);
    assert_eq!(
        common::stdout_string(&out).trim(),
        "nothing to summarize (no model configured, or that sequence is already \
         part of the active branch)"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn branch_summary_json_mode_reports_summarized_false_and_no_summary() {
    let state_dir = common::TempDir::new("branch-summary-json-no-model");
    common::daemon_start(state_dir.path());
    let session = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session, "hello");
    set_active_leaf(state_dir.path(), &session, 1);
    common::session_prompt(state_dir.path(), &session, "again");

    let out = common::run(
        state_dir.path(),
        &["--mode", "json", "session", "branch-summary", &session, "2"],
    );
    common::assert_success("session branch-summary --mode json", &out);
    let stdout = common::stdout_string(&out);
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["summarized"], false);
    assert!(value["summary"].is_null());

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn branch_summary_of_a_sequence_on_the_active_chain_is_a_no_op() {
    let state_dir = common::TempDir::new("branch-summary-active-chain");
    common::daemon_start(state_dir.path());
    let session = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session, "hello");

    let out = common::run(
        state_dir.path(),
        &["session", "branch-summary", &session, "1"],
    );
    common::assert_success("session branch-summary", &out);
    assert!(
        common::stdout_string(&out).contains("nothing to summarize"),
        "got: {}",
        common::stdout_string(&out)
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn branch_summary_rejects_an_unknown_sequence() {
    let state_dir = common::TempDir::new("branch-summary-unknown");
    common::daemon_start(state_dir.path());
    let session = common::session_new(state_dir.path(), None);
    common::session_prompt(state_dir.path(), &session, "hello");

    let out = common::run(
        state_dir.path(),
        &["session", "branch-summary", &session, "999"],
    );
    assert!(
        !out.status.success(),
        "expected a failure exit code, got success: {}",
        common::stdout_string(&out)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("999"), "got: {stderr}");

    common::daemon_shutdown(state_dir.path());
}
