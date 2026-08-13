//! Real, importable Python packages for `session new --runtime ipython`
//! -- see `crate::skills`'s own module doc comment for the on-disk
//! layout and why this is global-only (no project-local tier, unlike
//! `prompt_template`'s).

mod common;

use std::path::Path;
use std::process::Command;

/// Like `common::run`, but with extra environment variables set on the
/// child process only (no global `std::env::set_var`, which would race
/// other tests in this same binary running concurrently) -- mirrors
/// `tests/ipython_runtime.rs`'s own identical helper.
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

fn write_skill(state_dir: &Path, name: &str, manifest: &str, init_py: &str) {
    let dir = state_dir.join("skills").join(name);
    std::fs::create_dir_all(&dir).expect("create skill dir");
    std::fs::write(dir.join("SKILL.md"), manifest).expect("write SKILL.md");
    std::fs::write(dir.join("__init__.py"), init_py).expect("write __init__.py");
}

#[test]
fn skill_list_reports_none_when_empty() {
    let state_dir = common::TempDir::new("skill-empty");

    // No daemon needed -- a pure local directory scan, same as
    // `prompt-template list`.
    let out = common::run(state_dir.path(), &["skill", "list"]);
    common::assert_success("skill list", &out);
    assert_eq!(common::stdout_string(&out), "no skills");
}

#[test]
fn skill_list_discovers_an_installed_skill() {
    let state_dir = common::TempDir::new("skill-installed");
    write_skill(
        state_dir.path(),
        "weather",
        "---\ndescription: fetch weather data\n---\nLonger docs.\n",
        "def forecast():\n    return 'sunny'\n",
    );

    let out = common::run(state_dir.path(), &["skill", "list"]);
    common::assert_success("skill list", &out);
    assert_eq!(common::stdout_string(&out), "weather\tfetch weather data");
}

#[test]
fn skill_list_skips_directories_with_no_skill_md() {
    let state_dir = common::TempDir::new("skill-not-a-skill");
    std::fs::create_dir_all(state_dir.path().join("skills").join("just_a_folder"))
        .expect("create stray dir");

    let out = common::run(state_dir.path(), &["skill", "list"]);
    common::assert_success("skill list", &out);
    assert_eq!(common::stdout_string(&out), "no skills");
}

#[test]
fn skill_list_json_mode_emits_structured_entries() {
    let state_dir = common::TempDir::new("skill-json");
    write_skill(
        state_dir.path(),
        "weather",
        "---\ndescription: fetch weather data\n---\n",
        "",
    );

    let out = common::run(state_dir.path(), &["--mode", "json", "skill", "list"]);
    common::assert_success("skill list", &out);
    let stdout = common::stdout_string(&out);
    assert!(
        stdout.contains(r#""name":"weather""#) && stdout.contains(r#""fetch weather data""#),
        "got: {stdout}"
    );
}

#[test]
fn skill_list_shows_license_compatibility_and_the_disable_model_invocation_tag() {
    let state_dir = common::TempDir::new("skill-extra-fields");
    write_skill(
        state_dir.path(),
        "weather",
        "---\ndescription: fetch weather data\nlicense: MIT\n\
         compatibility: >=1.0\ndisable-model-invocation: true\n---\n",
        "",
    );

    let out = common::run(state_dir.path(), &["skill", "list"]);
    common::assert_success("skill list", &out);
    let stdout = common::stdout_string(&out);
    assert!(
        stdout.contains("weather\tfetch weather data"),
        "got: {stdout}"
    );
    assert!(stdout.contains("license: MIT"), "got: {stdout}");
    assert!(stdout.contains("compatibility: >=1.0"), "got: {stdout}");
    assert!(
        stdout.contains("disable-model-invocation") && stdout.contains("/skill:weather"),
        "got: {stdout}"
    );
}

#[test]
fn skill_list_shows_a_name_mismatch_warning_without_failing() {
    let state_dir = common::TempDir::new("skill-name-mismatch");
    write_skill(
        state_dir.path(),
        "weather",
        "---\ndescription: fetch weather data\nname: weather-forecaster\n---\n",
        "",
    );

    let out = common::run(state_dir.path(), &["skill", "list"]);
    common::assert_success("skill list", &out);
    let stdout = common::stdout_string(&out);
    assert!(
        stdout.contains("weather\tfetch weather data"),
        "got: {stdout}"
    );
    assert!(
        stdout.contains("warning:") && stdout.contains("weather-forecaster"),
        "got: {stdout}"
    );
}

/// `session new --runtime ipython` fails loudly (not silently) before it
/// ever gets far enough to install skills, when no real kernel is
/// available -- same negative-path proof `tests/ipython_runtime.rs`'s own
/// `runtime_ipython_fails_loudly_when_no_kernel_is_available` establishes
/// for the base `--runtime ipython` flag, confirming skill installation
/// doesn't change that failure mode (it runs strictly after
/// `tool_runtime.start()` succeeds, see `worker::run`).
#[test]
fn runtime_ipython_with_a_skill_installed_still_fails_loudly_without_a_kernel() {
    let state_dir = common::TempDir::new("skill-no-kernel");
    write_skill(
        state_dir.path(),
        "weather",
        "---\ndescription: fetch weather data\n---\n",
        "",
    );
    // The `RUSTY_PRIME_AGENT_IPYTHON_BIN` override has to reach the
    // *worker* process, which `worker::spawn` launches from the daemon's
    // own environment, not the CLI client's -- so it's set on `daemon
    // start`, not on `session new` (same pattern `tests/ipython_runtime.rs`'s
    // own `runtime_ipython_fails_loudly_when_no_kernel_is_available` uses).
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
        "session new --runtime ipython should fail when no kernel binary exists"
    );

    common::daemon_shutdown(state_dir.path());
}
