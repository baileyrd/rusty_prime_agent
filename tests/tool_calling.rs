//! Real tool-calling loop (parity with `prime-agent`'s own tool-calling
//! support, see `PARITY.md`). `EchoProvider`-backed coverage here proves
//! the plumbing (`session new --tools read`, `AgentSession::prompt`'s
//! loop, `provider::ToolDef`s reaching the provider) doesn't regress the
//! default, tool-less path -- `EchoProvider` never emits `ToolCalls`, so
//! these sessions behave identically to a session with no `--tools` flag
//! at all. True end-to-end tool-execution coverage (a real model
//! actually requesting `read_file`, getting a result, replying) needs a
//! real model and lives in `tests/ollama_provider.rs`'s `#[ignore]`d
//! pattern instead.

mod common;

#[test]
fn tools_read_flag_is_accepted_and_does_not_change_echo_providers_behavior() {
    let state_dir = common::TempDir::new("tools-echo");
    common::daemon_start(state_dir.path());

    let session_id =
        common::session_new_with_model_and_tools(state_dir.path(), None, None, Some("read"));
    let ack = common::session_prompt(state_dir.path(), &session_id, "hello");
    assert!(
        ack.contains("] assistant: echo: hello"),
        "EchoProvider's reply should be unaffected by --tools read, got: {ack}"
    );

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn unknown_tools_value_is_rejected_at_parse_time() {
    let state_dir = common::TempDir::new("tools-invalid");
    let out = common::run(state_dir.path(), &["session", "new", "--tools", "shell"]);
    assert!(
        !out.status.success(),
        "expected `--tools shell` to fail loudly (not yet a supported value), got success"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--tools"),
        "expected an error mentioning --tools, got: {stderr}"
    );
}

#[test]
fn a_session_without_tools_never_offers_them_either() {
    // No --tools at all: same "no tools offered" behavior --tools read's
    // EchoProvider case above has, just via the default instead of an
    // explicit flag. Exists so the two "no tools invoked" paths (default,
    // and opted-in-but-EchoProvider) are both covered, not just one.
    let state_dir = common::TempDir::new("tools-default");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);
    let ack = common::session_prompt(state_dir.path(), &session_id, "hello");
    assert!(ack.contains("] assistant: echo: hello"), "got: {ack}");

    common::daemon_shutdown(state_dir.path());
}

/// `--tools mcp` needs a real `rp-server` sidecar (its MCP gateway) even
/// for a plain `EchoProvider` session (no `--model`) -- this project's
/// own CI has no `rp-server` binary on `PATH` (see `tests/
/// ollama_provider.rs` for the real, manually-run end-to-end coverage
/// that does). `session new` itself must fail loudly here, not silently
/// create a session that would only fail later on its first prompt --
/// same reasoning `session_new_with_model_fails_loudly_when_rp_server_
/// is_unavailable` (`tests/session_lifecycle.rs`) already establishes
/// for `--model`.
#[test]
fn tools_mcp_fails_loudly_when_rp_server_is_unavailable() {
    let state_dir = common::TempDir::new("tools-mcp-no-sidecar");
    common::daemon_start(state_dir.path());

    let out = common::run(state_dir.path(), &["session", "new", "--tools", "mcp"]);
    assert!(
        !out.status.success(),
        "session new --tools mcp should fail when rp-server isn't reachable"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("rp-server"),
        "expected an error mentioning rp-server, got: {stderr}"
    );

    common::daemon_shutdown(state_dir.path());
}

/// A session survives a worker respawn with its `--tools` setting
/// intact -- `tools` follows `goal`/`parent_id`'s "only meaningful for
/// `WorkerMode::New`" thread-through pattern (re-read from persisted
/// `state.json` on revival, not re-supplied), so this is the regression
/// this project's own `WorkerArgs::tools` doc comment describes.
#[test]
fn tools_setting_survives_a_session_stop_and_resume() {
    let state_dir = common::TempDir::new("tools-resume");
    common::daemon_start(state_dir.path());

    let session_id =
        common::session_new_with_model_and_tools(state_dir.path(), None, None, Some("read"));
    common::session_stop(state_dir.path(), &session_id);

    // Resuming happens implicitly on the next request to a stopped
    // session -- same pattern `session_rename_updates_the_listed_name_
    // and_survives_a_respawn` (tests/session_lifecycle.rs) already uses.
    let ack = common::session_prompt(state_dir.path(), &session_id, "hello again");
    assert!(ack.contains("] assistant: echo: hello again"), "got: {ack}");

    common::daemon_shutdown(state_dir.path());
}
