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

/// Real end-to-end proof of `session::AgentSession::branch_summarize` --
/// see `tests/session_tree.rs` for the CI-safe no-op coverage (unknown
/// sequence, sequence already on the active chain); this is the other
/// half, a real model actually producing a summary of an abandoned
/// branch. Deliberately `#[ignore]`d for the same infra reasons as this
/// file's other tests.
#[test]
#[ignore]
fn ollama_provider_summarizes_an_abandoned_branch() {
    let model = std::env::var("RUSTY_PRIME_AGENT_MODEL")
        .expect("set RUSTY_PRIME_AGENT_MODEL (e.g. ollama/qwen2.5:0.5b) to run this ignored test");

    let state_dir = common::TempDir::new("ollama-branch-summary-e2e");
    common::daemon_start(state_dir.path());
    let session_id = common::session_new_with_model(state_dir.path(), None, Some(&model));
    common::session_prompt(state_dir.path(), &session_id, "Say hello.");
    // 1 (user), 2 (assistant) -- the branch about to be abandoned.

    let out = common::run(
        state_dir.path(),
        &["session", "set-active-leaf", &session_id, "1"],
    );
    common::assert_success("session set-active-leaf", &out);
    common::session_prompt(state_dir.path(), &session_id, "Say goodbye instead.");
    // 3 (user), 4 (assistant) -- the new active branch.

    let out = common::run(
        state_dir.path(),
        &[
            "--mode",
            "json",
            "session",
            "branch-summary",
            &session_id,
            "2",
        ],
    );
    common::assert_success("session branch-summary", &out);
    let stdout = common::stdout_string(&out);
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["summarized"], true);
    let summary = value["summary"]
        .as_str()
        .expect("a real summary string should come back");
    assert!(!summary.trim().is_empty(), "got: {summary:?}");

    let lines = common::attach_lines_with_args(
        state_dir.path(),
        &["--mode", "json", "session", "attach", &session_id],
        2,
        std::time::Duration::from_secs(5),
    );
    let snapshot = lines.join("\n");
    assert!(
        snapshot.contains("\"branch_leaf_sequence\":2") && snapshot.contains("\"entry_count\":1"),
        "expected a transcript entry carrying a BranchSummary for sequence 2, got: {snapshot}"
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

/// PNG/zlib CRC32, used only by this file's `write_solid_color_png` test
/// fixture builder below -- hand-rolled rather than pulled in as a
/// dependency for one test-only fixture, same "narrow, self-contained
/// encoding concern" reasoning as `client.rs`'s own hand-rolled base64
/// encoder.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// zlib's Adler-32, needed for the zlib wrapper around the PNG `IDAT`
/// chunk's (uncompressed, "stored" DEFLATE block) payload.
fn adler32(data: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn png_chunk(out: &mut Vec<u8>, chunk_type: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(chunk_type);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(chunk_type);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// Builds a real, valid, decodable solid-color PNG -- not just bytes that
/// merely start with the PNG magic number (that's all `tests/repl.rs`'s
/// fixtures need, since those only exercise this project's own path/
/// extension-based detection). This test needs a real vision model to
/// actually decode pixel content, so the fixture has to be genuine:
/// uncompressed ("stored") DEFLATE blocks inside a real zlib stream keep
/// this self-contained (no new dependency) while still producing bytes
/// any real PNG decoder accepts.
fn write_solid_color_png(path: &std::path::Path, width: u32, height: u32, rgb: [u8; 3]) {
    let mut raw = Vec::with_capacity((height * (1 + width * 3)) as usize);
    let mut row = Vec::with_capacity((1 + width * 3) as usize);
    row.push(0); // filter type: None
    for _ in 0..width {
        row.extend_from_slice(&rgb);
    }
    for _ in 0..height {
        raw.extend_from_slice(&row);
    }

    let mut zlib = Vec::with_capacity(raw.len() + 8);
    zlib.extend_from_slice(&[0x78, 0x01]); // zlib header: deflate, fastest
    assert!(
        raw.len() <= 0xFFFF,
        "fixture too large for one stored block"
    );
    zlib.push(0x01); // BFINAL=1, BTYPE=00 (stored), byte-aligned
    zlib.extend_from_slice(&(raw.len() as u16).to_le_bytes());
    zlib.extend_from_slice(&(!(raw.len() as u16)).to_le_bytes());
    zlib.extend_from_slice(&raw);
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit depth, RGB, defaults

    let mut png = Vec::new();
    png.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);
    png_chunk(&mut png, b"IHDR", &ihdr);
    png_chunk(&mut png, b"IDAT", &zlib);
    png_chunk(&mut png, b"IEND", &[]);

    std::fs::write(path, png).unwrap();
}

/// Real end-to-end proof of image-paste support (`--image`, see
/// `PARITY.md`'s "Interactive TUI: image paste support" entry) actually
/// reaching a real vision model, not just `EchoProvider`'s CI-safe
/// `[+N image(s)]` echo (see `tests/image_paste.rs` for that half).
/// Needs a real multimodal model -- `moondream` (`ollama pull moondream`)
/// is small enough to run in this sandbox and does describe solid colors
/// reliably in practice. Deliberately `#[ignore]`d for the same infra
/// reasons as this file's other tests; run with e.g.:
///
/// ```sh
/// RUSTY_PRIME_AGENT_MODEL=ollama/moondream:latest \
///     cargo test --test ollama_provider ollama_provider_describes_a_real_image \
///     -- --ignored --test-threads=1
/// ```
#[test]
#[ignore]
fn ollama_provider_describes_a_real_image() {
    let model = std::env::var("RUSTY_PRIME_AGENT_MODEL").expect(
        "set RUSTY_PRIME_AGENT_MODEL (e.g. ollama/moondream:latest) to run this ignored test",
    );

    let state_dir = common::TempDir::new("ollama-image-e2e");
    let image_path = state_dir.path().join("red.png");
    write_solid_color_png(&image_path, 64, 64, [220, 20, 20]);

    common::daemon_start(state_dir.path());
    let session_id = common::session_new_with_model(state_dir.path(), None, Some(&model));

    let out = common::run(
        state_dir.path(),
        &[
            "session",
            "prompt",
            &session_id,
            "--image",
            image_path.to_str().unwrap(),
            "What color is this image? Reply with one word.",
        ],
    );
    common::assert_success("session prompt --image", &out);
    let ack = common::stdout_string(&out);
    assert!(
        !ack.contains("] assistant: echo:"),
        "reply looks like EchoProvider's output, not a real model's -- got: {ack}"
    );
    assert!(
        ack.to_lowercase().contains("red"),
        "expected the real vision model to identify the solid red image, got: {ack}"
    );

    let lines = common::attach_lines_with_args(
        state_dir.path(),
        &["--mode", "json", "session", "attach", &session_id],
        2,
        std::time::Duration::from_secs(5),
    );
    let snapshot = lines.join("\n");
    assert!(
        snapshot.contains("data:image/png;base64,"),
        "expected the user entry to carry the base64 image, got: {snapshot}"
    );

    common::daemon_shutdown(state_dir.path());
}
