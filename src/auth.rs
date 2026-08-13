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
//! A third `key` form, parity with `prime-agent`'s own named-env-var-
//! lookup shape (`{"key": "MY_KEY"}` meaning "read this env var", not
//! "use the literal string `MY_KEY`"): a value that looks like a bare
//! identifier (`^[A-Za-z_][A-Za-z0-9_]*$`, not `!`-prefixed) is tried
//! against `std::env::var` first, falling back to the literal string
//! only if no such env var is actually set. This is deliberately
//! conservative -- a real API key almost never has that exact shape
//! (`sk-...`/`sk-ant-...`/etc. all contain a hyphen, which fails the
//! identifier check outright), and even one that happens to match only
//! changes behavior if an env var of that exact name is also set, which
//! was already true before this indirection existed (env var always
//! wins over `auth.json` regardless).
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

use serde::{Deserialize, Serialize};

use crate::error::{Context, HarnessError, Result};

/// How long a `!`-prefixed key command gets before this project gives up
/// on it -- generous enough for a real credential-manager round trip
/// (network-backed secret stores, a `sudo` prompt), short enough that a
/// forgotten interactive prompt doesn't hang sidecar startup
/// indefinitely.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
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

/// Resolves one `auth.json` `key` value: a string prefixed with `!` runs
/// the rest as a shell command and uses its trimmed stdout (see below);
/// otherwise, if the whole value looks like a bare env-var-name
/// identifier and that env var is actually set, its value is used (see
/// this module's own doc comment for the named-env-var-lookup form);
/// otherwise the value is used as a literal string, unchanged. A
/// non-zero `!command` exit, or exceeding [`RESOLVE_TIMEOUT`], is a loud
/// error, not a silent "unconfigured" -- a caller who wrote a `!command`
/// clearly expects it to work, unlike a provider with no `auth.json`
/// entry at all.
pub async fn resolve_key(raw: &str) -> Result<String> {
    let Some(command) = raw.strip_prefix('!') else {
        if looks_like_env_var_name(raw) {
            if let Ok(value) = std::env::var(raw) {
                return Ok(value);
            }
        }
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

/// Whether `raw` has the shape of a bare env-var-name identifier
/// (`^[A-Za-z_][A-Za-z0-9_]*$`) -- the trigger [`resolve_key`] uses to
/// even attempt the named-env-var-lookup form, before checking whether
/// an env var of that name is actually set. Deliberately not
/// case-restricted (a real env var name is conventionally
/// `SCREAMING_SNAKE_CASE`, but nothing stops a caller from configuring
/// or reading a lowercase one) -- the actual safety net is that
/// `std::env::var` still has to succeed for this to change anything.
fn looks_like_env_var_name(raw: &str) -> bool {
    let mut chars = raw.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Writes (inserting or overwriting) one `<provider>` entry in
/// `<state_root>/auth.json` -- the save half of `client::session_repl`'s
/// `/login` wizard (see that module's own doc comment for the full
/// design story, including why this is an interactive `auth.json` editor
/// rather than a real OAuth client). `key` is stored as a literal string,
/// never a `!command`; a caller who wants shell-command resolution can
/// still hand-edit the file afterward, same as before this function
/// existed -- `/login` only ever needs to write what a human just typed.
/// Every other entry already in the file is preserved (`load`'s own
/// permissive "malformed/missing reads as empty" behavior means a
/// corrupt file is silently replaced with just this one entry, the same
/// tradeoff `load` already accepts elsewhere).
pub fn write_key(state_root: &Path, provider: &str, key: &str) -> Result<()> {
    let mut auth = load(state_root);
    auth.insert(
        provider.to_string(),
        ProviderAuth {
            key: key.to_string(),
        },
    );
    let path = state_root.join("auth.json");
    let json = serde_json::to_string_pretty(&auth)
        .map_err(|e| HarnessError::json(Context::Provider, None, e))?;
    std::fs::write(&path, json).map_err(|e| HarnessError::io(Context::Provider, Some(path), e))
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

    #[test]
    fn looks_like_env_var_name_accepts_identifiers_and_rejects_everything_else() {
        assert!(looks_like_env_var_name("MY_KEY"));
        assert!(looks_like_env_var_name("_leading_underscore"));
        assert!(looks_like_env_var_name("lowercase_ok_too"));
        assert!(!looks_like_env_var_name(""));
        assert!(!looks_like_env_var_name("sk-literal-123"));
        assert!(!looks_like_env_var_name("has space"));
        assert!(!looks_like_env_var_name("9starts_with_digit"));
    }

    /// Guards the two tests below: both set/clear the same process-wide
    /// env var, the same "can't run concurrently under `cargo test`'s
    /// default parallelism" reasoning `session.rs`'s own `COMPACT_ENV_
    /// GUARD`/`rp_server.rs`'s own `PROVIDER_ENV_GUARD` already document
    /// -- dropped before the `resolve_key` call each test actually
    /// awaits, same as `PROVIDER_ENV_GUARD`'s own tests, so a
    /// `std::sync::MutexGuard` is never held across an `.await`.
    static ENV_VAR_INDIRECTION_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[rusty_tokio::test]
    async fn resolve_key_looks_up_a_named_env_var_when_it_is_set() {
        {
            let _guard = ENV_VAR_INDIRECTION_GUARD
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::env::set_var("RPA_TEST_AUTH_ENV_INDIRECTION", "value-from-env");
        }
        assert_eq!(
            resolve_key("RPA_TEST_AUTH_ENV_INDIRECTION").await.unwrap(),
            "value-from-env"
        );
        std::env::remove_var("RPA_TEST_AUTH_ENV_INDIRECTION");
    }

    #[rusty_tokio::test]
    async fn resolve_key_falls_back_to_the_literal_when_the_named_env_var_is_unset() {
        {
            let _guard = ENV_VAR_INDIRECTION_GUARD
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::env::remove_var("RPA_TEST_AUTH_ENV_INDIRECTION_UNSET");
        }
        assert_eq!(
            resolve_key("RPA_TEST_AUTH_ENV_INDIRECTION_UNSET")
                .await
                .unwrap(),
            "RPA_TEST_AUTH_ENV_INDIRECTION_UNSET"
        );
    }

    #[test]
    fn write_key_creates_the_file_when_none_existed() {
        let root = temp_state_root("write-new");
        write_key(&root, "openai", "sk-new-123").unwrap();
        let auth = load(&root);
        assert_eq!(auth.get("openai").unwrap().key, "sk-new-123");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn write_key_preserves_other_providers_already_present() {
        let root = temp_state_root("write-preserve");
        std::fs::write(
            root.join("auth.json"),
            r#"{"anthropic": {"key": "sk-anthropic-old"}}"#,
        )
        .unwrap();
        write_key(&root, "openai", "sk-openai-new").unwrap();
        let auth = load(&root);
        assert_eq!(auth.get("anthropic").unwrap().key, "sk-anthropic-old");
        assert_eq!(auth.get("openai").unwrap().key, "sk-openai-new");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn write_key_overwrites_an_existing_entry_for_the_same_provider() {
        let root = temp_state_root("write-overwrite");
        write_key(&root, "openai", "sk-old").unwrap();
        write_key(&root, "openai", "sk-new").unwrap();
        let auth = load(&root);
        assert_eq!(auth.get("openai").unwrap().key, "sk-new");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
