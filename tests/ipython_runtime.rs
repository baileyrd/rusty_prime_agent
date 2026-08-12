//! CI-safe coverage for `session new --runtime ipython`'s CLI/daemon/
//! worker plumbing (parity with the RLM programming model, see
//! `PARITY.md`). Real end-to-end coverage against a genuine `ipykernel`
//! subprocess -- spawn, ZMTP handshake, `execute_request`/iopub round
//! trip -- lives as an `#[ignore]`d test directly in `src/
//! ipython_runtime.rs`'s own test module instead of here, since this
//! project's binary has no `[lib]` target (see `tests/common/mod.rs`'s
//! own doc comment) and a Rust-level test of `IpythonKernelRuntime` can
//! only live inside the crate that defines it. Run that one explicitly:
//!
//! ```sh
//! cargo test --bin harness ipython_runtime::tests::real_kernel -- --ignored
//! ```
//!
//! The tests here instead prove the flag reaches all the way through
//! `session new` -> the daemon -> a spawned worker's `ToolRuntime`
//! selection without needing a real kernel at all, by pointing
//! `RUSTY_PRIME_AGENT_IPYTHON_BIN` at a binary name that can't possibly
//! exist -- the same "fails loudly, not silently" property `tests/
//! tool_calling.rs`'s `tools_mcp_fails_loudly_when_rp_server_is_unavailable`
//! already establishes for `--tools mcp`.

mod common;

use std::path::Path;
use std::process::Command;

/// Like `common::run`, but with one extra environment variable set on
/// just this child process -- `common::run` only ever sets
/// `RUSTY_PRIME_AGENT_HOME`, and mutating this test process's own global
/// environment (`std::env::set_var`) to add `RUSTY_PRIME_AGENT_IPYTHON_BIN`
/// would race every other test in this binary running concurrently on a
/// separate thread (Rust's test harness runs `#[test]`s in parallel by
/// default within one binary; env vars are process-global, not
/// thread-local).
fn run_with_env(
    state_dir: &Path,
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
fn unknown_runtime_value_is_rejected_at_parse_time() {
    let state_dir = common::TempDir::new("runtime-invalid");
    let out = common::run(state_dir.path(), &["session", "new", "--runtime", "shell"]);
    assert!(
        !out.status.success(),
        "expected `--runtime shell` to fail loudly (not yet a supported value), got success"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--runtime"),
        "expected an error mentioning --runtime, got: {stderr}"
    );
}

/// A session with no `--runtime` at all keeps today's `NoopToolRuntime`
/// behavior completely unaffected -- same "the default path never
/// regresses" property `tests/tool_calling.rs`'s own
/// `a_session_without_tools_never_offers_them_either` establishes for
/// `--tools`.
#[test]
fn session_without_runtime_is_unaffected() {
    let state_dir = common::TempDir::new("runtime-default");
    common::daemon_start(state_dir.path());

    let session_id = common::session_new(state_dir.path(), None);
    let ack = common::session_prompt(state_dir.path(), &session_id, "hello");
    assert!(ack.contains("] assistant: echo: hello"), "got: {ack}");

    let listing = common::session_list(state_dir.path());
    assert!(
        !listing.contains("runtime=ipython"),
        "a session created without --runtime should never show runtime=ipython, got: {listing}"
    );

    common::daemon_shutdown(state_dir.path());
}

/// `--runtime ipython` reaches all the way to the worker's `ToolRuntime`
/// selection and fails loudly when the kernel can't actually be spawned
/// -- `RUSTY_PRIME_AGENT_IPYTHON_BIN` points at a binary name that can't
/// exist, so this is deterministic regardless of whether a real `python3`/
/// `ipykernel` happens to be installed wherever this test runs (CI never
/// has one; a developer's own machine might).
///
/// The override is set on the *daemon's* own environment (via
/// `run_with_env` on `daemon start` itself), not on the later `session
/// new` invocation: `rusty_tokio::process::Command` inherits its own
/// process's full environment by default (see `rp_server`'s own doc
/// comment for this exact reasoning applied to `rp-server`'s env vars),
/// and the worker that actually spawns the kernel is a child of the
/// long-running *daemon* process, not of the short-lived `session new`
/// CLI invocation that merely asks it to over a socket -- an env var set
/// only on that one CLI call never reaches the daemon at all. An earlier
/// version of this test set it on `session new` instead and the kernel
/// spawned successfully anyway (silently using this sandbox's own real
/// `python3`/`ipykernel`), which is exactly the bug this comment now
/// documents.
///
/// The underlying failure is the worker exiting before it ever binds its
/// private socket (`worker::run`'s own `tool_runtime.start().await?`
/// early-return, since the kernel subprocess can't spawn) -- but unlike
/// `tools_mcp_fails_loudly_when_rp_server_is_unavailable`'s fast,
/// synchronous `rp_server::ensure_running` pre-check, a kernel is spawned
/// inside the (detached, asynchronous) worker process itself, so the
/// daemon can only detect the failure via `daemon::WORKER_READY_TIMEOUT`'s
/// 30s poll. This test's own client-side call gives up well before that,
/// on `client::RESPONSE_TIMEOUT`'s much shorter 5s bound (`session new`'s
/// ordinary, non-prompt request path) -- confirmed by direct testing: an
/// earlier version of this test asserted the stderr would name "worker",
/// expecting to observe the daemon's own eventual `WORKER_READY_TIMEOUT`
/// failure, but the client always times out first and reports its own
/// generic "daemon did not respond in time" instead. This is a
/// pre-existing client/daemon timeout mismatch, not something this
/// increment introduces -- every other `session new` failure path this
/// project tests either fails synchronously (well under 5s) or succeeds,
/// so `--runtime ipython`'s worker-side-only failure mode is the first to
/// actually expose it. Asserted loosely here (any failure, any daemon-
/// mentioning message) rather than pinned to the exact wording, since
/// which of the two timeouts wins first is a race this test doesn't need
/// to nail down further to prove the flag reaches the worker.
#[test]
fn runtime_ipython_fails_loudly_when_no_kernel_is_available() {
    let state_dir = common::TempDir::new("runtime-no-kernel");
    let daemon_out = run_with_env(
        state_dir.path(),
        &["daemon", "start"],
        &[(
            "RUSTY_PRIME_AGENT_IPYTHON_BIN",
            "rusty-prime-agent-definitely-not-a-real-binary",
        )],
    );
    common::assert_success("daemon start", &daemon_out);

    let out = common::run(
        state_dir.path(),
        &["session", "new", "--runtime", "ipython"],
    );
    assert!(
        !out.status.success(),
        "session new --runtime ipython should fail when no kernel can be spawned"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("daemon"),
        "expected an error mentioning the daemon, got: {stderr}"
    );

    common::daemon_shutdown(state_dir.path());
}
