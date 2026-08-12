//! Minimal MCP (Model Context Protocol) client, talking only to
//! `rp-server`'s own built-in `/mcp` gateway (see `rp_server`'s own doc
//! comment for the sidecar boundary this crosses) -- not a
//! general-purpose MCP client. `rp-server`'s gateway already merges its
//! own native tools (`chat_completion`/`list_models`/`embeddings`) with
//! every tool proxied from a configured `[[mcp.upstreams]]` entry
//! (namespaced `"{upstream}/{tool}"`), so this project only ever needs
//! to speak to *one* MCP server, not implement a multi-upstream gateway
//! itself.
//!
//! Hand-rolled to match the wire behavior `rp-server` (built on
//! `rusty_mcp`, `LocalSessionManager`) actually exhibits -- confirmed by
//! directly probing a real sidecar before writing this (see `PARITY.md`'s
//! MCP entry for the full story), the same "reproduce first" discipline
//! this project used for its original AF_UNIX bug:
//!
//! - Every response is SSE-framed (`Content-Type: text/event-stream`,
//!   `Transfer-Encoding: chunked`) even for a single, non-streaming
//!   request -- `rp-server` rejects a request whose `Accept` header
//!   doesn't include *both* `application/json` and `text/event-stream`
//!   with `406 Not Acceptable`. `http_client::decode_chunked` handles
//!   the chunked framing; this module handles the SSE framing on top of
//!   that (splitting the decoded body into events, taking the first
//!   event with a non-empty `data:` payload).
//! - The server closes the connection after answering a single request
//!   (confirmed empirically: a bounded-timeout probe completed in
//!   milliseconds) -- no persistent stream to keep open, so this
//!   project's existing "one request per connection" `http_client`
//!   design already fits without changes beyond header/chunked support.
//! - `initialize`'s response carries an `Mcp-Session-Id` header that
//!   every subsequent request on this "session" must also send --
//!   omitting it makes `rp-server` answer `422 Unprocessable Entity`
//!   ("Unexpected message, expect initialize request"), even though each
//!   individual HTTP request is otherwise a fresh, unrelated TCP
//!   connection (the session is server-side state keyed by this header,
//!   not by the connection itself).
//! - A `tools/call` error can come back either as a JSON-RPC `error`
//!   object (e.g. an unknown tool name) or as a successful `result`
//!   with `isError: true` (a tool that ran but failed) -- both are
//!   surfaced as the tool's own result text here, visible-to-model data
//!   rather than a hard `HarnessError`, the same "let the model see and
//!   recover from it" reasoning `tools::execute`'s built-in tools
//!   already use for a bad path/unknown tool name.
//!
//! Implements only `initialize`/`notifications/initialized` + `tools/
//! list` + `tools/call` -- the minimal surface this project's tool
//! registry needs. Skips JSON-Schema validation of tool call arguments
//! entirely (passes the model's raw arguments straight through, lets
//! `rp-server`/the upstream reject malformed ones), matching this
//! project's own built-in tools' handling of theirs.

use crate::error::{Context, HarnessError, Result};
use crate::http_client;

/// The MCP protocol revision this client speaks -- `rp-server`'s
/// `LocalSessionManager` serves both this and the newer 2026-07-28
/// stateless `discover` bootstrap, but the older `initialize` handshake
/// (what this client uses) is still what most MCP clients in the wild
/// speak, per `rusty_provider/docs/MCP.md`.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// `Accept` header every request to `/mcp` must send -- `rp-server`
/// answers `406 Not Acceptable` otherwise (confirmed by direct probe,
/// see this module's own doc comment).
const MCP_ACCEPT: &str = "application/json, text/event-stream";

/// One tool this project's own tool registry can discover from
/// `rp-server`'s MCP gateway -- already namespaced `"{upstream}/{tool}"`
/// by `rp-server` itself for a proxied upstream tool, so this project
/// doesn't need to namespace it again.
#[derive(Debug, Clone)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// A connected MCP session against one `rp-server` sidecar. Cheap to
/// hold (just a port and a session id string) -- callers create one via
/// [`connect`] the first time a session needs MCP tools and keep it for
/// as long as they keep needing them; there's no persistent connection
/// underneath to keep alive (see this module's own doc comment for why
/// each call is still its own fresh TCP connection).
#[derive(Debug, Clone)]
pub struct McpClient {
    port: u16,
    session_id: String,
}

impl McpClient {
    /// Performs the `initialize`/`notifications/initialized` handshake
    /// against a running `rp-server` sidecar's `/mcp` endpoint, capturing
    /// the `Mcp-Session-Id` every subsequent request must carry. The
    /// `notifications/initialized` step is sent for spec compliance (a
    /// real upstream MCP server behind the gateway may expect it) even
    /// though `rp-server` itself doesn't require it before `tools/list`/
    /// `tools/call` (confirmed by direct probe).
    pub async fn connect(port: u16) -> Result<Self> {
        let request_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "rusty_prime_agent",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            },
        })
        .to_string();
        let (status, headers, body) = http_client::post_json_with_headers(
            port,
            "/mcp",
            &request_body,
            &[("Accept", MCP_ACCEPT)],
        )
        .await?;
        if status != 200 {
            return Err(HarnessError::conflict(
                Context::Provider,
                format!("rp-server MCP initialize returned {status}: {body}"),
            ));
        }
        let session_id = headers
            .iter()
            .find(|(name, _)| name == "mcp-session-id")
            .map(|(_, value)| value.clone())
            .ok_or_else(|| {
                HarnessError::protocol(
                    Context::Provider,
                    "rp-server MCP initialize response had no Mcp-Session-Id header",
                )
            })?;
        let _ = extract_response(&body)?;

        let client = McpClient { port, session_id };
        client.notify("notifications/initialized").await?;
        Ok(client)
    }

    /// Sends a JSON-RPC notification (no `id`, no response expected) --
    /// only `notifications/initialized` today.
    async fn notify(&self, method: &str) -> Result<()> {
        let body = serde_json::json!({ "jsonrpc": "2.0", "method": method }).to_string();
        self.post(&body).await?;
        Ok(())
    }

    /// Sends a JSON-RPC request with a fresh, arbitrary id (this client
    /// never has more than one request in flight at a time, so a fixed
    /// id is fine -- there's nothing to disambiguate against) and
    /// returns the parsed `result` value, or a `HarnessError` built from
    /// the JSON-RPC `error` object if the call itself failed at the
    /// protocol level (as opposed to the tool it named failing, which
    /// comes back as an ordinary `result` with `isError: true`).
    async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        })
        .to_string();
        let response_body = self.post(&body).await?;
        extract_response(&response_body)
    }

    async fn post(&self, body: &str) -> Result<String> {
        let (status, _headers, body) = http_client::post_json_with_headers(
            self.port,
            "/mcp",
            body,
            &[("Accept", MCP_ACCEPT), ("Mcp-Session-Id", &self.session_id)],
        )
        .await?;
        if status != 200 && status != 202 {
            return Err(HarnessError::conflict(
                Context::Provider,
                format!("rp-server MCP request returned {status}: {body}"),
            ));
        }
        Ok(body)
    }

    /// Lists every tool `rp-server`'s MCP gateway currently offers --
    /// its own native tools plus every connected upstream's, already
    /// namespaced.
    pub async fn list_tools(&self) -> Result<Vec<McpTool>> {
        let result = self.call("tools/list", serde_json::json!({})).await?;
        let tools = result["tools"].as_array().ok_or_else(|| {
            HarnessError::protocol(
                Context::Provider,
                format!("unexpected MCP tools/list response shape: {result}"),
            )
        })?;
        Ok(tools
            .iter()
            .map(|t| McpTool {
                name: t["name"].as_str().unwrap_or_default().to_string(),
                description: t["description"].as_str().unwrap_or_default().to_string(),
                input_schema: t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or(serde_json::json!({"type": "object"})),
            })
            .collect())
    }

    /// Calls one MCP tool by name (already namespaced, e.g.
    /// `"filesystem/read_file"`) with raw, model-generated JSON
    /// arguments, returning the text to send back as the `Role::Tool`
    /// transcript entry's own text. Concatenates every `content` block
    /// of type `"text"` (the only block type `rp-server`'s own tools and
    /// most upstreams emit) -- a block of another type is skipped rather
    /// than erroring, since this project has nowhere to put an image or
    /// other rich content in a plain-text transcript entry yet.
    pub async fn call_tool(&self, name: &str, arguments_json: &str) -> Result<String> {
        let arguments: serde_json::Value =
            serde_json::from_str(arguments_json).unwrap_or(serde_json::json!({}));
        let params = serde_json::json!({ "name": name, "arguments": arguments });
        let result = match self.call("tools/call", params).await {
            Ok(result) => result,
            // A protocol-level error (unknown tool, bad arguments the
            // gateway itself rejected) -- still just data for the model
            // to see, not a reason to fail this session's whole prompt.
            Err(err) => return Ok(format!("error: {err}")),
        };
        let content = result["content"].as_array().cloned().unwrap_or_default();
        let text: String = content
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if result["isError"].as_bool().unwrap_or(false) {
            return Ok(format!("error: {text}"));
        }
        Ok(text)
    }
}

/// Extracts the JSON-RPC `result` from one SSE-framed response body
/// (already chunked-decoded by `http_client`): splits on blank lines
/// into events, skips the leading empty keepalive event (`rp-server`
/// always sends one first, with an empty `data:` field, before the
/// event actually carrying the JSON-RPC response), and parses the first
/// event whose `data:` field is non-empty as a JSON-RPC response object,
/// returning `result` on success or a `HarnessError` built from `error`
/// on a protocol-level failure.
fn extract_response(sse_body: &str) -> Result<serde_json::Value> {
    for event in sse_body.split("\n\n") {
        let data: String = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&data)
            .map_err(|e| HarnessError::json(Context::Provider, None, e))?;
        if let Some(error) = value.get("error") {
            let message = error["message"].as_str().unwrap_or("unknown MCP error");
            return Err(HarnessError::protocol(
                Context::Provider,
                message.to_string(),
            ));
        }
        return value.get("result").cloned().ok_or_else(|| {
            HarnessError::protocol(
                Context::Provider,
                format!("MCP response had neither result nor error: {value}"),
            )
        });
    }
    Err(HarnessError::protocol(
        Context::Provider,
        "empty MCP SSE response body",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_response_skips_the_leading_keepalive_event() {
        let body = "data: \nid: 0\nretry: 3000\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\nid: 1\n\n";
        let result = extract_response(body).unwrap();
        assert_eq!(result, serde_json::json!({"ok": true}));
    }

    #[test]
    fn extract_response_surfaces_a_json_rpc_error_as_a_harness_error() {
        let body =
            "data: \nid: 0\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32602,\"message\":\"no MCP upstream named 'x' is connected\"}}\n\n";
        let err = extract_response(body).unwrap_err();
        assert!(err.to_string().contains("no MCP upstream named 'x'"));
    }

    #[test]
    fn extract_response_fails_on_a_truly_empty_body() {
        assert!(extract_response("").is_err());
    }
}
