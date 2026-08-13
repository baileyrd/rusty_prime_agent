//! Model provider stub (Phase 1 non-goal: "Model provider clients --
//! stub with a fake provider that echoes turns"). Real provider clients
//! (streaming, retries) are Phase 2's job; this exists only so
//! `AgentSession` has something to call that proves the prompt ->
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
//!
//! Real tool-calling (`PARITY.md`'s "real tool-calling loop" entry): the
//! types below (`ChatTurn`/`TurnRole`/`ToolDef`/`ProviderReply`) are
//! hand-rolled to match `rp-server`'s own OpenAI-shaped wire types for
//! exactly the fields this project uses -- not a dependency on
//! `rp_core`, keeping the "talk to rp-server over HTTP only, never link
//! it in" boundary `rp_server.rs`'s own doc comment already establishes.
//! This is a separate capability from `tool_runtime::ToolRuntime`, which
//! stays exactly what its own doc comments say: the IPython-kernel
//! boundary, still `NoopToolRuntime` until a real kernel backend exists.

use crate::error::{Context, HarnessError, Result};
use crate::http_client;
use crate::protocol::{ToolCallRequest, Usage};
use crate::tool_runtime::BoxFuture;

/// One turn of the conversation sent to a provider, independent of how
/// it's persisted (`protocol::TranscriptEntry`/`Role`) -- `session::
/// AgentSession::build_turns` maps one to the other on every call, since
/// a provider needs the full conversation so far, not just the latest
/// prompt, once tool calls are in the picture.
#[derive(Debug, Clone)]
pub struct ChatTurn {
    pub role: TurnRole,
    /// `None` for an assistant turn that's purely a tool-call request
    /// (no user-visible text alongside it) -- mirrors `rp-server`'s own
    /// `content: null` convention for that case.
    pub content: Option<String>,
    /// Set only on an assistant turn that's a tool-call request.
    pub tool_calls: Option<Vec<ToolCallRequest>>,
    /// Set only on a `TurnRole::Tool` turn: which `ToolCallRequest::id`
    /// this is the result of.
    pub tool_call_id: Option<String>,
    /// Set only on a `TurnRole::Tool` turn: the tool's name.
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRole {
    System,
    User,
    Assistant,
    Tool,
}

/// One tool a provider may call, in `rp-server`'s own `tools`/
/// `tool_choice` shape (`ChatRequest.tools: Option<Vec<Tool>>`,
/// OpenAI's `{"type": "function", "function": {...}}` convention).
/// `parameters` is a raw JSON Schema object, passed straight through --
/// this project doesn't validate or interpret it itself, only the model
/// (and the tool's own implementation) does.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// What one `respond` call produced: either a normal reply, or a
/// request to run one or more tools before the model can continue --
/// `session::AgentSession::prompt`'s loop branches on this.
#[derive(Debug, Clone)]
pub enum ProviderReply {
    Text(String),
    ToolCalls(Vec<ToolCallRequest>),
}

/// One `ModelProvider::respond` call's full result: the reply itself,
/// plus that same call's own real token accounting when the backend has
/// one to report. `usage` covers the whole call regardless of which
/// `ProviderReply` variant came back -- an OpenAI-shaped `usage` object
/// accounts for the request/response pair, not specifically a text vs.
/// tool-calls shape.
#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub reply: ProviderReply,
    pub usage: Option<Usage>,
}

/// `Send + Sync`, like `ToolRuntime`: `AgentSession` holds this behind a
/// `rusty_tokio::sync::Mutex` shared across every connection handler
/// task the worker spawns, so the compiler needs `&AgentSession`
/// (reachable through the mutex guard) to itself be `Send` across an
/// `.await` -- which requires `AgentSession`, and therefore this field,
/// to be `Sync`.
pub trait ModelProvider: Send + Sync {
    fn respond<'a>(
        &'a mut self,
        turns: &'a [ChatTurn],
        tools: &'a [ToolDef],
    ) -> BoxFuture<'a, Result<ProviderResponse>>;
}

/// Echoes the latest user turn back verbatim, prefixed so a transcript
/// reader can tell an echoed reply from a real model response at a
/// glance. Ignores `tools` entirely (never emits `ProviderReply::
/// ToolCalls`) -- proves the tool-calling plumbing doesn't regress the
/// default, tool-less path rather than exercising it. Never reports
/// `usage` -- there's no real model call to account for.
#[derive(Debug, Default)]
pub struct EchoProvider;

impl ModelProvider for EchoProvider {
    fn respond<'a>(
        &'a mut self,
        turns: &'a [ChatTurn],
        _tools: &'a [ToolDef],
    ) -> BoxFuture<'a, Result<ProviderResponse>> {
        let last_user_text = turns
            .iter()
            .rev()
            .find(|t| t.role == TurnRole::User)
            .and_then(|t| t.content.clone())
            .unwrap_or_default();
        Box::pin(async move {
            Ok(ProviderResponse {
                reply: ProviderReply::Text(format!("echo: {last_user_text}")),
                usage: None,
            })
        })
    }
}

fn turn_role_str(role: TurnRole) -> &'static str {
    match role {
        TurnRole::System => "system",
        TurnRole::User => "user",
        TurnRole::Assistant => "assistant",
        TurnRole::Tool => "tool",
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
    /// `ReasoningConfig.effort` on `rp-server`'s wire types: `"low"`/
    /// `"medium"`/`"high"`, or `None` for no reasoning requested. See
    /// `Request::SessionNew::thinking`'s own doc comment for the
    /// end-to-end thread-through this comes from.
    thinking: Option<String>,
}

impl RustyProviderModel {
    pub fn new(port: u16, model: String, thinking: Option<String>) -> Self {
        RustyProviderModel {
            port,
            model,
            thinking,
        }
    }

    /// Builds the JSON body for `POST /v1/chat/completions`, separated
    /// out from `respond` so it's unit-testable without a real network
    /// call -- turns map to `messages`, `tools` to `ChatRequest.tools`
    /// (omitted entirely when empty, matching `rp-server`'s own
    /// `Option<Vec<Tool>>`), and `reasoning.effort` appears iff
    /// `self.thinking` is set.
    fn build_request_body(&self, turns: &[ChatTurn], tools: &[ToolDef]) -> String {
        let messages: Vec<serde_json::Value> = turns
            .iter()
            .map(|t| {
                let mut msg = serde_json::json!({ "role": turn_role_str(t.role) });
                if let Some(content) = &t.content {
                    msg["content"] = serde_json::Value::String(content.clone());
                }
                if let Some(tool_calls) = &t.tool_calls {
                    msg["tool_calls"] = serde_json::Value::Array(
                        tool_calls
                            .iter()
                            .map(|c| {
                                serde_json::json!({
                                    "id": c.id,
                                    "type": "function",
                                    "function": { "name": c.name, "arguments": c.arguments },
                                })
                            })
                            .collect(),
                    );
                }
                if let Some(tool_call_id) = &t.tool_call_id {
                    msg["tool_call_id"] = serde_json::Value::String(tool_call_id.clone());
                }
                if let Some(name) = &t.name {
                    msg["name"] = serde_json::Value::String(name.clone());
                }
                msg
            })
            .collect();

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
        });
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(
                tools
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": t.parameters,
                            },
                        })
                    })
                    .collect(),
            );
        }
        if let Some(thinking) = &self.thinking {
            body["reasoning"] = serde_json::json!({"effort": thinking});
        }
        body.to_string()
    }
}

/// Parses `rp-server`'s `/v1/chat/completions` response body into a
/// [`ProviderResponse`] -- separated from `respond` for the same
/// unit-testability reason as `build_request_body`. A `tool_calls` array
/// on the message (non-empty) wins over `content`, matching OpenAI's own
/// convention that a tool-calling turn's `content` is typically absent.
/// `usage` is a top-level sibling of `choices` in the same body
/// (`rp-server`'s own `core::types::ChatResponse.usage`, an OpenAI-shaped
/// `{prompt_tokens, completion_tokens, total_tokens, ...}` object) --
/// `None` when the key is absent entirely; a present-but-malformed
/// sub-field defaults to `0` rather than failing the whole parse, the
/// same "don't fail an otherwise-successful reply over a telemetry
/// nicety" leniency this parser already has for individual `tool_calls`
/// fields above.
fn parse_response(body: &str) -> Result<ProviderResponse> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| HarnessError::json(Context::Provider, None, e))?;
    let usage = value.get("usage").map(|u| Usage {
        prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0) as u32,
        completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0) as u32,
        total_tokens: u["total_tokens"].as_u64().unwrap_or(0) as u32,
    });
    let message = &value["choices"][0]["message"];
    if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
        if !tool_calls.is_empty() {
            let calls = tool_calls
                .iter()
                .map(|c| ToolCallRequest {
                    id: c["id"].as_str().unwrap_or_default().to_string(),
                    name: c["function"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    arguments: c["function"]["arguments"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect();
            return Ok(ProviderResponse {
                reply: ProviderReply::ToolCalls(calls),
                usage,
            });
        }
    }
    message["content"]
        .as_str()
        .map(|s| ProviderResponse {
            reply: ProviderReply::Text(s.to_string()),
            usage,
        })
        .ok_or_else(|| {
            HarnessError::protocol(
                Context::Provider,
                format!("unexpected rp-server response shape: {body}"),
            )
        })
}

impl ModelProvider for RustyProviderModel {
    fn respond<'a>(
        &'a mut self,
        turns: &'a [ChatTurn],
        tools: &'a [ToolDef],
    ) -> BoxFuture<'a, Result<ProviderResponse>> {
        Box::pin(async move {
            let body = self.build_request_body(turns, tools);
            let (status, body) =
                http_client::post_json(self.port, "/v1/chat/completions", &body).await?;
            if status != 200 {
                return Err(HarnessError::conflict(
                    Context::Provider,
                    format!("rp-server returned {status}: {body}"),
                ));
            }
            parse_response(&body)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_turn(text: &str) -> ChatTurn {
        ChatTurn {
            role: TurnRole::User,
            content: Some(text.to_string()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn build_request_body_omits_reasoning_when_thinking_is_none() {
        let provider = RustyProviderModel::new(1234, "ollama/qwen2.5:0.5b".to_string(), None);
        let body: serde_json::Value =
            serde_json::from_str(&provider.build_request_body(&[user_turn("hello")], &[])).unwrap();
        assert_eq!(body["model"], "ollama/qwen2.5:0.5b");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hello");
        assert!(body.get("reasoning").is_none());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn build_request_body_includes_reasoning_effort_when_thinking_is_set() {
        let provider = RustyProviderModel::new(
            1234,
            "ollama/qwen2.5:0.5b".to_string(),
            Some("high".to_string()),
        );
        let body: serde_json::Value =
            serde_json::from_str(&provider.build_request_body(&[user_turn("hello")], &[])).unwrap();
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn build_request_body_includes_tools_when_offered() {
        let provider = RustyProviderModel::new(1234, "ollama/qwen2.5:0.5b".to_string(), None);
        let tools = [ToolDef {
            name: "read_file".to_string(),
            description: "reads a file".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let body: serde_json::Value =
            serde_json::from_str(&provider.build_request_body(&[user_turn("hello")], &tools))
                .unwrap();
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
    }

    #[test]
    fn build_request_body_serializes_a_tool_result_turn() {
        let provider = RustyProviderModel::new(1234, "ollama/qwen2.5:0.5b".to_string(), None);
        let turns = [
            user_turn("what's in a.txt?"),
            ChatTurn {
                role: TurnRole::Assistant,
                content: None,
                tool_calls: Some(vec![ToolCallRequest {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: r#"{"path":"a.txt"}"#.to_string(),
                }]),
                tool_call_id: None,
                name: None,
            },
            ChatTurn {
                role: TurnRole::Tool,
                content: Some("contents of a.txt".to_string()),
                tool_calls: None,
                tool_call_id: Some("call_1".to_string()),
                name: Some("read_file".to_string()),
            },
        ];
        let body: serde_json::Value =
            serde_json::from_str(&provider.build_request_body(&turns, &[])).unwrap();
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert!(body["messages"][1].get("content").is_none());
        assert_eq!(
            body["messages"][1]["tool_calls"][0]["function"]["name"],
            "read_file"
        );
        assert_eq!(body["messages"][2]["role"], "tool");
        assert_eq!(body["messages"][2]["tool_call_id"], "call_1");
        assert_eq!(body["messages"][2]["name"], "read_file");
    }

    #[test]
    fn parse_response_extracts_plain_text_content() {
        let body = serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "hi there"}}]
        })
        .to_string();
        let response = parse_response(&body).unwrap();
        match response.reply {
            ProviderReply::Text(text) => assert_eq!(text, "hi there"),
            other => panic!("expected Text, got {other:?}"),
        }
        assert!(response.usage.is_none(), "got: {:?}", response.usage);
    }

    #[test]
    fn parse_response_extracts_tool_calls_over_content() {
        let body = serde_json::json!({
            "choices": [{"message": {
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"a.txt\"}"},
                }],
            }}]
        })
        .to_string();
        match parse_response(&body).unwrap().reply {
            ProviderReply::ToolCalls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "call_1");
                assert_eq!(calls[0].name, "read_file");
                assert_eq!(calls[0].arguments, r#"{"path":"a.txt"}"#);
            }
            other => panic!("expected ToolCalls, got {other:?}"),
        }
    }

    #[test]
    fn parse_response_extracts_usage_when_present() {
        let body = serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "hi there"}}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 34, "total_tokens": 46},
        })
        .to_string();
        let usage = parse_response(&body).unwrap().usage.expect("usage present");
        assert_eq!(usage.prompt_tokens, 12);
        assert_eq!(usage.completion_tokens, 34);
        assert_eq!(usage.total_tokens, 46);
    }
}
