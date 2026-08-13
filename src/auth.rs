//! `<state_dir>/auth.json` -- shell-command key resolution for provider
//! API keys, parity with `prime-agent`'s own `auth.json`. Global only,
//! same cwd-visibility reason `settings.json`/`skills::discover`/
//! `session::read_context_file` are: the daemon process (the only one
//! that ever reads this file, via `rp_server::ensure_running`) has no
//! access to a CLI caller's own cwd.
//!
//! Precedence, highest wins: an already-set env var (`OPENAI_API_KEY`
//! etc.) beats an `auth.json` entry for that same provider, which beats
//! being unconfigured at all -- the same "env var wins" order
//! `settings.json`'s own compaction-threshold overrides established,
//! just for a different pair of tiers (there's no hardcoded default for
//! an API key to fall back to). An `auth.json` entry's `key` is either a
//! literal string, used as-is, or a string prefixed with `!`, whose
//! remainder is run as a shell command and whose trimmed stdout becomes
//! the key -- e.g. `"!security find-generic-password -w -s my-service"`
//! on macOS. Malformed or missing JSON reads as "no entries" rather
//! than a hard error, the same permissive stance `settings::load`
//! already takes.
//!
//! Executing a command named in a config file this project's own single
//! local user controls is the same trust model `session_autonomous
//! --quality-gate` and recursive subagents already accept (see
//! `PARITY.md`): no sandboxing, because there is exactly one caller this
//! whole process ever trusts. Bounded with a short timeout (unlike
//! `--quality-gate`, this runs synchronously on `rp-server` sidecar
//! startup, which already budgets `wait_for_health` -- an interactive
//! prompt a caller forgot they'd configured, e.g. a GUI Keychain dialog,
//! must not hang daemon startup indefinitely) and never memoized beyond
//! what already falls out of `rp_server::ensure_running` itself only
//! running `write_config` once per sidecar lifetime, not per-session or
//! per-prompt.
//!
//! Resolution deliberately never happens in [`crate::rp_server::
//! known_providers`] (`harness model list`, no daemon involved): that
//! function only checks whether an `auth.json` entry *exists* for a
//! provider, the same presence-only check it already does for env vars,
//! so a plain `harness model list` never runs an arbitrary command as a
//! side effect of listing.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::error::{Context, HarnessError, Result};

/// How long a `!`-prefixed key command gets before this project gives up
/// on it -- generous enough for a real credential-manager round trip
/// (network-backed secret stores, a `sudo` prompt), short enough that a
/// forgotten interactive prompt doesn't hang sidecar startup
/// indefinitely.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ProviderAuth {
    pub key: String,
}

pub type Auth = HashMap<String, ProviderAuth>;

/// Reads and parses `<state_root>/auth.json`. Never fails: a missing
/// file, an unreadable one, or one that isn't valid JSON all read the
/// same as "no entries" -- see this module's own doc comment.
pub fn load(state_root: &Path) -> Auth {
    std::fs::read_to_string(state_root.join("auth.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Resolves one `auth.json` `key` value: a literal string as-is, or (a
/// string prefixed with `!`) the trimmed stdout of running the rest as a
/// shell command (`sh -c` on Unix, `cmd /C` on Windows -- the same
/// cross-platform split `client::run_quality_gate` already uses for an
/// identical "arbitrary shell command from user config" need). A
/// non-zero exit, or exceeding [`RESOLVE_TIMEOUT`], is a loud error, not
/// a silent "unconfigured" -- a caller who wrote a `!command` clearly
/// expects it to work, unlike a provider with no `auth.json` entry at
/// all.
pub async fn resolve_key(raw: &str) -> Result<String> {
    let Some(command) = raw.strip_prefix('!') else {
        return Ok(raw.to_string());
    };

    let mut cmd = if cfg!(windows) {
        let mut c = rusty_tokio::process::Command::new("cmd");
        c.args(["/C", command]);
        c
    } else {
        let mut c = rusty_tokio::process::Command::new("sh");
        c.args(["-c", command]);
        c
    };

    let output = rusty_tokio::time::timeout(RESOLVE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            HarnessError::conflict(
                Context::Provider,
                format!("auth.json key command timed out after {RESOLVE_TIMEOUT:?}: {command}"),
            )
        })?
        .map_err(|e| HarnessError::io(Context::Provider, None, e))?;

    if !output.status.success() {
        return Err(HarnessError::conflict(
            Context::Provider,
            format!(
                "auth.json key command exited with {:?}: {command}\nstderr: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state_root(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rpa-auth-test-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_auth_file_reads_as_no_entries() {
        let root = temp_state_root("missing");
        assert!(load(&root).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn malformed_json_reads_as_no_entries() {
        let root = temp_state_root("malformed");
        std::fs::write(root.join("auth.json"), "{ not json").unwrap();
        assert!(load(&root).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn parses_a_literal_key_entry() {
        let root = temp_state_root("literal");
        std::fs::write(
            root.join("auth.json"),
            r#"{"openai": {"key": "sk-literal-123"}}"#,
        )
        .unwrap();
        let auth = load(&root);
        assert_eq!(auth.get("openai").unwrap().key, "sk-literal-123");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[rusty_tokio::test]
    async fn resolve_key_returns_a_literal_string_unchanged() {
        assert_eq!(
            resolve_key("sk-literal-123").await.unwrap(),
            "sk-literal-123"
        );
    }

    #[rusty_tokio::test]
    async fn resolve_key_runs_a_bang_prefixed_command_and_trims_stdout() {
        let resolved = resolve_key("!echo sk-from-command").await.unwrap();
        assert_eq!(resolved, "sk-from-command");
    }

    #[rusty_tokio::test]
    async fn resolve_key_reports_a_failing_command_loudly() {
        let err = resolve_key("!exit 1").await.unwrap_err();
        assert!(err.to_string().contains("exited with"), "got: {err}");
    }
}
