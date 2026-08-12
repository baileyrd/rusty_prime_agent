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
//! to forward them one at a time. A session's own `--model
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
const OPTIONAL_PROVIDERS: &[(&str, &str, &str)] = &[
    ("openai", "https://api.openai.com/v1", "OPENAI_API_KEY"),
    (
        "anthropic",
        "https://api.anthropic.com",
        "ANTHROPIC_API_KEY",
    ),
    (
        "gemini",
        "https://generativelanguage.googleapis.com",
        "GEMINI_API_KEY",
    ),
    ("groq", "https://api.groq.com/openai/v1", "GROQ_API_KEY"),
];

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

/// Every provider name `write_config` could ever activate, plus whether
/// this process's own environment configures it right now -- exactly
/// the same `OPTIONAL_PROVIDERS`/env-var check `write_config` itself
/// uses, so this can never drift from what a real `session new --model
/// <name>/...` would actually be able to reach. Ollama is always
/// `configured: true`: it needs no real key (see `write_config`'s own
/// doc comment).
pub fn known_providers() -> Vec<ProviderInfo> {
    let mut providers: Vec<ProviderInfo> = OPTIONAL_PROVIDERS
        .iter()
        .map(|(name, _base_url, api_key_env)| ProviderInfo {
            name: name.to_string(),
            configured: std::env::var_os(api_key_env).is_some(),
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

fn write_config(state_root: &Path, port: u16) -> Result<()> {
    let path = paths::provider_config_path(state_root);
    let mut toml = format!("[server]\nhost = \"127.0.0.1\"\nport = {port}\n");
    for (name, base_url, api_key_env) in OPTIONAL_PROVIDERS {
        if std::env::var_os(api_key_env).is_none() {
            continue;
        }
        toml.push_str(&format!(
            "\n[providers.{name}]\nkind = \"openai\"\nbase_url = \"{base_url}\"\napi_key_env = \"{api_key_env}\"\n"
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
    write_config(state_root, port)?;

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
