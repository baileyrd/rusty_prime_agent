//! Real end-to-end coverage for `provider::OllamaProvider`/`rp_server`
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
    // Safe despite being process-wide env mutation: this file has a
    // single test function, so there's no other test in this binary to
    // race with -- see this file's own module doc comment for why
    // `--test-threads=1` is part of the documented invocation anyway.
    std::env::set_var("RUSTY_PRIME_AGENT_PROVIDER", "ollama");
    std::env::set_var("RUSTY_PRIME_AGENT_MODEL", &model);

    let state_dir = common::TempDir::new("ollama-e2e");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);
    let ack = common::session_prompt(state_dir.path(), &session_id, "Say hello.");
    // Not an exact-string match: a 0.5B model doesn't reliably follow a
    // literal "reply with exactly X" instruction (observed replying
    // "PARTITION" to a "reply with exactly PARITY" prompt in an earlier
    // version of this test) -- real small-model variance, not a bug in
    // the plumbing this test actually exists to prove. What matters here
    // is that a real model answered at all: `entry.text` is never empty
    // and never starts with `EchoProvider`'s own "echo: " prefix, which
    // is exactly the property that distinguishes "OllamaProvider is
    // wired up and a real completion came back" from "silently still
    // running EchoProvider" or "got an empty/error response".
    assert!(
        !ack.contains("] assistant: echo:"),
        "reply looks like EchoProvider's output, not a real model's -- is \
         RUSTY_PRIME_AGENT_PROVIDER=ollama actually wired up? got: {ack}"
    );
    assert!(
        ack.contains("] assistant: ") && !ack.trim_end().ends_with("assistant:"),
        "expected a non-empty assistant reply, got: {ack}"
    );

    common::daemon_shutdown(state_dir.path());
}
