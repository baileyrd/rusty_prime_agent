//! `harness doctor [--fix]` -- see `doctor`'s own module doc comment for
//! exactly what's checked. All CI-safe: no daemon needed to run
//! `doctor` itself (reachability is one of the things it checks), no
//! real model/kernel involved anywhere.

mod common;

#[test]
fn doctor_reports_the_daemon_as_not_running_when_it_isnt() {
    let state_dir = common::TempDir::new("doctor-daemon-down");

    let out = common::run(state_dir.path(), &["doctor"]);
    common::assert_success("doctor", &out);
    let stdout = common::stdout_string(&out);
    assert!(
        stdout.contains("daemon\twarn\tnot running"),
        "got: {stdout}"
    );
}

#[test]
fn doctor_reports_the_daemon_as_reachable_when_it_is() {
    let state_dir = common::TempDir::new("doctor-daemon-up");
    common::daemon_start(state_dir.path());

    let out = common::run(state_dir.path(), &["doctor"]);
    common::assert_success("doctor", &out);
    let stdout = common::stdout_string(&out);
    assert!(stdout.contains("daemon\tok\treachable"), "got: {stdout}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn doctor_fix_starts_the_daemon_if_it_was_not_running() {
    let state_dir = common::TempDir::new("doctor-fix");

    let out = common::run(state_dir.path(), &["doctor", "--fix"]);
    common::assert_success("doctor --fix", &out);
    let stdout = common::stdout_string(&out);
    assert!(
        stdout.contains("daemon\tok\twas not running -- started"),
        "got: {stdout}"
    );

    // A second call now finds it already reachable -- --fix's own
    // idempotent spawn behavior, same as `daemon start`'s.
    let out2 = common::run(state_dir.path(), &["doctor", "--fix"]);
    common::assert_success("doctor --fix (again)", &out2);
    let stdout2 = common::stdout_string(&out2);
    assert!(stdout2.contains("daemon\tok\treachable"), "got: {stdout2}");

    common::daemon_shutdown(state_dir.path());
}

#[test]
fn doctor_reports_a_malformed_config_file_loudly() {
    let state_dir = common::TempDir::new("doctor-malformed-config");
    std::fs::write(state_dir.path().join("auth.json"), "{ not json").unwrap();

    let out = common::run(state_dir.path(), &["doctor"]);
    common::assert_success("doctor", &out);
    let stdout = common::stdout_string(&out);
    assert!(
        stdout.contains("auth.json\terror\tmalformed JSON"),
        "got: {stdout}"
    );
    // A config file that was never dropped at all reads as ok, not a
    // warning or error -- most callers never configure any of these.
    assert!(
        stdout.contains("settings.json\tok\tnot present"),
        "got: {stdout}"
    );
}

#[test]
fn doctor_reports_a_valid_config_file_as_ok() {
    let state_dir = common::TempDir::new("doctor-valid-config");
    std::fs::write(
        state_dir.path().join("settings.json"),
        r#"{"theme": "dark"}"#,
    )
    .unwrap();

    let out = common::run(state_dir.path(), &["doctor"]);
    common::assert_success("doctor", &out);
    let stdout = common::stdout_string(&out);
    assert!(
        stdout.contains("settings.json\tok\tvalid JSON"),
        "got: {stdout}"
    );
}

#[test]
fn doctor_json_mode_emits_a_structured_array() {
    let state_dir = common::TempDir::new("doctor-json");

    let out = common::run(state_dir.path(), &["--mode", "json", "doctor"]);
    common::assert_success("--mode json doctor", &out);
    let stdout = common::stdout_string(&out);
    let checks: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let checks = checks.as_array().expect("expected a JSON array");
    assert!(checks.iter().any(|c| c["name"] == "daemon"));
    assert!(checks.iter().any(|c| c["name"] == "rp-server"));
    assert!(checks.iter().any(|c| c["name"] == "settings.json"));
    assert!(checks.iter().any(|c| c["name"] == "auth.json"));
    assert!(checks.iter().any(|c| c["name"] == "providers.json"));
}
