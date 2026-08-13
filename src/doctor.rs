//! `harness doctor [--fix]` -- bounded, honest parity with `prime-agent
//! doctor [--fix]` (`CLAIMS_AUDIT.md` previously confirmed this entirely
//! absent). Runs a small, fixed set of real diagnostic checks against
//! this local install and reports what it finds; deliberately does not
//! duplicate what `session list`'s own `catalog::scan` already surfaces
//! (stale/crashed worker detection) -- this instead checks the things
//! nothing else in this project ever reports proactively: whether the
//! daemon is reachable at all, whether `rp-server` can be found (needed
//! by anything other than `EchoProvider`), and whether the three
//! top-level config files (`settings.json`/`auth.json`/`providers.json`)
//! actually parse. Every one of `settings::load`/`auth::load`/
//! `providers::load` is deliberately permissive (malformed JSON silently
//! reads as "no config" -- see each module's own doc comment), which is
//! the right default for every other caller but means a typo in one of
//! these files is otherwise invisible; `doctor` is the one place that
//! surfaces it loudly.
//!
//! `--fix` is deliberately narrow: it only ever starts the daemon if it
//! isn't already running (`ensure_daemon_started`, the same idempotent
//! spawn `daemon start` itself uses) -- a safe, reversible, already-
//! expected-to-be-idempotent action. It never rewrites a malformed
//! config file or otherwise mutates state on the caller's behalf; that
//! would be presumptuous about what the caller actually wants, the same
//! reasoning `self_update`'s own `--force` deliberately never means
//! "discard local changes."

use std::path::Path;

use serde::Serialize;

use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

/// Checks one optional top-level JSON config file for existence +
/// parseability, without caring about its actual shape -- `doctor` only
/// wants to catch "this is not valid JSON at all," not second-guess any
/// particular field the way the file's own permissive `load` never does.
fn check_json_file(state_root: &Path, filename: &str) -> Check {
    let path = state_root.join(filename);
    let name = filename.to_string();
    match std::fs::read_to_string(&path) {
        Err(_) => Check {
            name,
            status: CheckStatus::Ok,
            detail: "not present".to_string(),
        },
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(_) => Check {
                name,
                status: CheckStatus::Ok,
                detail: "valid JSON".to_string(),
            },
            Err(e) => Check {
                name,
                status: CheckStatus::Error,
                detail: format!("malformed JSON, being silently ignored by every real caller: {e}"),
            },
        },
    }
}

/// Runs every check. `fix`, when `true`, additionally starts the daemon
/// if it wasn't already reachable -- see this module's own doc comment
/// for why that's the one safe auto-fix attempted.
pub async fn run(state_root: &Path, exe_path: &Path, fix: bool) -> Result<Vec<Check>> {
    let mut checks = Vec::new();

    let daemon_reachable = crate::client::daemon_reachable(state_root).await;
    if daemon_reachable {
        checks.push(Check {
            name: "daemon".to_string(),
            status: CheckStatus::Ok,
            detail: "reachable".to_string(),
        });
    } else if fix {
        match crate::client::ensure_daemon_started(state_root, exe_path).await {
            Ok(Some(pid)) => checks.push(Check {
                name: "daemon".to_string(),
                status: CheckStatus::Ok,
                detail: format!("was not running -- started (pid {pid})"),
            }),
            Ok(None) => checks.push(Check {
                name: "daemon".to_string(),
                status: CheckStatus::Ok,
                detail: "reachable".to_string(),
            }),
            Err(e) => checks.push(Check {
                name: "daemon".to_string(),
                status: CheckStatus::Error,
                detail: format!("not running, and --fix failed to start it: {e}"),
            }),
        }
    } else {
        checks.push(Check {
            name: "daemon".to_string(),
            status: CheckStatus::Warn,
            detail: "not running -- run `daemon start`, or `doctor --fix`".to_string(),
        });
    }

    checks.push(if crate::rp_server::rp_server_available() {
        Check {
            name: "rp-server".to_string(),
            status: CheckStatus::Ok,
            detail: "found".to_string(),
        }
    } else {
        Check {
            name: "rp-server".to_string(),
            status: CheckStatus::Warn,
            detail:
                "not found on PATH -- needed for any non-Echo provider and `model list --detailed`"
                    .to_string(),
        }
    });

    checks.push(check_json_file(state_root, "settings.json"));
    checks.push(check_json_file(state_root, "auth.json"));
    checks.push(check_json_file(state_root, "providers.json"));

    Ok(checks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state_root(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rpa-doctor-test-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_config_file_is_ok_not_a_warning() {
        let root = temp_state_root("missing-config");
        let check = check_json_file(&root, "settings.json");
        assert_eq!(check.status, CheckStatus::Ok);
        assert_eq!(check.detail, "not present");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_valid_config_file_is_ok() {
        let root = temp_state_root("valid-config");
        std::fs::write(root.join("settings.json"), r#"{"theme": "dark"}"#).unwrap();
        let check = check_json_file(&root, "settings.json");
        assert_eq!(check.status, CheckStatus::Ok);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_malformed_config_file_is_an_error_not_silently_ignored() {
        let root = temp_state_root("malformed-config");
        std::fs::write(root.join("auth.json"), "{ not json").unwrap();
        let check = check_json_file(&root, "auth.json");
        assert_eq!(check.status, CheckStatus::Error);
        assert!(check.detail.contains("malformed JSON"), "{}", check.detail);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
