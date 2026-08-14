//! `harness acp [--model PROVIDER/MODEL]` -- a bounded, spec-verified
//! first slice of the Agent Client Protocol (`agentclientprotocol.com`),
//! parity with `prime-agent`'s `--mode acp`.
//!
//! `PARITY.md`'s own ACP entry held off implementing this precisely
//! because it hadn't yet done a direct probe of ACP's real wire shapes
//! the way MCP integration and the ZMTP client both required before any
//! code was written for either -- "risks producing something that
//! *claims* ACP compliance without actually having it." This module is
//! that probe, done properly: every shape below is checked against the
//! canonical machine-readable schema
//! (`https://raw.githubusercontent.com/agentclientprotocol/agent-client-protocol/main/schema/v2/schema.json`),
//! not recalled from memory or a summary.
//!
//! Scope, chosen the same way every other "bounded first slice" in this
//! project is: implement the baseline agent method surface the ACP spec
//! itself names as a coherent unit -- `AgentCapabilities.session = {}`
//! (an empty object, not omitted) is spec-defined to mean exactly
//! "`session/new`, `session/prompt`, `session/cancel`, and
//! `session/update`" (see `InitializeResponse`'s own schema
//! description), so that's what this module reports and that's what it
//! implements, plus `session/close` (required to free resources
//! cleanly). Deliberately cut, all matching real, already-established
//! gaps elsewhere in this project rather than new ones:
//!
//! - `auth/login`/`auth/logout` -- no OAuth backend exists anywhere in
//!   this project (see `/login`'s own "interactive provider-setup
//!   wizard, no real OAuth backend" entry); `authMethods: []` in the
//!   `initialize` response is the spec-legal way to say so, since a
//!   non-empty list would obligate supporting both methods.
//! - `session/resume`, `session/list` -- this project's own
//!   `resolve_session_id`/`session list` already cover the underlying
//!   need outside ACP; wiring them into the ACP surface too is a
//!   natural v2 extension, not attempted here.
//! - `session/request_permission`, `elicitation/*` -- agent-initiated
//!   requests *to* the client. This project's tool-calling loop already
//!   auto-executes every tool call with no confirmation step (the same
//!   "no sandboxing, single local user" trust model `tools.rs`'s own doc
//!   comment states), so there is nothing today that would ever need to
//!   ask permission.
//! - `tool_call`/`tool_call_update` session/update kinds -- a real
//!   `execute_python`/`--tools` round trip happens entirely inside one
//!   `AgentSession::prompt` call before this module ever sees a result,
//!   so there is no natural point to emit an in-progress tool-call
//!   update from; only the final `agent_message_chunk` is emitted. A
//!   genuine simplification (prime-agent's own IPython-cell-as-tool_call
//!   mapping this project's `PARITY.md` once predicted would apply), not
//!   an oversight.
//! - No real streaming -- `ProviderReply` is a complete reply or
//!   complete tool-call batch, never a partial delta (see `json.md`'s
//!   own claims-audit finding), so exactly one `agent_message_chunk`
//!   carries the whole reply per turn, immediately followed by one
//!   `state_update` (`idle`, `stopReason`) -- the "one legal
//!   `session/update` chunk per turn" shape `PARITY.md` predicted.
//!
//! Session identity: an ACP `sessionId` *is* this project's own session
//! id string, no separate mapping table -- the two concepts already
//! coincide exactly (both are server-minted opaque strings scoped to one
//! conversation), so introducing a second id space would only add a
//! lookup with nothing to gain from it.
//!
//! Concurrency, a real gap found and fixed rather than inherited:
//! `session_repl`'s own steering gap (`PARITY.md`'s "Interactive TUI:
//! steering vs. follow-up message queue" entry) exists because that
//! loop reads and dispatches one stdin line at a time, so a line typed
//! while a prompt is in flight can't be acted on until the prompt
//! finishes. `session/cancel` is a genuinely time-sensitive notification
//! (a client mid-turn, mid-`session/prompt` needs it to interrupt that
//! *same* turn, not the next one) -- this module spawns each incoming
//! message as its own task instead of processing the stdin loop
//! sequentially, so a `session/cancel` line sitting behind an in-flight
//! `session/prompt` line is read and dispatched immediately rather than
//! queued behind it. `stdout_lock` (the same `Arc<Mutex<()>>` pattern
//! `client::session_rpc` already uses for its own two-lane stdout) keeps
//! concurrent responses/notifications from interleaving mid-line.
//!
//! Handshake ordering is enforced, not just assumed: every method other
//! than `initialize` itself (and `session/cancel`, a notification with
//! no response to withhold) is rejected with `-32600` until
//! `initialize` has actually been handled on this connection.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use rusty_tokio::sync::Mutex;
use serde_json::{json, Value};

use crate::client::dispatch_one_shot;
use crate::error::Result;
use crate::protocol::{Request, Response};

/// The only ACP protocol version this module understands, matching this
/// schema pass's own `v2/schema.json`. Always reported back verbatim in
/// `initialize`'s response regardless of what the client asked for --
/// claiming support for a version whose shapes were never checked would
/// be exactly the "claims compliance without having it" risk this
/// module's own doc comment describes.
const PROTOCOL_VERSION: u64 = 2;

pub async fn run(state_root: &Path, default_model: Option<String>) -> Result<()> {
    let stdout_lock = Arc::new(Mutex::new(()));
    let sessions: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    // Spec: the client always sends `initialize` first, before any
    // session method. Enforced here rather than trusted, so a client
    // bug (or a test exercising the wire protocol directly) gets a
    // clear `-32600` instead of a confusing downstream failure.
    let initialized = Arc::new(Mutex::new(false));
    let mut handles = Vec::new();

    loop {
        let line = rusty_tokio::spawn_blocking(|| {
            let mut buf = String::new();
            match std::io::stdin().read_line(&mut buf) {
                Ok(0) | Err(_) => None,
                Ok(_) => Some(buf),
            }
        })
        .await
        .unwrap_or(None);
        let Some(line) = line else { break };
        let text = line.trim().to_string();
        if text.is_empty() {
            continue;
        }

        let state_root = state_root.to_path_buf();
        let default_model = default_model.clone();
        let stdout_lock = stdout_lock.clone();
        let sessions = sessions.clone();
        let initialized = initialized.clone();
        handles.push(rusty_tokio::spawn(async move {
            handle_line(
                &state_root,
                &text,
                default_model,
                &sessions,
                &initialized,
                &stdout_lock,
            )
            .await;
        }));
    }

    // Every spawned handler is awaited before returning, unlike
    // `session_rpc`'s own fixed grace-window sleep for the same
    // stdin-EOF-races-background-work hazard -- there, the events lane
    // runs for the whole connection's lifetime with no natural end to
    // join on; here, every task is one already-received message with a
    // real completion to wait for, so there is nothing a fixed sleep
    // would do better than actually waiting.
    for handle in handles {
        let _ = handle.await;
    }
    Ok(())
}

async fn handle_line(
    state_root: &Path,
    text: &str,
    default_model: Option<String>,
    sessions: &Arc<Mutex<HashSet<String>>>,
    initialized: &Arc<Mutex<bool>>,
    stdout_lock: &Arc<Mutex<()>>,
) {
    let value: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            write_error(
                stdout_lock,
                Value::Null,
                -32700,
                &format!("parse error: {e}"),
            )
            .await;
            return;
        }
    };
    let id = value.get("id").cloned();
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_string);
    let params = value.get("params").cloned().unwrap_or(Value::Null);

    let Some(method) = method else {
        if let Some(id) = id {
            write_error(stdout_lock, id, -32600, "invalid request: missing `method`").await;
        }
        return;
    };

    // Spec: the client always sends `initialize` first. `session/cancel`
    // is exempt -- it's a notification with no response to withhold,
    // and if nothing was ever initialized, `sessions` is empty anyway,
    // so `handle_cancel` is already a guaranteed no-op in that case.
    if method != "initialize" && method != "session/cancel" && !*initialized.lock().await {
        if let Some(id) = id {
            write_error(
                stdout_lock,
                id,
                -32600,
                "invalid request: `initialize` must be called first",
            )
            .await;
        }
        return;
    }

    match (method.as_str(), id) {
        ("initialize", Some(id)) => {
            *initialized.lock().await = true;
            handle_initialize(stdout_lock, id, &params).await;
        }
        ("session/new", Some(id)) => {
            handle_session_new(
                state_root,
                stdout_lock,
                id,
                &params,
                default_model,
                sessions,
            )
            .await
        }
        ("session/prompt", Some(id)) => {
            handle_prompt(state_root, stdout_lock, id, &params, sessions).await
        }
        ("session/close", Some(id)) => {
            handle_close(state_root, stdout_lock, id, &params, sessions).await
        }
        ("session/cancel", None) => handle_cancel(state_root, &params, sessions).await,
        (other, Some(id)) => {
            write_error(
                stdout_lock,
                id,
                -32601,
                &format!("method not found: `{other}`"),
            )
            .await;
        }
        // An unrecognized *notification* has no response to send at
        // all -- silently ignored, the same tolerance the spec's own
        // `ExtNotification`/`_`-prefixed-extension conventions expect
        // of an implementation that doesn't recognize something.
        (_, None) => {}
    }
}

async fn handle_initialize(stdout_lock: &Arc<Mutex<()>>, id: Value, _params: &Value) {
    let result = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "info": {
            "name": "rusty_prime_agent",
            "version": env!("CARGO_PKG_VERSION"),
        },
        // `{}` is spec-defined to mean exactly the baseline surface this
        // module implements -- see this module's own doc comment.
        "capabilities": { "session": {} },
        // No OAuth backend exists anywhere in this project -- see this
        // module's own doc comment.
        "authMethods": [],
    });
    write_result(stdout_lock, id, result).await;
}

async fn handle_session_new(
    state_root: &Path,
    stdout_lock: &Arc<Mutex<()>>,
    id: Value,
    params: &Value,
    default_model: Option<String>,
    sessions: &Arc<Mutex<HashSet<String>>>,
) {
    // `cwd` is a required field per spec, but this project has no
    // per-session working-directory concept to seed it into (see
    // `CLAIMS_AUDIT.md`'s own "no `--cwd`" finding) -- validated for
    // presence, matching spec compliance, then otherwise unused.
    if params.get("cwd").and_then(Value::as_str).is_none() {
        write_error(stdout_lock, id, -32602, "invalid params: `cwd` is required").await;
        return;
    }

    let request = Request::SessionNew {
        name: None,
        model: default_model,
        goal: None,
        parent_id: None,
        spawned_from_sequence: None,
        thinking: None,
        tools: None,
        runtime: None,
    };
    match dispatch_one_shot(state_root, request).await {
        Ok(Response::SessionNew { session_id }) => {
            sessions.lock().await.insert(session_id.clone());
            write_result(stdout_lock, id, json!({ "sessionId": session_id })).await;
        }
        Ok(Response::Error { message, .. }) => {
            write_error(stdout_lock, id, -32603, &message).await;
        }
        Ok(other) => {
            write_error(
                stdout_lock,
                id,
                -32603,
                &format!("unexpected daemon response to session/new: {other:?}"),
            )
            .await;
        }
        Err(e) => write_error(stdout_lock, id, -32603, &e.to_string()).await,
    }
}

async fn handle_prompt(
    state_root: &Path,
    stdout_lock: &Arc<Mutex<()>>,
    id: Value,
    params: &Value,
    sessions: &Arc<Mutex<HashSet<String>>>,
) {
    let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
        write_error(
            stdout_lock,
            id,
            -32602,
            "invalid params: `sessionId` is required",
        )
        .await;
        return;
    };
    if !sessions.lock().await.contains(session_id) {
        write_error(
            stdout_lock,
            id,
            -32602,
            &format!("unknown session id `{session_id}` (not created via this connection)"),
        )
        .await;
        return;
    }
    let blocks = params
        .get("prompt")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let text = flatten_prompt_content(&blocks);

    let request = Request::SessionPrompt {
        session_id: session_id.to_string(),
        text,
        images: None,
        request_id: None,
    };
    let entry = match dispatch_one_shot(state_root, request).await {
        Ok(Response::SessionPromptAck { entry }) => entry,
        Ok(Response::Error { message, .. }) => {
            write_error(stdout_lock, id, -32603, &message).await;
            return;
        }
        Ok(other) => {
            write_error(
                stdout_lock,
                id,
                -32603,
                &format!("unexpected daemon response to session/prompt: {other:?}"),
            )
            .await;
            return;
        }
        Err(e) => {
            write_error(stdout_lock, id, -32603, &e.to_string()).await;
            return;
        }
    };

    // One non-streaming `ProviderReply` -> one legal `session/update`
    // chunk, immediately followed by the `idle`/`end_turn` transition --
    // see this module's own doc comment for why there's no earlier
    // "accepted" moment to report separately from the finished turn.
    let message_id = entry.sequence.to_string();
    write_notification(
        stdout_lock,
        "session/update",
        json!({
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": message_id,
                "content": { "type": "text", "text": entry.text },
            },
        }),
    )
    .await;
    write_notification(
        stdout_lock,
        "session/update",
        json!({
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "state_update",
                "state": "idle",
                "stopReason": "end_turn",
            },
        }),
    )
    .await;

    write_result(stdout_lock, id, json!({})).await;
}

async fn handle_close(
    state_root: &Path,
    stdout_lock: &Arc<Mutex<()>>,
    id: Value,
    params: &Value,
    sessions: &Arc<Mutex<HashSet<String>>>,
) {
    let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
        write_error(
            stdout_lock,
            id,
            -32602,
            "invalid params: `sessionId` is required",
        )
        .await;
        return;
    };
    // Spec: "the agent MUST cancel any ongoing work... and then free up
    // any resources associated with the session" -- `Request::
    // SessionStop` is this project's own "free the resources" primitive
    // (stops the worker), a stronger action than this project's usual
    // "closing the TUI detaches the client; it does not stop the worker"
    // default (see `ARCHITECTURE.md`'s daemon section), deliberately
    // chosen here because ACP's own spec says so explicitly, not
    // because this module is guessing.
    let _ = dispatch_one_shot(
        state_root,
        Request::SessionStop {
            session_id: session_id.to_string(),
        },
    )
    .await;
    sessions.lock().await.remove(session_id);
    write_result(stdout_lock, id, json!({})).await;
}

async fn handle_cancel(state_root: &Path, params: &Value, sessions: &Arc<Mutex<HashSet<String>>>) {
    // A notification -- no response of any kind, success or error, is
    // ever sent for this method.
    let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
        return;
    };
    if !sessions.lock().await.contains(session_id) {
        return;
    }
    let _ = dispatch_one_shot(
        state_root,
        Request::SessionInterrupt {
            session_id: session_id.to_string(),
        },
    )
    .await;
}

/// Concatenates every `type: "text"` block's own text, in order,
/// separated by blank lines. Any other `ContentBlock` variant (image,
/// audio, resource_link, resource) is represented by an honest inline
/// placeholder rather than silently dropped -- this project's own
/// established "loud, not silent" convention for a genuinely unsupported
/// input (see `tools::read_file`'s own "reports a missing file as an
/// error string, not a panic" precedent) applied to a gap instead of a
/// failure. Only `ContentBlock::Text` is a baseline-required content
/// type for prompts per spec; every other variant needs a
/// `PromptCapabilities` flag this module never advertises in the first
/// place (`initialize`'s `capabilities.session` reports no such flags),
/// so a compliant client shouldn't send them here at all -- this is a
/// defensive fallback, not the expected path.
fn flatten_prompt_content(blocks: &[Value]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    parts.push(text.to_string());
                }
            }
            Some(other) => parts.push(format!("[unsupported content block: {other}]")),
            None => {}
        }
    }
    parts.join("\n\n")
}

async fn write_result(stdout_lock: &Arc<Mutex<()>>, id: Value, result: Value) {
    write_line(
        stdout_lock,
        json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    )
    .await;
}

async fn write_error(stdout_lock: &Arc<Mutex<()>>, id: Value, code: i64, message: &str) {
    write_line(
        stdout_lock,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }),
    )
    .await;
}

async fn write_notification(stdout_lock: &Arc<Mutex<()>>, method: &str, params: Value) {
    write_line(
        stdout_lock,
        json!({ "jsonrpc": "2.0", "method": method, "params": params }),
    )
    .await;
}

async fn write_line(stdout_lock: &Arc<Mutex<()>>, value: Value) {
    let _guard = stdout_lock.lock().await;
    println!("{}", serde_json::to_string(&value).unwrap_or_default());
}
