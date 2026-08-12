//! Bounded parity with `prime-agent model list`'s catalog browse -- see
//! `client::model_list`'s own doc comment for why this is a provider
//! catalog, not a per-model one. A pure environment-variable check, so
//! these tests build their own `std::process::Command` with an
//! explicitly cleared environment rather than using `tests/common::run`
//! (which only adds `RUSTY_PRIME_AGENT_HOME` on top of whatever this
//! test process itself inherited -- not deterministic for this).

mod common;

use std::process::Command;

/// `model list` genuinely never talks to `state_dir()`'s own resolved
/// directory, but `main::run` still resolves it unconditionally before
/// dispatching to any command (`paths::state_dir()` is the one call
/// every subcommand's dispatch arm sits behind) -- so `RUSTY_PRIME_
/// AGENT_HOME` still has to be set here, the same as every other
/// command in this project needs it, even though `model_list` itself
/// never reads it.
fn run_with_env(state_dir: &std::path::Path, extra_env: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(common::bin());
    cmd.args(["model", "list"])
        .env_clear()
        .env("RUSTY_PRIME_AGENT_HOME", state_dir);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    cmd.output().expect("failed to run harness model list")
}

#[test]
fn ollama_is_always_configured_and_others_are_not_by_default() {
    let state_dir = common::TempDir::new("model-list-default");
    let out = run_with_env(state_dir.path(), &[]);
    common::assert_success("model list", &out);
    let stdout = common::stdout_string(&out);
    assert!(stdout.contains("ollama\tconfigured"), "got: {stdout}");
    assert!(stdout.contains("openai\tnot configured"), "got: {stdout}");
    assert!(
        stdout.contains("anthropic\tnot configured"),
        "got: {stdout}"
    );
    assert!(stdout.contains("gemini\tnot configured"), "got: {stdout}");
    assert!(stdout.contains("groq\tnot configured"), "got: {stdout}");
}

#[test]
fn a_configured_api_key_env_var_flips_that_provider_to_configured() {
    let state_dir = common::TempDir::new("model-list-configured");
    let out = run_with_env(state_dir.path(), &[("ANTHROPIC_API_KEY", "test-key")]);
    common::assert_success("model list", &out);
    let stdout = common::stdout_string(&out);
    assert!(stdout.contains("anthropic\tconfigured"), "got: {stdout}");
    // Only anthropic flips -- the others stay unconfigured.
    assert!(stdout.contains("openai\tnot configured"), "got: {stdout}");
    assert!(stdout.contains("gemini\tnot configured"), "got: {stdout}");
    assert!(stdout.contains("groq\tnot configured"), "got: {stdout}");
}

#[test]
fn json_mode_emits_a_structured_provider_list() {
    let state_dir = common::TempDir::new("model-list-json");
    let mut cmd = Command::new(common::bin());
    cmd.args(["--mode", "json", "model", "list"])
        .env_clear()
        .env("RUSTY_PRIME_AGENT_HOME", state_dir.path());
    let out = cmd.output().expect("failed to run harness model list");
    common::assert_success("model list --mode json", &out);
    let stdout = common::stdout_string(&out);
    let providers: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("expected one JSON line, got {stdout:?}: {e}"));
    let providers = providers.as_array().expect("providers is a JSON array");
    assert!(providers.len() >= 5, "got: {providers:?}");
    let ollama = providers
        .iter()
        .find(|p| p["name"] == "ollama")
        .expect("ollama entry present");
    assert_eq!(ollama["configured"], true);
}

/// `--detailed` needs a real `rp-server` sidecar (`rp_server::
/// ensure_running`) -- this project's own CI has no such binary on
/// `PATH`, same gap `session_new_with_model_fails_loudly_when_rp_server_
/// is_unavailable` (`tests/session_lifecycle.rs`) already covers for
/// `session new --model`. A deterministic, CI-safe negative case: fails
/// loudly with a clear message rather than hanging or silently falling
/// back to the plain provider listing.
#[test]
fn detailed_fails_loudly_when_rp_server_is_unavailable() {
    let state_dir = common::TempDir::new("model-list-detailed-unavailable");
    let mut cmd = Command::new(common::bin());
    cmd.args(["model", "list", "--detailed"])
        .env_clear()
        .env("RUSTY_PRIME_AGENT_HOME", state_dir.path())
        // Force "rp-server" to resolve to a name that can't possibly
        // exist on PATH, so this doesn't depend on the test-running
        // environment actually lacking a real rp-server binary.
        .env(
            "RUSTY_PRIME_AGENT_RP_SERVER_BIN",
            "definitely-not-a-real-rp-server-binary",
        );
    let out = cmd.output().expect("failed to run harness model list");
    assert!(
        !out.status.success(),
        "expected a failure exit code, got success: {}",
        common::stdout_string(&out)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.trim().is_empty(),
        "expected a non-empty error message on stderr"
    );
}

/// Real end-to-end coverage for `--detailed`'s `GET /v1/models` query,
/// same infra-gated shape as `tests/ollama_provider.rs`: needs
/// `rp-server` on `PATH` and `ollama serve` running. Deliberately
/// `#[ignore]`d for the same "no third repo/model download in this
/// project's own CI" reasoning as that file's own tests. Unlike those,
/// this doesn't need a `daemon start` at all -- `--detailed` starts its
/// own `rp-server` sidecar directly, no daemon/worker involved. Run
/// explicitly with `rp-server` on `PATH` and `ollama serve` running:
///
/// ```sh
/// cargo test --test model_list -- --ignored --test-threads=1
/// ```
#[test]
#[ignore]
fn detailed_lists_ollamas_real_models() {
    let state_dir = common::TempDir::new("model-list-detailed-e2e");
    let out = common::run(state_dir.path(), &["model", "list", "--detailed"]);
    common::assert_success("model list --detailed", &out);
    let stdout = common::stdout_string(&out);
    assert!(
        stdout.contains("owned_by="),
        "expected at least one real model entry, got: {stdout}"
    );
}
