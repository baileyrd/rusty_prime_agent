//! Sidecar lifecycle for [`rusty_provider`](https://github.com/baileyrd/rusty_provider)'s
//! `rp-server` -- the OpenAI-compatible provider router
//! `provider::RustyProviderModel` talks to over HTTP, per `PARITY.md`'s
//! "real `ModelProvider` backend" entry. Not a library dependency:
//! `rp-server` is `rusty_provider`'s own axum/tokio HTTP server binary,
//! built on the real `tokio` rather than this project's `rusty_tokio` --
//! embedding it as a library would mean two async runtimes competing in
//! one process. Spawning it as a separate, HTTP-addressable process keeps
//! the boundary the same shape as every other external thing this
//! project already treats as "a service to call," and mirrors
//! `worker::spawn`'s own detached-process pattern.
//!
//! Owned by the supervisor, not each worker: `daemon::Supervisor` calls
//! [`ensure_running`] lazily, the first time any `SessionNew` (or a
//! recovery respawn) actually needs a real model (`EchoProvider` stays
//! the default and never touches this module at all). Guarded implicitly
//! by `daemon.sock`'s own bind-exclusivity the same way `Supervisor`
//! itself is a singleton per state root -- no separate lock needed, and
//! [`ensure_running`] is itself idempotent (adopts an already-healthy
//! sidecar rather than double-spawning) for the ordinary case of several
//! sessions in a row each needing a real model. Each worker process
//! (spawned separately) reads the resulting port back via [`read_port`]
//! rather than spawning its own sidecar, so N sessions share one
//! `rp-server` instead of one each.
//!
//! [`write_config`] activates a `[providers.*]` block for every provider
//! this process's own environment has a real API key for
//! (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`,
//! `GROQ_API_KEY`), plus `[providers.ollama]` unconditionally (Ollama
//! needs no real key) -- `rusty_tokio::process::Command` inherits this
//! process's full environment by default, so whichever of those vars the
//! daemon was started with reach `rp-server` without this module having
//! to forward them one at a time. [`resolve_auth_env`] extends this with
//! `<state_root>/auth.json` (see the `auth` module) for whichever of
//! those same env vars *aren't* already set -- resolved values are
//! handed to the spawned `rp-server` child directly (`Command::env`),
//! never `std::env::set_var`'d onto this process itself. A session's own `--model
//! provider/model` string picks which of these `rp-server`'s router
//! actually dispatches to per request; a provider with no key configured
//! simply isn't a valid choice (`rp-server` itself reports that, as a
//! 4xx from the chat-completions call, not something this module
//! pre-validates).

use std::path::Path;
use std::time::Duration;

use rusty_tokio::process::{Command, Stdio};

use crate::error::{Context, HarnessError, Result};
use crate::http_client;
use crate::paths;
use crate::procutil;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ProviderState {
    port: u16,
    pid: u32,
}

fn rp_server_bin() -> std::ffi::OsString {
    std::env::var_os("RUSTY_PRIME_AGENT_RP_SERVER_BIN").unwrap_or_else(|| "rp-server".into())
}

/// Whether [`rp_server_bin`] can actually be found -- used by `harness
/// doctor` to report a real, otherwise-silent gap ahead of time (today,
/// a missing `rp-server` only ever surfaces as a spawn error the first
/// time `ensure_running` needs it). Deliberately doesn't *run*
/// `rp-server` to check (no `--version`/`--help` probe) -- this project
/// doesn't control that binary's own CLI surface, so invoking it with an
/// arbitrary flag on the strength of a guess would be unsafe; existence
/// on disk is all a health check needs.
///
/// A bin name containing a path separator (an explicit path, e.g. via
/// `RUSTY_PRIME_AGENT_RP_SERVER_BIN=/opt/rp-server/bin/rp-server`, the
/// same override this whole module's own tests use to avoid depending
/// on a real `rp-server` install) is checked directly, the same
/// "used as-is, not `PATH`-searched" rule every shell already follows
/// for a name that isn't bare. A bare name is searched across `PATH`
/// (`std::env::split_paths`), trying the bare name and (on Windows,
/// where executables conventionally carry an extension the bare name
/// might omit) `<name>.exe` in each directory.
pub(crate) fn rp_server_available() -> bool {
    let bin = rp_server_bin();
    let bin_path = Path::new(&bin);
    if bin_path.components().count() > 1 {
        return bin_path.is_file();
    }
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| {
        dir.join(&bin).is_file()
            || (cfg!(windows) && dir.join(format!("{}.exe", bin.to_string_lossy())).is_file())
    })
}

/// Where `[providers.ollama]` in the generated config points -- Ollama's
/// own OpenAI-compatible endpoint, not `rp-server`'s. Configurable since
/// a real deployment might run Ollama somewhere other than this same
/// host's default port.
fn ollama_base_url() -> String {
    std::env::var("RUSTY_PRIME_AGENT_OLLAMA_BASE_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:11434/v1".to_string())
}

fn read_state(state_root: &Path) -> Option<ProviderState> {
    let text = std::fs::read_to_string(paths::provider_state_path(state_root)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_state(state_root: &Path, state: &ProviderState) -> Result<()> {
    let path = paths::provider_state_path(state_root);
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| HarnessError::json(Context::Provider, Some(path.clone()), e))?;
    std::fs::write(&path, json).map_err(|e| HarnessError::io(Context::Provider, Some(path), e))
}

/// A free port picked by asking the OS for one (bind `:0`, read it back,
/// drop the listener before `rp-server` binds it itself). The same
/// bind-then-release pattern every ephemeral-port allocator uses; the
/// brief window between drop and `rp-server`'s own bind is not
/// meaningfully different from the race any such allocator accepts.
fn pick_free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| HarnessError::io(Context::Provider, None, e))?;
    listener
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|e| HarnessError::io(Context::Provider, None, e))
}

/// `(provider name, base_url, api_key_env)` for every non-Ollama backend
/// this module knows how to activate, gated on that env var actually
/// being set in this process's own environment -- see this module's own
/// doc comment for why that's sufficient to reach the spawned
/// `rp-server` too.
const OPTIONAL_PROVIDERS: &[(&str, &str, &str, &str)] = &[
    (
        "openai",
        "https://api.openai.com/v1",
        "OPENAI_API_KEY",
        "openai",
    ),
    (
        "anthropic",
        "https://api.anthropic.com",
        "ANTHROPIC_API_KEY",
        "anthropic",
    ),
    (
        "gemini",
        "https://generativelanguage.googleapis.com",
        "GEMINI_API_KEY",
        "gemini",
    ),
    (
        "groq",
        "https://api.groq.com/openai/v1",
        "GROQ_API_KEY",
        "openai",
    ),
];

/// One resolved provider `write_config`/`known_providers`/
/// `resolve_auth_env` can act on, whether it came from the hardcoded
/// [`OPTIONAL_PROVIDERS`] or a user's own `<state_root>/providers.json`
/// -- see [`all_providers`].
struct ProviderEntry {
    name: String,
    base_url: String,
    api_key_env: String,
    kind: String,
}

/// The env var name a registered custom provider's key is looked for
/// under, absent an explicit override -- `<NAME>_API_KEY` with every
/// non-alphanumeric character (hyphens are the common case: `my-vllm`)
/// folded to `_`, the same shape every built-in `OPTIONAL_PROVIDERS`
/// entry's own `*_API_KEY` var already has.
fn custom_provider_api_key_env(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("{}_API_KEY", sanitized.to_ascii_uppercase())
}

/// Merges [`OPTIONAL_PROVIDERS`] with `<state_root>/providers.json`
/// (`providers::load`, see that module's own doc comment for why an
/// arbitrary provider *name* is exactly the mechanism `rp-server`'s real
/// router already supports) into the one list every provider-aware
/// function in this module iterates. A custom entry reusing a reserved
/// name (one of `OPTIONAL_PROVIDERS`'s own names, or `"ollama"`, which
/// `write_config` always appends as its own unconditional block) is
/// silently dropped rather than an error -- the built-in wins, the same
/// permissive "an ambiguous config entry is ignored, not fatal" stance
/// `settings.json`'s own unknown-field handling already takes.
fn all_providers(state_root: &Path) -> Vec<ProviderEntry> {
    let mut entries: Vec<ProviderEntry> = OPTIONAL_PROVIDERS
        .iter()
        .map(|(name, base_url, api_key_env, kind)| ProviderEntry {
            name: name.to_string(),
            base_url: base_url.to_string(),
            api_key_env: api_key_env.to_string(),
            kind: kind.to_string(),
        })
        .collect();
    for (name, custom) in crate::providers::load(state_root) {
        if name == "ollama" || entries.iter().any(|e| e.name == name) {
            continue;
        }
        entries.push(ProviderEntry {
            api_key_env: custom_provider_api_key_env(&name),
            base_url: custom.base_url,
            kind: custom.kind,
            name,
        });
    }
    entries
}

/// One entry of the provider catalog (`harness model list`) -- bounded
/// parity with `prime-agent model list`'s catalog browse: which
/// backends `write_config` would activate given this process's own
/// environment, not each one's actual per-model IDs. Listing real model
/// IDs needs a live query against each provider's own API (`GET /v1/
/// models` or equivalent) -- untestable in CI (no real API keys there)
/// and not attempted here, see `PARITY.md`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderInfo {
    pub name: String,
    pub configured: bool,
}

/// Every provider name `write_config` could ever activate -- every
/// `OPTIONAL_PROVIDERS` entry plus whatever `<state_root>/providers.json`
/// registers (see [`all_providers`]) -- plus whether this process's own
/// environment (or `<state_root>/auth.json`) configures it right now.
/// Exactly the same env-var-or-auth.json-entry check [`ensure_running`]
/// itself uses, so this can never drift from what a real `session new
/// --model <name>/...` would actually be able to reach. Ollama is always
/// `configured: true`: it needs no real key (see `write_config`'s own
/// doc comment).
///
/// Deliberately only checks whether an `auth.json` entry *exists*, the
/// same presence-only check already used for env vars -- never resolves
/// a `!command` entry, so a plain `harness model list` (no daemon
/// involved) never runs an arbitrary command as a side effect of
/// listing. See `auth`'s own module doc comment.
pub fn known_providers(state_root: &Path) -> Vec<ProviderInfo> {
    let auth = crate::auth::load(state_root);
    let mut providers: Vec<ProviderInfo> = all_providers(state_root)
        .into_iter()
        .map(|entry| ProviderInfo {
            configured: std::env::var_os(&entry.api_key_env).is_some()
                || auth.contains_key(&entry.name),
            name: entry.name,
        })
        .collect();
    providers.push(ProviderInfo {
        name: "ollama".to_string(),
        configured: true,
    });
    providers
}

/// One entry of `rp-server`'s real per-model catalog (`GET /v1/models`,
/// `harness model list --detailed`) -- unlike [`ProviderInfo`], these
/// are actual model IDs/pricing/context length, sourced from
/// `rp-server`'s own `route_aliases()`/`configured_providers()`/
/// `priced_models()` rather than an env-var check here. `pricing` is
/// left as a passthrough `serde_json::Value` (its shape varies by
/// provider and isn't this project's concern to model precisely) --
/// `#[serde(default)]` on both optional fields since `rp-server` omits
/// them for a model it has no priced/context-length data for.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelCatalogEntry {
    pub id: String,
    pub owned_by: String,
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub pricing: Option<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize)]
struct ModelListResponse {
    data: Vec<ModelCatalogEntry>,
}

/// Queries a running `rp-server` sidecar's real per-model catalog.
/// Callers are expected to have already called [`ensure_running`] to
/// get `port` -- this function doesn't spawn anything itself, matching
/// `provider::RustyProviderModel`'s own "just an HTTP client" shape.
pub async fn fetch_model_catalog(port: u16) -> Result<Vec<ModelCatalogEntry>> {
    let (status, body) = http_client::get(port, "/v1/models").await?;
    if status != 200 {
        return Err(HarnessError::conflict(
            Context::Provider,
            format!("rp-server returned {status} for GET /v1/models: {body}"),
        ));
    }
    let parsed: ModelListResponse =
        serde_json::from_str(&body).map_err(|e| HarnessError::json(Context::Provider, None, e))?;
    Ok(parsed.data)
}

/// For every provider `all_providers` returns whose env var isn't
/// already set in this process's own environment, checks
/// `<state_root>/auth.json` for an entry (keyed by that same provider
/// name -- `auth.rs` needed no changes at all for custom providers to
/// work, see `providers`' own module doc comment) and resolves it (see
/// `auth::resolve_key`) -- an env var already being set always wins,
/// `auth.json` never consulted in that case, the same precedence
/// `settings.json`'s own overrides established for a different pair of
/// tiers. Returns `(api_key_env, resolved_key)` pairs only for what
/// `auth.json` actually configured; the daemon's own process environment
/// is never mutated -- callers hand these to the spawned `rp-server`
/// child's own `Command::env` instead (see `ensure_running`), so a
/// caller who never restarts the daemon still sees a
/// same-process-lifetime `auth.json` edit take effect on the next
/// sidecar spawn, without a stray global env var leaking anywhere else.
async fn resolve_auth_env(state_root: &Path) -> Result<Vec<(String, String)>> {
    let auth = crate::auth::load(state_root);
    let mut resolved = Vec::new();
    for entry in all_providers(state_root) {
        if std::env::var_os(&entry.api_key_env).is_some() {
            continue;
        }
        if let Some(provider_auth) = auth.get(&entry.name) {
            let key = crate::auth::resolve_key(&provider_auth.key).await?;
            resolved.push((entry.api_key_env, key));
        }
    }
    Ok(resolved)
}

fn write_config(state_root: &Path, port: u16, resolved_env: &[(String, String)]) -> Result<()> {
    let path = paths::provider_config_path(state_root);
    let mut toml = format!("[server]\nhost = \"127.0.0.1\"\nport = {port}\n");
    for entry in all_providers(state_root) {
        let configured = std::env::var_os(&entry.api_key_env).is_some()
            || resolved_env.iter().any(|(k, _)| *k == entry.api_key_env);
        if !configured {
            continue;
        }
        toml.push_str(&format!(
            "\n[providers.{}]\nkind = \"{}\"\nbase_url = \"{}\"\napi_key_env = \"{}\"\n",
            entry.name, entry.kind, entry.base_url, entry.api_key_env
        ));
    }
    // Ollama unconditionally: it needs no real key, and this project's
    // own OllamaProvider-shaped testing (`tests/ollama_provider.rs`)
    // depends on it always being available regardless of which other
    // providers happen to be configured.
    toml.push_str(&format!(
        "\n[providers.ollama]\nkind = \"openai\"\nbase_url = \"{}\"\napi_key_env = \"OLLAMA_API_KEY\"\n",
        ollama_base_url()
    ));
    // `[mcp] enabled = true` unconditionally, same reasoning as
    // `[providers.ollama]` above: harmless with no `[[mcp.upstreams]]`
    // configured (`rp-server`'s own docs: "gives you just the native
    // chat_completion/list_models/embeddings tools, no gateway
    // proxying"), and `session new --tools mcp` (`PARITY.md`) needs it
    // on every sidecar this project spawns, not just ones a caller
    // happened to ask for MCP on -- `ensure_running` has no per-session
    // knowledge to gate this on, and the sidecar is shared across every
    // session anyway.
    toml.push_str("\n[mcp]\nenabled = true\n");
    std::fs::write(&path, toml).map_err(|e| HarnessError::io(Context::Provider, Some(path), e))
}

async fn health_check(port: u16) -> bool {
    matches!(http_client::get(port, "/health").await, Ok((200, _)))
}

/// Ensures an `rp-server` sidecar is reachable, spawning one if needed,
/// and returns its port. Idempotent and adopts a still-running sidecar
/// from a previous supervisor generation the same way
/// `Supervisor::recover_on_startup` adopts a still-running worker,
/// instead of always spawning a fresh one on every restart.
pub async fn ensure_running(state_root: &Path) -> Result<u16> {
    if let Some(existing) = read_state(state_root) {
        if health_check(existing.port).await {
            return Ok(existing.port);
        }
        // Stale record from a sidecar that's no longer answering --
        // fall through and spawn a fresh one.
    }

    let port = pick_free_port()?;
    let resolved_env = resolve_auth_env(state_root).await?;
    write_config(state_root, port, &resolved_env)?;

    let log_path = paths::provider_log_path(state_root);
    let log_file = std::fs::File::create(&log_path)
        .map_err(|e| HarnessError::io(Context::Provider, Some(log_path), e))?;

    let mut cmd = Command::new(rp_server_bin());
    cmd.env("CONFIG_PATH", paths::provider_config_path(state_root));
    // Ollama does not check the Authorization header at all -- rp-server
    // still requires *a* value here to activate the provider (a provider
    // whose api_key_env isn't set is skipped at startup), so this is a
    // placeholder, not a real credential.
    cmd.env("OLLAMA_API_KEY", "unused-ollama-key");
    // Keys `auth.json` resolved for a provider whose env var wasn't
    // already set -- handed to the child directly rather than
    // `std::env::set_var`'d onto this (daemon) process, so an
    // `auth.json` edit never needs a daemon restart to take effect and
    // never leaks into anything else this process does.
    for (key, value) in &resolved_env {
        cmd.env(key, value);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file));
    procutil::prepare_detached(&mut cmd);

    let mut child = cmd.spawn().map_err(|e| {
        HarnessError::io(
            Context::Provider,
            Some(std::path::PathBuf::from(rp_server_bin())),
            e,
        )
    })?;
    let pid = child.id();
    // Same zombie-avoidance reasoning as `worker::spawn`'s own reaper
    // task: this process (the supervisor) stays the sidecar's parent
    // until something here calls `wait` on it.
    rusty_tokio::spawn(async move {
        let _ = child.wait().await;
    });

    wait_for_health(port, Duration::from_secs(20)).await?;
    write_state(state_root, &ProviderState { port, pid })?;
    Ok(port)
}

async fn wait_for_health(port: u16, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if health_check(port).await {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(HarnessError::conflict(
                Context::Provider,
                "rp-server did not become healthy in time",
            ));
        }
        rusty_tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Reads back the port [`ensure_running`] recorded, for a worker process
/// (which never calls `ensure_running` itself -- see this module's own
/// doc comment) constructing its own `RustyProviderModel`. `None` if no
/// sidecar has been started for this state root (the ordinary case when
/// no session has ever set a `model`).
pub fn read_port(state_root: &Path) -> Option<u16> {
    read_state(state_root).map(|s| s.port)
}

/// Best-effort: terminates the sidecar recorded for `state_root`, if
/// any, and removes the record. A no-op (not an error) when no sidecar
/// was ever started -- called unconditionally from `daemon shutdown`, the
/// same "safe to call even if unused" shape as every other per-feature
/// cleanup that command does.
pub fn shutdown(state_root: &Path) {
    if let Some(state) = read_state(state_root) {
        let _ = procutil::kill(state.pid);
    }
    let _ = std::fs::remove_file(paths::provider_state_path(state_root));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state_root(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rpa-rp-server-test-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Guards every test below's env-var *mutation* (never held across an
    /// `.await` -- `clippy::await_holding_lock` rightly flags that, and a
    /// `std::sync::Mutex` isn't an async-aware one anyway). Each test
    /// below targets a different `OPTIONAL_PROVIDERS` var (`OPENAI_*`/
    /// `ANTHROPIC_*`/`GEMINI_*`/`GROQ_*`), so there's no real cross-test
    /// race to close here regardless -- this exists purely to match
    /// `session::tests::COMPACT_ENV_GUARD`'s own defensive-consistency
    /// reasoning for a different pair of vars, dropped before the
    /// `resolve_auth_env` call each test actually awaits.
    static PROVIDER_ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[rusty_tokio::test]
    async fn resolve_auth_env_skips_a_provider_whose_env_var_is_already_set() {
        let root = temp_state_root("skip-env-set");
        {
            let _guard = PROVIDER_ENV_GUARD
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::env::set_var("OPENAI_API_KEY", "sk-already-set");
        }
        // A command that would error loudly if it ever actually ran --
        // proves the env var short-circuits before `auth.json` (and its
        // `!`-command) is even consulted, not just that the *result*
        // happens to match.
        std::fs::write(root.join("auth.json"), r#"{"openai": {"key": "!exit 1"}}"#).unwrap();
        let resolved = resolve_auth_env(&root).await.unwrap();
        assert!(
            resolved.iter().all(|(k, _)| k != "OPENAI_API_KEY"),
            "got: {resolved:?}"
        );
        std::env::remove_var("OPENAI_API_KEY");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[rusty_tokio::test]
    async fn resolve_auth_env_resolves_a_literal_key_from_auth_json() {
        let root = temp_state_root("literal-auth-json");
        {
            let _guard = PROVIDER_ENV_GUARD
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
        std::fs::write(
            root.join("auth.json"),
            r#"{"anthropic": {"key": "sk-literal-from-auth-json"}}"#,
        )
        .unwrap();
        let resolved = resolve_auth_env(&root).await.unwrap();
        assert_eq!(
            resolved,
            vec![(
                "ANTHROPIC_API_KEY".to_string(),
                "sk-literal-from-auth-json".to_string()
            )]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[rusty_tokio::test]
    async fn resolve_auth_env_resolves_a_bang_command_key_from_auth_json() {
        let root = temp_state_root("command-auth-json");
        {
            let _guard = PROVIDER_ENV_GUARD
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::env::remove_var("GEMINI_API_KEY");
        }
        std::fs::write(
            root.join("auth.json"),
            r#"{"gemini": {"key": "!echo sk-resolved-via-command"}}"#,
        )
        .unwrap();
        let resolved = resolve_auth_env(&root).await.unwrap();
        assert_eq!(
            resolved,
            vec![(
                "GEMINI_API_KEY".to_string(),
                "sk-resolved-via-command".to_string()
            )]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[rusty_tokio::test]
    async fn resolve_auth_env_propagates_a_failing_command_loudly() {
        let root = temp_state_root("failing-command-auth-json");
        {
            let _guard = PROVIDER_ENV_GUARD
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::env::remove_var("GROQ_API_KEY");
        }
        std::fs::write(root.join("auth.json"), r#"{"groq": {"key": "!exit 1"}}"#).unwrap();
        assert!(resolve_auth_env(&root).await.is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn write_config_activates_a_provider_configured_only_via_resolved_auth_json() {
        let _guard = PROVIDER_ENV_GUARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::remove_var("GROQ_API_KEY");
        let root = temp_state_root("write-config-auth-json");
        let resolved_env = vec![("GROQ_API_KEY".to_string(), "sk-resolved".to_string())];
        write_config(&root, 12345, &resolved_env).unwrap();
        let toml = std::fs::read_to_string(paths::provider_config_path(&root)).unwrap();
        assert!(toml.contains("[providers.groq]"), "got: {toml}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn write_config_skips_a_provider_configured_by_neither_env_nor_auth_json() {
        let _guard = PROVIDER_ENV_GUARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::remove_var("GROQ_API_KEY");
        let root = temp_state_root("write-config-unconfigured");
        write_config(&root, 12345, &[]).unwrap();
        let toml = std::fs::read_to_string(paths::provider_config_path(&root)).unwrap();
        assert!(!toml.contains("[providers.groq]"), "got: {toml}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn custom_provider_api_key_env_uppercases_and_folds_non_alphanumerics() {
        assert_eq!(custom_provider_api_key_env("my-vllm"), "MY_VLLM_API_KEY");
        assert_eq!(
            custom_provider_api_key_env("company.proxy"),
            "COMPANY_PROXY_API_KEY"
        );
    }

    #[test]
    fn all_providers_includes_a_registered_custom_provider() {
        let root = temp_state_root("all-providers-custom");
        std::fs::write(
            root.join("providers.json"),
            r#"{"my-vllm": {"base_url": "http://127.0.0.1:8000/v1"}}"#,
        )
        .unwrap();
        let entries = all_providers(&root);
        let custom = entries.iter().find(|e| e.name == "my-vllm").unwrap();
        assert_eq!(custom.base_url, "http://127.0.0.1:8000/v1");
        assert_eq!(custom.kind, "openai");
        assert_eq!(custom.api_key_env, "MY_VLLM_API_KEY");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A custom entry reusing a reserved name (a built-in
    /// `OPTIONAL_PROVIDERS` name, or `"ollama"`) is silently dropped --
    /// the built-in's own `base_url` survives untouched, proving the
    /// built-in wins rather than the custom entry overriding it or the
    /// two colliding into a duplicate `[providers.groq]` TOML table
    /// (which `rp-server`'s own TOML parser would reject).
    #[test]
    fn all_providers_drops_a_custom_entry_reusing_a_reserved_name() {
        let root = temp_state_root("all-providers-reserved");
        std::fs::write(
            root.join("providers.json"),
            r#"{
                "groq": {"base_url": "http://should-not-win.example.com"},
                "ollama": {"base_url": "http://should-not-win.example.com"}
            }"#,
        )
        .unwrap();
        let entries = all_providers(&root);
        assert_eq!(entries.iter().filter(|e| e.name == "groq").count(), 1);
        let groq = entries.iter().find(|e| e.name == "groq").unwrap();
        assert_eq!(groq.base_url, "https://api.groq.com/openai/v1");
        assert!(
            entries.iter().all(|e| e.name != "ollama"),
            "ollama is appended separately by write_config, never through all_providers"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn known_providers_lists_a_registered_custom_provider() {
        let root = temp_state_root("known-providers-custom");
        std::fs::write(
            root.join("providers.json"),
            r#"{"my-vllm": {"base_url": "http://127.0.0.1:8000/v1"}}"#,
        )
        .unwrap();
        let providers = known_providers(&root);
        let custom = providers
            .iter()
            .find(|p| p.name == "my-vllm")
            .expect("my-vllm listed");
        assert!(!custom.configured, "no key configured yet");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[rusty_tokio::test]
    async fn resolve_auth_env_resolves_a_custom_providers_auth_json_entry() {
        let root = temp_state_root("resolve-auth-env-custom");
        {
            let _guard = PROVIDER_ENV_GUARD
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::env::remove_var("MY_VLLM_API_KEY");
        }
        std::fs::write(
            root.join("providers.json"),
            r#"{"my-vllm": {"base_url": "http://127.0.0.1:8000/v1"}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("auth.json"),
            r#"{"my-vllm": {"key": "sk-custom-literal"}}"#,
        )
        .unwrap();
        let resolved = resolve_auth_env(&root).await.unwrap();
        assert_eq!(
            resolved,
            vec![(
                "MY_VLLM_API_KEY".to_string(),
                "sk-custom-literal".to_string()
            )]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn write_config_activates_a_registered_custom_provider_with_its_own_kind() {
        let root = temp_state_root("write-config-custom");
        std::fs::write(
            root.join("providers.json"),
            r#"{"my-vllm": {"base_url": "http://127.0.0.1:8000/v1", "kind": "openai"}}"#,
        )
        .unwrap();
        let resolved_env = vec![("MY_VLLM_API_KEY".to_string(), "sk-resolved".to_string())];
        write_config(&root, 12345, &resolved_env).unwrap();
        let toml = std::fs::read_to_string(paths::provider_config_path(&root)).unwrap();
        assert!(toml.contains("[providers.my-vllm]"), "got: {toml}");
        assert!(
            toml.contains("base_url = \"http://127.0.0.1:8000/v1\""),
            "got: {toml}"
        );
        assert!(
            toml.contains("api_key_env = \"MY_VLLM_API_KEY\""),
            "got: {toml}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
