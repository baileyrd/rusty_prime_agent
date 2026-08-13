//! Real end-to-end coverage for `provider::RustyProviderModel`/`rp_server`
//! (parity with `prime-agent --provider`/`--model`, see `PARITY.md`'s
//! "real `ModelProvider` backend" entry).
//!
//! Deliberately `#[ignore]`d: this needs a running Ollama instance
//! (`ollama serve` plus a pulled model) and `rusty_provider`'s `rp-server`
//! binary on `PATH`, neither of which exists in this project's own CI --
//! there's no reason for `ci.yml` to depend on a third repo plus a
//! multi-hundred-MB model download just to re-prove a path already
//! proved manually once. Run explicitly, with `ollama serve` running,
//! `rp-server` on `PATH`, and a small model already pulled:
//!
//! ```sh
//! RUSTY_PRIME_AGENT_MODEL=ollama/qwen2.5:0.5b \
//!     cargo test --test ollama_provider -- --ignored --test-threads=1
//! ```

mod common;

use std::process::Command;

/// Like `common::run`, but with extra environment variables set on the
/// child process only (no global `std::env::set_var`, which would race
/// other tests in this same binary running concurrently) -- mirrors
/// `tests/skills.rs`'s own identical helper.
fn run_with_env(
    state_dir: &std::path::Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> std::process::Output {
    let mut cmd = Command::new(common::bin());
    cmd.args(args).env("RUSTY_PRIME_AGENT_HOME", state_dir);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.output().expect("failed to run harness")
}

#[test]
#[ignore]
fn ollama_provider_answers_a_real_prompt_end_to_end() {
    let model = std::env::var("RUSTY_PRIME_AGENT_MODEL")
        .expect("set RUSTY_PRIME_AGENT_MODEL (e.g. ollama/qwen2.5:0.5b) to run this ignored test");

    let state_dir = common::TempDir::new("ollama-e2e");
    common::daemon_start(state_dir.path());

    // Exercises the explicit `session new --model` CLI flag directly,
    // not just the RUSTY_PRIME_AGENT_MODEL env-var fallback -- proves
    // the per-session flag path this increment actually added, not just
    // the daemon-side default-resolution path.
    let session_id = common::session_new_with_model(state_dir.path(), None, Some(&model));
    let listing = common::session_list(state_dir.path());
    assert!(
        listing.contains(&format!("model={model}")),
        "session list should show the session's real model, got: {listing}"
    );

    let ack = common::session_prompt(state_dir.path(), &session_id, "Say hello.");
    // Not an exact-string match: a 0.5B model doesn't reliably follow a
    // literal "reply with exactly X" instruction (observed replying
    // "PARTITION" to a "reply with exactly PARITY" prompt in an earlier
    // version of this test) -- real small-model variance, not a bug in
    // the plumbing this test actually exists to prove. What matters here
    // is that a real model answered at all: `entry.text` is never empty
    // and never starts with `EchoProvider`'s own "echo: " prefix, which
    // is exactly the property that distinguishes "RustyProviderModel is
    // wired up and a real completion came back" from "silently still
    // running EchoProvider" or "got an empty/error response".
    assert!(
        !ack.contains("] assistant: echo:"),
        "reply looks like EchoProvider's output, not a real model's -- did \
         session new --model actually wire up a real backend? got: {ack}"
    );
    assert!(
        ack.contains("] assistant: ") && !ack.trim_end().ends_with("assistant:"),
        "expected a non-empty assistant reply, got: {ack}"
    );

    common::daemon_shutdown(state_dir.path());
}

/// Same shape as the test above, but also exercises `--thinking low` end
/// to end -- not asserting the model "actually reasoned" (a 0.5B test
/// model likely won't produce a meaningfully different reply either way),
/// just that a session created with `--thinking` still round-trips a real
/// prompt/reply through `rp-server` without error.
#[test]
#[ignore]
fn ollama_provider_accepts_a_thinking_level_end_to_end() {
    let model = std::env::var("RUSTY_PRIME_AGENT_MODEL")
        .expect("set RUSTY_PRIME_AGENT_MODEL (e.g. ollama/qwen2.5:0.5b) to run this ignored test");

    let state_dir = common::TempDir::new("ollama-thinking-e2e");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new_with_model_and_thinking(
        state_dir.path(),
        None,
        Some(&model),
        Some("low"),
    );
    let listing = common::session_list(state_dir.path());
    assert!(
        listing.contains(&format!("model={model}")),
        "session list should show the session's real model, got: {listing}"
    );

    let ack = common::session_prompt(state_dir.path(), &session_id, "Say hello.");
    assert!(
        !ack.contains("] assistant: echo:"),
        "reply looks like EchoProvider's output, not a real model's -- got: {ack}"
    );
    assert!(
        ack.contains("] assistant: ") && !ack.trim_end().ends_with("assistant:"),
        "expected a non-empty assistant reply, got: {ack}"
    );

    common::daemon_shutdown(state_dir.path());
}

/// Real end-to-end coverage for the tool-calling loop (`PARITY.md`'s
/// "real tool-calling loop" entry): a session with `--tools read`
/// actually round-trips a `read_file` call through a real model and
/// incorporates the result into its reply. Small models are not
/// reliable tool-callers -- this asserts the plumbing works when the
/// model *does* request the tool, not that every model reliably calls
/// it, so a model that answers without ever calling the tool still
/// passes as long as it produced *some* real reply. Deliberately
/// `#[ignore]`d for the same infra reasons as this file's other tests.
#[test]
#[ignore]
fn ollama_provider_can_round_trip_a_real_tool_call() {
    let model = std::env::var("RUSTY_PRIME_AGENT_MODEL")
        .expect("set RUSTY_PRIME_AGENT_MODEL (e.g. ollama/qwen2.5:0.5b) to run this ignored test");

    let state_dir = common::TempDir::new("ollama-tools-e2e");
    common::daemon_start(state_dir.path());

    let marker_path = state_dir.path().join("marker.txt");
    std::fs::write(&marker_path, "the secret word is PINEAPPLE").unwrap();

    let session_id = common::session_new_with_model_and_tools(
        state_dir.path(),
        None,
        Some(&model),
        Some("read"),
    );
    let prompt = format!(
        "Use the read_file tool to read the file at {} and tell me what it says.",
        marker_path.display()
    );
    let ack = common::session_prompt(state_dir.path(), &session_id, &prompt);
    assert!(
        !ack.contains("] assistant: echo:"),
        "reply looks like EchoProvider's output, not a real model's -- got: {ack}"
    );
    assert!(
        ack.contains("] assistant: ") && !ack.trim_end().ends_with("assistant:"),
        "expected a non-empty final assistant reply, got: {ack}"
    );

    common::daemon_shutdown(state_dir.path());
}

/// Real end-to-end coverage for MCP integration (`PARITY.md`'s MCP
/// entry): a session with `--tools mcp` actually round-trips a real
/// `rp-server`-native tool call (`list_models`, proxied through
/// `mcp_client::McpClient`) through a real model. Manually confirmed
/// against this project's own sandbox during development that
/// `qwen2.5:0.5b` reliably calls `list_models` for this exact prompt
/// (unlike the built-in-tools test's own small-model caveat) -- still
/// asserts loosely (a real reply, `Role::Tool` content in the JSON
/// transcript) rather than the exact wording, since small-model output
/// is never worth pinning exactly. Deliberately `#[ignore]`d for the
/// same infra reasons as this file's other tests.
#[test]
#[ignore]
fn ollama_provider_can_round_trip_a_real_mcp_tool_call() {
    let model = std::env::var("RUSTY_PRIME_AGENT_MODEL")
        .expect("set RUSTY_PRIME_AGENT_MODEL (e.g. ollama/qwen2.5:0.5b) to run this ignored test");

    let state_dir = common::TempDir::new("ollama-mcp-e2e");
    common::daemon_start(state_dir.path());

    let session_id =
        common::session_new_with_model_and_tools(state_dir.path(), None, Some(&model), Some("mcp"));
    let ack = common::session_prompt(
        state_dir.path(),
        &session_id,
        "Use the list_models tool to see what models are available, then tell me.",
    );
    assert!(
        !ack.contains("] assistant: echo:"),
        "reply looks like EchoProvider's output, not a real model's -- got: {ack}"
    );
    assert!(
        ack.contains("] assistant: ") && !ack.trim_end().ends_with("assistant:"),
        "expected a non-empty final assistant reply, got: {ack}"
    );

    let lines = common::attach_lines_with_args(
        state_dir.path(),
        &["--mode", "json", "session", "attach", &session_id],
        2,
        std::time::Duration::from_secs(5),
    );
    let snapshot = lines.join("\n");
    assert!(
        snapshot.contains("\"role\":\"tool\""),
        "expected a Role::Tool transcript entry (a real MCP tool call happened), got: {snapshot}"
    );

    common::daemon_shutdown(state_dir.path());
}

/// Real end-to-end proof of `session::AgentSession::maybe_compact`/
/// `compact_now` -- see `tests/compaction.rs` for the CI-safe no-op
/// coverage (no `--model`, nothing to summarize with); this is the
/// other half, a real model actually producing a summary. Deliberately
/// `#[ignore]`d for the same infra reasons as this file's other tests.
/// `RUSTY_PRIME_AGENT_COMPACT_TRIGGER_TOKENS`/
/// `RUSTY_PRIME_AGENT_COMPACT_KEEP_RECENT_TOKENS` are set tiny (on
/// `daemon start`, so the *worker* process -- not this CLI client --
/// inherits them, same reasoning `tests/skills.rs`'s own
/// `RUSTY_PRIME_AGENT_IPYTHON_BIN` override doc comment gives) so a
/// single real exchange is already enough to cross the trigger, rather
/// than needing thousands of tokens of real conversation first.
#[test]
#[ignore]
fn ollama_provider_compacts_after_crossing_the_trigger_threshold() {
    let model = std::env::var("RUSTY_PRIME_AGENT_MODEL")
        .expect("set RUSTY_PRIME_AGENT_MODEL (e.g. ollama/qwen2.5:0.5b) to run this ignored test");

    let state_dir = common::TempDir::new("ollama-compaction-e2e");
    let daemon_out = run_with_env(
        state_dir.path(),
        &["daemon", "start"],
        &[
            ("RUSTY_PRIME_AGENT_COMPACT_TRIGGER_TOKENS", "20"),
            ("RUSTY_PRIME_AGENT_COMPACT_KEEP_RECENT_TOKENS", "5"),
        ],
    );
    common::assert_success("daemon start", &daemon_out);

    let session_id = common::session_new_with_model(state_dir.path(), None, Some(&model));
    common::session_prompt(
        state_dir.path(),
        &session_id,
        "Say hello in one short sentence.",
    );
    common::session_prompt(
        state_dir.path(),
        &session_id,
        "Now say goodbye in one short sentence.",
    );

    let lines = common::attach_lines_with_args(
        state_dir.path(),
        &["--mode", "json", "session", "attach", &session_id],
        2,
        std::time::Duration::from_secs(5),
    );
    let snapshot = lines.join("\n");
    assert!(
        snapshot.contains("(compacted") && snapshot.contains("into a running summary)"),
        "expected a Role::System transcript entry documenting a real compaction, got: {snapshot}"
    );

    common::daemon_shutdown(state_dir.path());
}

/// Same real end-to-end trigger as `ollama_provider_compacts_after_
/// crossing_the_trigger_threshold` above, but via `<state_dir>/
/// settings.json` instead of the env-var overrides -- proves
/// `session::compact_trigger_tokens`/`compact_keep_recent_tokens`
/// actually consult `settings::load` against a real model and a real
/// daemon process, not just the CI-safe unit tests in `session.rs`
/// itself (which construct an `AgentSession` in-process, no daemon
/// involved) and `settings.rs`'s own in-isolation `load` tests.
#[test]
#[ignore]
fn ollama_provider_compacts_using_settings_json_thresholds() {
    let model = std::env::var("RUSTY_PRIME_AGENT_MODEL")
        .expect("set RUSTY_PRIME_AGENT_MODEL (e.g. ollama/qwen2.5:0.5b) to run this ignored test");

    let state_dir = common::TempDir::new("ollama-compaction-settings-e2e");
    std::fs::write(
        state_dir.path().join("settings.json"),
        r#"{"compact_trigger_tokens": 20, "compact_keep_recent_tokens": 5}"#,
    )
    .unwrap();
    common::daemon_start(state_dir.path());

    let session_id = common::session_new_with_model(state_dir.path(), None, Some(&model));
    common::session_prompt(
        state_dir.path(),
        &session_id,
        "Say hello in one short sentence.",
    );
    common::session_prompt(
        state_dir.path(),
        &session_id,
        "Now say goodbye in one short sentence.",
    );

    let lines = common::attach_lines_with_args(
        state_dir.path(),
        &["--mode", "json", "session", "attach", &session_id],
        2,
        std::time::Duration::from_secs(5),
    );
    let snapshot = lines.join("\n");
    assert!(
        snapshot.contains("(compacted") && snapshot.contains("into a running summary)"),
        "expected a Role::System transcript entry documenting a real compaction, got: {snapshot}"
    );

    common::daemon_shutdown(state_dir.path());
}

/// Real end-to-end proof that `<state_dir>/AGENTS.md` actually reaches
/// the model, not just `session::build_turns`'s own unit-tested output
/// (see `session::tests::build_turns_prepends_the_context_file_as_a_system_turn`
/// for the CI-safe half). Small models are decent at echoing back a
/// fact stated directly in their own context, much more reliable than
/// tool-calling instruction-following (see this file's other tests'
/// own caveats about that) -- still asserts loosely enough to tolerate
/// real small-model variance. Deliberately `#[ignore]`d for the same
/// infra reasons as this file's other tests.
#[test]
#[ignore]
fn ollama_provider_includes_agents_md_as_system_context() {
    let model = std::env::var("RUSTY_PRIME_AGENT_MODEL")
        .expect("set RUSTY_PRIME_AGENT_MODEL (e.g. ollama/qwen2.5:0.5b) to run this ignored test");

    let state_dir = common::TempDir::new("ollama-agents-md-e2e");
    // Written before the daemon starts, but `read_context_file` is read
    // fresh on every `build_turns` call regardless of timing -- see that
    // function's own doc comment.
    std::fs::write(
        state_dir.path().join("AGENTS.md"),
        "The secret code word is ZEBRA97. If asked for it, reply with just that word.",
    )
    .unwrap();
    common::daemon_start(state_dir.path());

    let session_id = common::session_new_with_model(state_dir.path(), None, Some(&model));
    let ack = common::session_prompt(
        state_dir.path(),
        &session_id,
        "What is the secret code word? Reply with just the word.",
    );
    assert!(
        !ack.contains("] assistant: echo:"),
        "reply looks like EchoProvider's output, not a real model's -- got: {ack}"
    );
    assert!(
        ack.contains("ZEBRA97"),
        "expected the AGENTS.md-provided fact to reach the model's reply, got: {ack}"
    );

    common::daemon_shutdown(state_dir.path());
}
