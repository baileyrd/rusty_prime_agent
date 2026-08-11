//! Model provider stub (Phase 1 non-goal: "Model provider clients --
//! stub with a fake provider that echoes turns"). Real provider clients
//! (streaming, tool calls, retries) are Phase 2's job; this exists only
//! so `AgentSession` has something to call that proves the prompt ->
//! response -> transcript -> event-stream pipeline works end to end.

use crate::error::Result;
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
