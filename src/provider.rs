//! Model provider stub (Phase 1 non-goal: "Model provider clients --
//! stub with a fake provider that echoes turns"). Real provider clients
//! (streaming, tool calls, retries) are Phase 2's job; this exists only
//! so `AgentSession` has something to call that proves the prompt ->
//! response -> transcript -> event-stream pipeline works end to end.
//!
//! [`RustyProviderModel`] is the one exception, per `PARITY.md`'s "real
//! `ModelProvider` backend" entry: a genuine, network-calling backend,
//! opt-in per session via `session new --model provider/id` / `-p
//! --model provider/id` (parity with `prime-agent --model provider/id`;
//! `EchoProvider` stays the default when no `--model` is given), routed
//! through the `rp_server` sidecar rather than calling any provider's API
//! directly -- see that module's own doc comment for why. Despite the
//! name, not Ollama-specific: `model` is the same `"provider/model"`
//! string `rusty_provider`'s router itself uses (`"ollama/qwen2.5:0.5b"`,
//! `"anthropic/claude-sonnet-5"`, `"openai/gpt-4o-mini"`, ...) -- this
//! type is a thin HTTP client for whichever provider that string names,
//! not a provider itself.

use crate::error::{Context, HarnessError, Result};
use crate::http_client;
use crate::tool_runtime::BoxFuture;

/// `Send + Sync`, like `ToolRuntime`: `AgentSession` holds this behind a
/// `rusty_tokio::sync::Mutex` shared across every connection handler
/// task the worker spawns, so the compiler needs `&AgentSession`
/// (reachable through the mutex guard) to itself be `Send` across an
/// `.await` -- which requires `AgentSession`, and therefore this field,
/// to be `Sync`.
pub trait ModelProvider: Send + Sync {
    fn respond<'a>(&'a mut self, prompt: &'a str) -> BoxFuture<'a, Result<String>>;
}

/// Echoes each prompt back verbatim, prefixed so a transcript reader can
/// tell an echoed reply from a real model response at a glance.
#[derive(Debug, Default)]
pub struct EchoProvider;

impl ModelProvider for EchoProvider {
    fn respond<'a>(&'a mut self, prompt: &'a str) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move { Ok(format!("echo: {prompt}")) })
    }
}

/// Calls a running `rp-server` sidecar's `POST /v1/chat/completions` --
/// see `rp_server`'s own doc comment for the sidecar-process boundary
/// this crosses. `model` is the `"provider/model"` string `rp-server`'s
/// router expects, e.g. `"ollama/qwen2.5:0.5b"`, `"anthropic/claude-sonnet-5"`.
#[derive(Debug, Clone)]
pub struct RustyProviderModel {
    port: u16,
    model: String,
}

impl RustyProviderModel {
    pub fn new(port: u16, model: String) -> Self {
        RustyProviderModel { port, model }
    }
}

impl ModelProvider for RustyProviderModel {
    fn respond<'a>(&'a mut self, prompt: &'a str) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let body = serde_json::json!({
                "model": self.model,
                "messages": [{"role": "user", "content": prompt}],
            })
            .to_string();
            let (status, body) =
                http_client::post_json(self.port, "/v1/chat/completions", &body).await?;
            if status != 200 {
                return Err(HarnessError::conflict(
                    Context::Provider,
                    format!("rp-server returned {status}: {body}"),
                ));
            }
            let value: serde_json::Value = serde_json::from_str(&body)
                .map_err(|e| HarnessError::json(Context::Provider, None, e))?;
            value["choices"][0]["message"]["content"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| {
                    HarnessError::protocol(
                        Context::Provider,
                        format!("unexpected rp-server response shape: {body}"),
                    )
                })
        })
    }
}
