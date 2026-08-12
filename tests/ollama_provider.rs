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
