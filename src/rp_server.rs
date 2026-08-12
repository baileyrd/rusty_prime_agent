//! Sidecar lifecycle for [`rusty_provider`](https://github.com/baileyrd/rusty_provider)'s
//! `rp-server` -- the OpenAI-compatible provider router `provider::OllamaProvider`
//! talks to over HTTP, per `PARITY.md`'s "real `ModelProvider` backend"
//! entry. Not a library dependency: `rp-server` is `rusty_provider`'s own
//! axum/tokio HTTP server binary, built on the real `tokio` rather than
//! this project's `rusty_tokio` -- embedding it as a library would mean
//! two async runtimes competing in one process. Spawning it as a
//! separate, HTTP-addressable process keeps the boundary the same shape
//! as every other external thing this project already treats as "a
//! service to call," and mirrors `worker::spawn`'s own detached-process
//! pattern.
//!
//! Owned by the supervisor, not each worker: `daemon::run` calls
//! [`ensure_running`] once at startup (gated on `RUSTY_PRIME_AGENT_PROVIDER
//! =ollama`; `EchoProvider` stays the default), guarded implicitly by
//! `daemon.sock`'s own bind-exclusivity the same way `Supervisor` itself
//! is a singleton per state root -- no separate lock needed. Each worker
//! process (spawned separately, after the supervisor's own startup
//! completes) reads the resulting port back via [`read_port`] rather than
//! spawning its own sidecar, so N sessions share one `rp-server` instead
//! of one each.

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

fn write_config(state_root: &Path, port: u16) -> Result<()> {
    let path = paths::provider_config_path(state_root);
    let toml = format!(
        "[server]\n\
         host = \"127.0.0.1\"\n\
         port = {port}\n\
         \n\
         [providers.ollama]\n\
         kind = \"openai\"\n\
         base_url = \"{}\"\n\
         api_key_env = \"OLLAMA_API_KEY\"\n",
        ollama_base_url()
    );
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
/// doc comment) constructing its own `OllamaProvider`. `None` if no
/// sidecar has been started for this state root (the ordinary case when
/// `RUSTY_PRIME_AGENT_PROVIDER` isn't `ollama`).
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
