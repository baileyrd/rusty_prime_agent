//! `AgentSession`: the worker-owned unit that persists a transcript,
//! drives the (currently fake) model provider, and fans new turns out to
//! every attached client.
//!
//! ## Session State File Format (brief-flagged, load-bearing decision)
//!
//! Full JSONL replay, not periodic snapshot + tail: `transcript.jsonl` is
//! the single source of truth, and `state.json` is a small, cheaply
//! rewritten *pointer* file (session id, status, worker pid, generation,
//! last sequence, timestamps) -- exactly the "transcript + a small state
//! file" split the brief's own Required Behavior section already names.
//! Recovery replays the whole transcript into memory rather than a
//! snapshot-plus-tail scheme, because:
//!
//! - Phase 1 has no compaction (non-goal) -- there is no point in this
//!   project's life yet where a transcript is large enough for full
//!   replay to be the wrong trade.
//! - A periodic snapshot is a second persisted representation of the
//!   same data that can drift from the JSONL log; full replay has
//!   exactly one source of truth to get right, and rustils' own
//!   Linux-untested-until-now caveat already means this project's disk
//!   I/O deserves the simplest correct design, not a second moving part.
//! - `state.json` still gives O(1) recovery of the pointers a crash
//!   check needs (`worker_pid`, `status`) without scanning the
//!   transcript at all -- see `catalog::scan`.
//!
//! Revisit if/when compaction (Phase 2+) makes full replay the wrong
//! trade; the file boundary here (`transcript.jsonl` = source of truth,
//! `state.json` = pointer cache) is designed to survive that change --
//! only `state.json` would gain a snapshot cursor field.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusty_tokio::sync::broadcast;

use crate::error::{Context, HarnessError, Result};
use crate::paths::{self, now_ms};
use crate::protocol::{
    ChildUsageAttribution, CompactionState, ForkedFrom, GoalAction, GoalState, GoalStatus,
    HarnessAction, HarnessNote, HarnessSnapshot, HarnessState, Request, Role, ScheduleKind,
    SessionEvent, SessionState, SessionStatus, ToolCallRequest, TranscriptEntry, Usage,
};
use crate::provider::{
    ChatTurn, ModelProvider, ProviderReply, ProviderResponse, ToolDef, TurnRole,
};
use crate::tool_runtime::ToolRuntime;
use crate::transport;

/// Printed by the kernel's own `rlm_heartbeat()` (defined in every
/// `--runtime ipython` kernel by `worker::bootstrap_kernel`) -- parity
/// with `prime-agent`'s kernel-callable manual re-entry trigger.
/// `execute_python_tool_call` watches for this in a call's stdout,
/// strips it from what the model sees, and dispatches to
/// `trigger_heartbeat` when found. A plain ASCII marker, not something a
/// real print() is likely to emit by accident.
pub(crate) const HEARTBEAT_MARKER: &str = "___RPA_HEARTBEAT___";

/// Parity with `prime-agent`'s `AGENTS.md`/`CLAUDE.md` auto-loading:
/// global-tier only, same reason `skills::discover` is (see that
/// module's own doc comment) -- the worker process has no access to the
/// CLI caller's own cwd, so a project-local tier (walked up from cwd,
/// `prime-agent`'s own other half) isn't attempted here. Checks
/// `<state_dir>/AGENTS.md` first, then `<state_dir>/CLAUDE.md` -- the
/// first one found wins, they're not merged. An empty or all-whitespace
/// file is treated the same as a missing one (nothing to inject).
/// `SYSTEM.md`/`APPEND_SYSTEM.md` (`prime-agent`'s own system-prompt
/// override/append pair) stay unimplemented: this project has no base
/// system prompt at all to override or append to outside of this
/// context-file injection and the compaction summary, so there's
/// nothing for either to hook into without a larger design change.
fn read_context_file(state_root: &Path) -> Option<String> {
    for name in ["AGENTS.md", "CLAUDE.md"] {
        if let Ok(content) = std::fs::read_to_string(state_root.join(name)) {
            if !content.trim().is_empty() {
                return Some(content);
            }
        }
    }
    None
}

/// Finds `HEARTBEAT_MARKER` in `stdout` (if present) and returns
/// `(every_argument, stdout_with_the_marker_line_removed)`.
/// `worker::bootstrap_kernel`'s `rlm_heartbeat(every=None)` prints
/// `marker + (every or "")` on one line -- a plain `print()` is the only
/// channel from kernel code back to this process, so the optional
/// `every` duration string rides along on the same line rather than
/// needing a second signal. `every_argument` is empty for a plain
/// `rlm_heartbeat()` call (the one-shot form).
fn extract_heartbeat_marker(stdout: &str) -> Option<(String, String)> {
    let start = stdout.find(HEARTBEAT_MARKER)?;
    let after = &stdout[start + HEARTBEAT_MARKER.len()..];
    let line_len = after.find('\n').unwrap_or(after.len());
    let every = after[..line_len].trim().to_string();
    let mut without_marker = stdout.to_string();
    without_marker.replace_range(start..start + HEARTBEAT_MARKER.len() + line_len, "");
    Some((every, without_marker))
}

/// Conservative, deliberately approximate context-length trigger for
/// automatic compaction (`maybe_compact`, called every round of
/// `prompt`'s tool-calling loop) -- parity with `prime-agent`'s
/// `compaction.md`, whose own trigger compares estimated context tokens
/// against the model's context window minus a reserved buffer. This
/// project doesn't know any given model's real context window
/// (`rp-server`'s `/v1/chat/completions` response isn't parsed for
/// `usage` at all -- see `provider::parse_response`), so there's no
/// per-model budget to compare against; this is a single fixed default
/// instead, chosen low enough to trigger well before even a modest real
/// model's context window (commonly 8k to 128k tokens) is threatened,
/// high enough that ordinary short sessions never compact at all.
/// Overridable via `RUSTY_PRIME_AGENT_COMPACT_TRIGGER_TOKENS`, mainly so
/// this project's own tests can exercise real compaction against a real
/// model without needing thousands of tokens of real conversation
/// first.
const DEFAULT_COMPACT_TRIGGER_TOKENS: usize = 6_000;

/// How many of the most recent estimated tokens' worth of turns stay
/// verbatim, uncompacted, every time compaction fires -- parity with
/// `prime-agent`'s `keepRecentTokens`. Overridable via
/// `RUSTY_PRIME_AGENT_COMPACT_KEEP_RECENT_TOKENS`, same testability
/// reason as the trigger threshold above.
const DEFAULT_COMPACT_KEEP_RECENT_TOKENS: usize = 2_000;

fn compact_trigger_tokens(state_root: &Path) -> usize {
    std::env::var("RUSTY_PRIME_AGENT_COMPACT_TRIGGER_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| crate::settings::load(state_root).compact_trigger_tokens)
        .unwrap_or(DEFAULT_COMPACT_TRIGGER_TOKENS)
}

fn compact_keep_recent_tokens(state_root: &Path) -> usize {
    std::env::var("RUSTY_PRIME_AGENT_COMPACT_KEEP_RECENT_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| crate::settings::load(state_root).compact_keep_recent_tokens)
        .unwrap_or(DEFAULT_COMPACT_KEEP_RECENT_TOKENS)
}

/// A deliberately approximate token count: roughly 4 characters per
/// token, the same rough English-text heuristic used when no real
/// tokenizer is available. This project has no tokenizer dependency --
/// exact enough to decide "should compaction fire at all", not exact
/// enough to enforce a hard budget the way a real token-aware provider
/// client would.
fn estimate_tokens(text: &str) -> usize {
    text.len() / 4
}

/// Pure boundary-finding logic, separated from `AgentSession::
/// compact_now` so it's unit-testable without a live session/provider:
/// walks `candidates` backward (most recent first) accumulating
/// estimated tokens until `keep_recent_tokens` is exceeded, then returns
/// how many of the *oldest* entries (a prefix of `candidates`) should be
/// folded into a summary. Returns 0 when nothing is old enough to fold
/// away yet (including when `candidates` alone is already under the
/// keep-recent budget) -- parity with `prime-agent`'s "working backward
/// through messages until reaching the keepRecentTokens threshold".
fn find_compaction_fold_count(candidates: &[TranscriptEntry], keep_recent_tokens: usize) -> usize {
    let mut recent_tokens = 0usize;
    for (i, entry) in candidates.iter().enumerate().rev() {
        recent_tokens += estimate_tokens(&entry.text);
        if recent_tokens > keep_recent_tokens {
            return i + 1;
        }
    }
    0
}

/// Generates a session id unique enough for this project's needs: not
/// cryptographically random (no `rand` dependency for a display id no
/// security property leans on), just a nanosecond timestamp plus this
/// process's pid, which is exactly what a single supervisor issuing ids
/// one spawn at a time needs to never collide.
pub fn new_session_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("sess-{nanos:x}-{:x}", std::process::id())
}

/// Same shape/uniqueness reasoning as [`new_session_id`] -- a display
/// id, not a security-sensitive one.
fn new_harness_note_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("note-{nanos:x}")
}

/// Broadcast capacity: how many not-yet-delivered events a slow attached
/// client can fall behind by before its next `recv()` reports
/// `RecvError::Lagged` (handled in `worker::stream_events` by falling
/// back to closing that one client's stream -- see its doc comment).
/// Generous for Phase 1's control-plane traffic volume.
const EVENT_CHANNEL_CAPACITY: usize = 256;

pub struct AgentSession {
    pub state: SessionState,
    pub transcript: Vec<TranscriptEntry>,
    session_dir: PathBuf,
    /// Needed only to lazily connect an `mcp_client::McpClient` the
    /// first time a `--tools mcp` session actually prompts (not at
    /// construction time, so a session that's never prompted never pays
    /// for a handshake it might not need) -- every other subsystem here
    /// already gets what it needs from `session_dir` alone.
    state_root: PathBuf,
    provider: Box<dyn ModelProvider>,
    /// Lazily connected on first use by a `--tools mcp` session (see
    /// `mcp_client_or_connect`) and reused after that -- `None` for
    /// every other session, including one that's never actually
    /// prompted yet.
    mcp_client: Option<crate::mcp_client::McpClient>,
    /// Held and shut down (see `mark_stopped`) for every session
    /// regardless of backend. `execute` is only ever reached from
    /// `execute_python_tool_call`, itself only reachable when
    /// `state.runtime == Some("ipython")` -- a `NoopToolRuntime` session
    /// (every session that doesn't opt into `--runtime ipython`) never
    /// offers the `execute_python` tool in the first place (see
    /// `enabled_tool_defs`), so its own `execute` is exactly as unreached
    /// as before Increment 5, just no longer by construction of the
    /// trait alone.
    tool_runtime: Box<dyn ToolRuntime>,
    events: broadcast::Sender<SessionEvent>,
    /// Set by [`emit_recovery_marker`](Self::emit_recovery_marker),
    /// cleared by [`take_pending_recovery_marker`](Self::take_pending_recovery_marker).
    /// A crash-recovered worker calls the former before its private
    /// socket is even bound -- long before any client can possibly have
    /// called [`subscribe`](Self::subscribe) yet -- so broadcasting the
    /// marker at that point would reach zero receivers and be lost
    /// (`broadcast::Sender::send`'s own contract: only receivers
    /// subscribed *before* a send observe it, per `sync/broadcast.rs`).
    /// Stashing it here instead lets the first attach after a recovery
    /// deliver it deterministically, right after the snapshot.
    pending_recovery_marker: Option<SessionEvent>,
}

/// Creation-time-only metadata for a brand-new session -- bundled so
/// `AgentSession::create`/`worker::spawn`'s own argument lists don't keep
/// growing every time a new `session new`-seedable field (`--model`,
/// `--goal`, `session spawn`'s own `parent_id`) is added. Every field
/// here is meaningful only for [`AgentSession::create`]: a resumed or
/// recovered session reads the equivalent values back from its own
/// persisted `state.json` instead (see `WorkerArgs::goal`'s own doc
/// comment for why).
#[derive(Debug, Clone, Default)]
pub struct NewSessionMeta {
    pub name: Option<String>,
    pub model: Option<String>,
    pub goal: Option<String>,
    pub parent_id: Option<String>,
    pub thinking: Option<String>,
    pub tools: Option<String>,
    /// See `protocol::Request::SessionNew::runtime`'s own doc comment.
    pub runtime: Option<String>,
    /// See `protocol::SessionState::rlm_depth`'s own doc comment. `None`
    /// here means "not yet resolved" (defaults to `0` in
    /// [`AgentSession::create`]); by the time a real worker is spawned,
    /// `daemon::handle_session_new` has always resolved a real value,
    /// same treatment as `model`'s env-var fallback.
    pub rlm_depth: Option<u32>,
    /// See `protocol::SessionState::rlm_max_depth`'s own doc comment.
    /// `None` defaults to `1` in [`AgentSession::create`], same
    /// resolved-server-side treatment as `rlm_depth`.
    pub rlm_max_depth: Option<u32>,
    /// See `protocol::SessionState::spawned_from_sequence`'s own doc
    /// comment. Set only by `handle_rlm_run`'s own composition; every
    /// other caller leaves this `None`, same as `NewSessionMeta::
    /// default()`'s blanket default.
    pub spawned_from_sequence: Option<u64>,
}

/// `rlm-runtime.md`'s own stated default for `RLM_MAX_DEPTH`: a root
/// session may create children; those children may not create
/// grandchildren unless the limit is configured higher (`
/// RUSTY_PRIME_AGENT_RLM_MAX_DEPTH`, resolved in `daemon::
/// handle_session_new`).
pub(crate) const DEFAULT_RLM_MAX_DEPTH: u32 = 1;

impl AgentSession {
    /// Create a brand-new session: fresh id, empty transcript, generation
    /// 1. Persists the initial `state.json` before returning.
    pub async fn create(
        state_root: &Path,
        session_id: String,
        meta: NewSessionMeta,
        provider: Box<dyn ModelProvider>,
        tool_runtime: Box<dyn ToolRuntime>,
    ) -> Result<Self> {
        let NewSessionMeta {
            name,
            model,
            goal: goal_text,
            parent_id,
            thinking,
            tools,
            runtime,
            rlm_depth,
            rlm_max_depth,
            spawned_from_sequence,
        } = meta;
        let session_dir = paths::session_dir(state_root, &session_id);
        paths::ensure_dir(Context::Session, &session_dir)?;
        let now = now_ms();
        // Parity with `prime-agent --goal`: "starts a persistent goal
        // only for a new root session" -- this is the one place a goal
        // can be seeded at creation time; every other path goes through
        // `update_goal`'s `Set` action on an already-existing session.
        let goal = goal_text.map(|text| GoalState {
            text,
            status: GoalStatus::Active,
            created_at_ms: now,
            updated_at_ms: now,
        });
        let state = SessionState {
            session_id,
            name,
            status: SessionStatus::Active,
            worker_pid: Some(std::process::id()),
            generation: 1,
            last_sequence: 0,
            created_at_ms: now,
            updated_at_ms: now,
            model,
            goal,
            harness: HarnessState::default(),
            parent_id,
            thinking,
            tools,
            runtime,
            compaction: None,
            forked_from: None,
            rlm_depth: rlm_depth.unwrap_or(0),
            rlm_max_depth: rlm_max_depth.unwrap_or(DEFAULT_RLM_MAX_DEPTH),
            spawned_from_sequence,
        };
        let session = AgentSession {
            state,
            transcript: Vec::new(),
            session_dir,
            state_root: state_root.to_path_buf(),
            provider,
            mcp_client: None,
            tool_runtime,
            events: broadcast::channel(EVENT_CHANNEL_CAPACITY).0,
            pending_recovery_marker: None,
        };
        session.write_state().await?;
        Ok(session)
    }

    /// Recover an existing session: full-replay the transcript, bump the
    /// generation (a new worker process is taking ownership), and record
    /// this process as the new owner. `recovered` is true when the prior
    /// worker's pid was found dead (crash) rather than this simply being
    /// a clean resume of a `Stopped` session -- callers use it to decide
    /// whether to append a `RecoveryMarker`.
    pub async fn recover(
        state_root: &Path,
        session_id: &str,
        provider: Box<dyn ModelProvider>,
        tool_runtime: Box<dyn ToolRuntime>,
    ) -> Result<Self> {
        let session_dir = paths::session_dir(state_root, session_id);
        let mut state = read_state(&session_dir)?;
        let transcript = read_transcript(&session_dir)?;
        // The transcript is the source of truth for the sequence cursor;
        // `state.json` is only ever a best-effort cache of it (a crash
        // between an append and the state-file rewrite is exactly the
        // case this project's recovery test suite exercises).
        let transcript_last_sequence = transcript.last().map(|e| e.sequence).unwrap_or(0);
        state.last_sequence = state.last_sequence.max(transcript_last_sequence);
        state.generation += 1;
        state.status = SessionStatus::Active;
        state.worker_pid = Some(std::process::id());
        state.updated_at_ms = now_ms();

        let session = AgentSession {
            state,
            transcript,
            session_dir,
            state_root: state_root.to_path_buf(),
            provider,
            mcp_client: None,
            tool_runtime,
            events: broadcast::channel(EVENT_CHANNEL_CAPACITY).0,
            pending_recovery_marker: None,
        };
        session.write_state().await?;
        Ok(session)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }

    pub fn snapshot_event(&self) -> SessionEvent {
        SessionEvent::Snapshot {
            state: Box::new(self.state.clone()),
            transcript: self.transcript.clone(),
        }
    }

    /// Mark this session as recovered from a crashed worker (daemon.md:
    /// "appends a visible recovery marker to the transcript"). Not part
    /// of the transcript's `Role`-tagged turns -- delivered only as a
    /// `SessionEvent`, not persisted as a `TranscriptEntry`, since it
    /// documents a host-side fact about this process's lifetime, not
    /// something either party said.
    ///
    /// Stashed in `pending_recovery_marker` rather than broadcast
    /// directly: this is called during worker startup, before the
    /// private socket is even bound, so there is no way any client could
    /// have subscribed yet -- broadcasting now would reach zero
    /// receivers and be silently lost. `take_pending_recovery_marker`
    /// delivers it deterministically to the first attach instead.
    pub fn emit_recovery_marker(&mut self, message: impl Into<String>) {
        self.pending_recovery_marker = Some(SessionEvent::RecoveryMarker {
            message: message.into(),
            at_ms: now_ms(),
        });
    }

    /// Takes the pending recovery marker, if any -- delivered once, to
    /// whichever attach happens to be first after a crash recovery; a
    /// second, concurrent or later attach to the same still-running
    /// worker sees `None` here (`supervisor_restart_recovery.rs`'s own
    /// live-adopt case never calls `emit_recovery_marker` at all, so this
    /// is `None` there from the start, not merely consumed).
    pub fn take_pending_recovery_marker(&mut self) -> Option<SessionEvent> {
        self.pending_recovery_marker.take()
    }

    /// Append a user turn, then loop with the provider until it returns
    /// a plain text reply: each `ProviderReply::ToolCalls` round appends
    /// an assistant tool-call-request entry, executes every requested
    /// tool (`execute_tool_call` -- a built-in `tools::execute` call, an
    /// MCP proxy call, or a real IPython kernel `execute_python` call,
    /// depending on the tool's name and this session's `state.tools`/
    /// `state.runtime`), appends one `Role::Tool` result entry per call,
    /// and asks the provider again with the tool results now part of the
    /// conversation -- the same multi-turn tool-calling shape
    /// `rp-server`'s own `ChatRequest.tools`/`Role::Tool` support
    /// expects. `EchoProvider` never emits `ToolCalls`, so this is a
    /// single round for every session that doesn't opt into `--tools`/
    /// `--runtime`.
    ///
    /// Every step below goes through `append`, which synchronously
    /// persists to `transcript.jsonl` before returning -- a crash
    /// mid-loop just leaves a coherent partial transcript (e.g. a
    /// dangling tool-call request with no result yet), the same
    /// "an in-flight prompt might not complete if the worker dies
    /// mid-call" gap that already exists today for a plain, tool-less
    /// prompt, not a new one this loop introduces.
    ///
    /// Capped at `MAX_TOOL_ROUNDS` rounds to bound a runaway loop (a
    /// model that keeps requesting tools and never settles on a final
    /// reply) -- similar in spirit to `session_autonomous`'s own
    /// bounded-loop reasoning. Hitting the cap appends a synthetic
    /// assistant note instead of erroring, so the client still gets a
    /// coherent ack.
    pub async fn prompt(&mut self, text: String) -> Result<TranscriptEntry> {
        self.append(Role::User, text, None, None, None, None)
            .await?;
        let tools = self.enabled_tool_defs().await?;

        const MAX_TOOL_ROUNDS: usize = 8;
        for _ in 0..MAX_TOOL_ROUNDS {
            self.maybe_compact().await?;
            let turns = self.build_turns();
            let ProviderResponse { reply, usage } = self.provider.respond(&turns, &tools).await?;
            match reply {
                ProviderReply::Text(reply) => {
                    return self
                        .append(Role::Assistant, reply, None, None, None, usage)
                        .await;
                }
                ProviderReply::ToolCalls(calls) => {
                    self.append(
                        Role::Assistant,
                        String::new(),
                        Some(calls.clone()),
                        None,
                        None,
                        usage,
                    )
                    .await?;
                    for call in calls {
                        let result = self.execute_tool_call(&call.name, &call.arguments).await?;
                        self.append(
                            Role::Tool,
                            result,
                            None,
                            Some(call.id),
                            Some(call.name),
                            None,
                        )
                        .await?;
                    }
                }
            }
        }
        self.append(
            Role::Assistant,
            format!("(stopped after {MAX_TOOL_ROUNDS} tool-call rounds without a final reply)"),
            None,
            None,
            None,
            None,
        )
        .await
    }

    /// This session's offered tool set, per `state.tools` (`session new
    /// --tools read|mcp`) plus, independently, `execute_python` when
    /// `state.runtime == Some("ipython")` (`session new --runtime
    /// ipython`) -- the two flags are orthogonal (see `protocol::
    /// Request::SessionNew::runtime`'s own doc comment), so a session can
    /// offer either, both, or neither. `"read"` is a cheap, pure lookup;
    /// `"mcp"` needs a live `tools/list` call against `rp-server`'s MCP
    /// gateway (`mcp_client_or_connect`, lazily connecting/reusing one
    /// client for this session's whole lifetime) -- all recomputed on
    /// every `prompt` call rather than cached across prompts, so a
    /// session picks up a newly-connected MCP upstream (or one that
    /// dropped) without needing a restart.
    async fn enabled_tool_defs(&mut self) -> Result<Vec<ToolDef>> {
        let mut defs = match self.state.tools.as_deref() {
            Some("read") => crate::tools::read_only_tool_defs(),
            Some("mcp") => {
                let client = self.mcp_client_or_connect().await?;
                let tools = client.list_tools().await?;
                tools
                    .into_iter()
                    .map(|t| ToolDef {
                        name: t.name,
                        description: t.description,
                        parameters: t.input_schema,
                    })
                    .collect()
            }
            _ => Vec::new(),
        };
        if self.state.runtime.as_deref() == Some("ipython") {
            defs.push(self.execute_python_tool_def_with_skills()?);
        }
        Ok(defs)
    }

    /// Runs one tool call by name. `execute_python` (offered only when
    /// `state.runtime == Some("ipython")`, see `enabled_tool_defs`) is
    /// checked first and routed to `self.tool_runtime` -- the model-facing
    /// code execution environment boundary, a different backend than the
    /// OpenAI-style tool-calling ones below it and checked independently
    /// of `state.tools`. Everything else is routed to whichever backend
    /// `state.tools` selects -- `tools::execute` for `"read"` (and the
    /// default, tool-less case), `mcp_client::McpClient::call_tool`
    /// (proxied through `rp-server`'s gateway) for `"mcp"`. Only ever
    /// called with a name/arguments pair the provider itself just
    /// requested against the tool list `enabled_tool_defs` handed it, so
    /// neither `state.tools` nor `state.runtime` can have changed
    /// in between within one `prompt` call.
    async fn execute_tool_call(&mut self, name: &str, arguments: &str) -> Result<String> {
        if name == "execute_python" && self.state.runtime.as_deref() == Some("ipython") {
            return self.execute_python_tool_call(arguments).await;
        }
        match self.state.tools.as_deref() {
            Some("mcp") => {
                let client = self.mcp_client_or_connect().await?;
                client.call_tool(name, arguments).await
            }
            _ => Ok(crate::tools::execute(name, arguments)),
        }
    }

    /// `tools::execute_python_tool_def`'s base `ToolDef`, with the names
    /// (and descriptions, when given) of every skill `skills::discover`
    /// finds appended to its description -- so the model knows what it
    /// can `import` without a human having to say so in the prompt.
    /// Recomputed on every `prompt` call, same as `enabled_tool_defs`'s
    /// other sources: a skill installed (or removed) between prompts is
    /// picked up without needing a session restart.
    fn execute_python_tool_def_with_skills(&self) -> Result<ToolDef> {
        let mut def = crate::tools::execute_python_tool_def();
        let skills = crate::skills::discover(&self.state_root)?;
        if !skills.is_empty() {
            let listed: Vec<String> = skills
                .iter()
                .map(|s| match &s.description {
                    Some(d) => format!("{} — {d}", s.name),
                    None => s.name.clone(),
                })
                .collect();
            def.description.push_str(&format!(
                " Available skills, importable directly (e.g. `import {}`): {}.",
                skills[0].name,
                listed.join("; ")
            ));
        }
        Ok(def)
    }

    /// Parses `arguments` for a `code` string and runs it against this
    /// session's real IPython kernel (`self.tool_runtime`), formatting
    /// the resulting `ExecutionOutcome` into the plain-text shape every
    /// other tool result is (`tools::execute`'s own convention: `stdout`
    /// then `result`, one blank line apart when both are present). A
    /// malformed `arguments` payload is reported the same
    /// `"error: ..."`-prefixed way `tools::execute`'s own argument
    /// parsing is -- a model sending bad JSON is normal, recoverable
    /// model behavior, not a protocol failure. A genuine kernel-level
    /// failure (the connection dropped, a timeout) still propagates as a
    /// real `Result::Err`, the same as `mcp_client::McpClient::call_tool`'s
    /// own failure path.
    ///
    /// If the code called the kernel's own `rlm_heartbeat()` (defined by
    /// `worker::bootstrap_kernel`), its `HEARTBEAT_MARKER` shows up in
    /// `stdout` -- stripped from what the model sees and dispatched to
    /// `trigger_heartbeat` instead of shown as raw internal protocol
    /// noise.
    ///
    /// If the code instead calls `await host_request(...)` (directly, or
    /// via `rlm(...)`), `ToolRuntime::execute` returns with
    /// `pending_host_request` set instead of finishing -- this loops
    /// calling [`handle_host_request`](Self::handle_host_request) and
    /// `ToolRuntime::resume_execute` until the cell actually completes
    /// (a cell may `await` more than one host request), concatenating
    /// `stdout` across every pause the same way a single uninterrupted
    /// `execute` call's own `stdout` would read. `result` always reflects
    /// only the *last* resume, matching what `execute_result`/`error`
    /// naturally does in the underlying Jupyter protocol -- an earlier
    /// pause's own result, if it had one, was already an intermediate
    /// value the cell's own code consumed, not something worth surfacing
    /// to the model.
    async fn execute_python_tool_call(&mut self, arguments: &str) -> Result<String> {
        let value: serde_json::Value = match serde_json::from_str(arguments) {
            Ok(v) => v,
            Err(e) => return Ok(format!("error: invalid arguments JSON: {e}")),
        };
        let code = match value["code"].as_str() {
            Some(c) => c.to_string(),
            None => return Ok("error: missing required `code` argument".to_string()),
        };
        let mut outcome = self.tool_runtime.execute(&code).await?;
        let mut stdout = outcome.stdout;
        while let Some(request) = outcome.pending_host_request.take() {
            let reply = self
                .handle_host_request(&request.kind, request.payload)
                .await?;
            outcome = self
                .tool_runtime
                .resume_execute(&request.comm_id, reply)
                .await?;
            stdout.push_str(&outcome.stdout);
        }
        let result = outcome.result;

        let heartbeat = extract_heartbeat_marker(&stdout);
        let mut text = match &heartbeat {
            Some((_, without_marker)) => without_marker.clone(),
            None => stdout,
        };
        if let Some(result) = result {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&result);
        }
        if let Some((every, _)) = heartbeat {
            let every = if every.is_empty() { None } else { Some(every) };
            let status = self.trigger_heartbeat(every).await?;
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&status);
        }
        if text.is_empty() {
            text = "(no output)".to_string();
        }
        Ok(text)
    }

    /// Dispatches one `HostRequest` the kernel is blocked awaiting a
    /// reply to (see `tool_runtime::HostRequest`'s own doc comment) and
    /// returns the JSON value to send back via `ToolRuntime::
    /// resume_execute`. `rlm.run` is the only kind implemented so far;
    /// an unrecognized `kind` gets an `{"error": ...}` reply rather than
    /// a `HarnessError` -- the kernel-side caller is meant to see and
    /// handle it, the same "surface it to the model, don't fail the
    /// whole call" posture `execute_python_tool_call`'s own doc comment
    /// already applies to a Python-level exception.
    async fn handle_host_request(
        &self,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value> {
        match kind {
            "rlm.run" => self.handle_rlm_run(payload).await,
            "rlm.list_subagents" => self.handle_list_subagents().await,
            "rlm.delete_subagent" => self.handle_delete_subagent(payload).await,
            other => Ok(serde_json::json!({
                "error": format!("unknown host request kind {other:?}"),
            })),
        }
    }

    /// Handles a kernel-side `rlm(task, name=None, model=None)` call --
    /// parity with `prime-agent`'s kernel-callable `rlm(...)`,
    /// `packages/coding-agent/docs/rlm.md`. Admits a child session
    /// through the exact same `SessionNew`/`ScheduleAdd` daemon round
    /// trip `client::session_spawn` (`session spawn`) already uses --
    /// this is that same underlying mechanism, just issued from inside
    /// the worker process instead of an external CLI invocation, the
    /// same "connect to this session's own `daemon.sock` like any other
    /// client" pattern `trigger_heartbeat` already established. Returns
    /// immediately after admission, never waiting for the child's own
    /// reply -- parity with `rlm(...)` "returns immediately after task
    /// admission... never waits for or returns the child's answer"
    /// (`rlm-runtime.md`). Rejects admission once `RLM_DEPTH >=
    /// RLM_MAX_DEPTH` (see the check at the top of the body below). See
    /// [`handle_list_subagents`](Self::handle_list_subagents)/
    /// [`handle_delete_subagent`](Self::handle_delete_subagent) for the
    /// parent-scoped registry a child admitted here becomes visible
    /// through.
    async fn handle_rlm_run(&self, payload: serde_json::Value) -> Result<serde_json::Value> {
        // Parity with `rlm-runtime.md`'s `AgentSession.runRlmChild()`:
        // "Check `RLM_DEPTH < RLM_MAX_DEPTH`" is step 1, checked by the
        // parent before a child is ever admitted -- not something the
        // daemon rejects after the fact. `self.state.rlm_depth`/
        // `rlm_max_depth` are already loaded in memory (no daemon round
        // trip needed just to check this).
        if self.state.rlm_depth >= self.state.rlm_max_depth {
            return Ok(serde_json::json!({
                "error": format!(
                    "recursion depth limit reached (RLM_DEPTH={}, RLM_MAX_DEPTH={})",
                    self.state.rlm_depth, self.state.rlm_max_depth
                ),
            }));
        }
        let Some(task) = payload.get("task").and_then(|v| v.as_str()) else {
            return Ok(serde_json::json!({"error": "rlm.run requires a \"task\" string"}));
        };
        let name = payload
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let model = payload
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| self.state.model.clone());

        let socket_path = paths::daemon_socket_path(&self.state_root);
        let mut conn = transport::connect(Context::Daemon, socket_path).await?;
        conn.write_request(
            Context::Daemon,
            &Request::SessionNew {
                name: name.clone(),
                model: model.clone(),
                goal: None,
                parent_id: Some(self.state.session_id.clone()),
                // Parity with `rlm-runtime.md`'s "the target parent
                // assistant message ID": `self.state.last_sequence`, at
                // this exact point, is the sequence of the `Role::
                // Assistant` tool-calls entry `prompt`'s own loop just
                // appended before calling `execute_tool_call` -> ... ->
                // `handle_rlm_run` -- see `protocol::SessionState::
                // spawned_from_sequence`'s own doc comment.
                spawned_from_sequence: Some(self.state.last_sequence),
                thinking: None,
                tools: None,
                runtime: None,
            },
        )
        .await?;
        let response = conn.read_response(Context::Daemon).await?;
        let child_id = match response {
            Some(crate::protocol::Response::SessionNew { session_id }) => session_id,
            Some(crate::protocol::Response::Error { message, .. }) => {
                return Ok(serde_json::json!({ "error": message }));
            }
            other => {
                return Err(HarnessError::protocol(
                    Context::Daemon,
                    format!(
                        "expected a session_new response to rlm.run's SessionNew, got {other:?}"
                    ),
                ));
            }
        };

        let socket_path = paths::daemon_socket_path(&self.state_root);
        let mut conn = transport::connect(Context::Daemon, socket_path).await?;
        conn.write_request(
            Context::Daemon,
            &Request::ScheduleAdd {
                session_id: child_id.clone(),
                text: task.to_string(),
                kind: ScheduleKind::Once { at_ms: now_ms() },
            },
        )
        .await?;
        let _ =
            rusty_tokio::time::timeout(Duration::from_secs(5), conn.read_response(Context::Daemon))
                .await;

        let session_dir = paths::session_dir(&self.state_root, &child_id);
        Ok(serde_json::json!({
            "rlm_child_id": child_id,
            "name": name,
            "session_dir": session_dir.display().to_string(),
            "model": model,
        }))
    }

    /// Handles a kernel-side `rlm_list_subagents()` call -- parity with
    /// `rlm.list_subagents()`, `rlm-runtime.md`: "the TypeScript parent
    /// maintains the authoritative direct-child registry"/"returns stable
    /// child IDs, ... session IDs, names, directories, and running/
    /// completed status." This project has no separate registry data
    /// structure to maintain, though -- a child's own `parent_id` (set
    /// once, at admission, by `handle_rlm_run`'s `SessionNew` call) is
    /// already the durable record of the relationship, so "the registry"
    /// here is simply `session list` filtered down to this session's own
    /// direct children, the exact same derivation `client::
    /// session_children` (`session children <id>`) already performs --
    /// this is that same filter, just reached from inside the worker
    /// process instead of the CLI. Only *direct* children are visible,
    /// matching "parent-scoped": a grandchild admitted by one of this
    /// session's own children never appears here, the same boundary
    /// `session_children` already enforces.
    async fn handle_list_subagents(&self) -> Result<serde_json::Value> {
        let socket_path = paths::daemon_socket_path(&self.state_root);
        let mut conn = transport::connect(Context::Daemon, socket_path).await?;
        conn.write_request(Context::Daemon, &Request::SessionList)
            .await?;
        let response = conn.read_response(Context::Daemon).await?;
        let sessions = match response {
            Some(crate::protocol::Response::SessionList { sessions }) => sessions,
            other => {
                return Err(HarnessError::protocol(
                    Context::Daemon,
                    format!(
                        "expected a session_list response to rlm.list_subagents, got {other:?}"
                    ),
                ));
            }
        };
        let subagents: Vec<_> = sessions
            .into_iter()
            .filter(|s| s.parent_id.as_deref() == Some(self.state.session_id.as_str()))
            .map(|s| {
                let status = match s.status {
                    SessionStatus::Active => "active",
                    SessionStatus::Stopped => "stopped",
                    SessionStatus::Crashed => "crashed",
                };
                let session_dir = paths::session_dir(&self.state_root, &s.session_id);
                serde_json::json!({
                    "child_id": s.session_id,
                    "name": s.name,
                    "status": status,
                    "session_dir": session_dir.display().to_string(),
                })
            })
            .collect();
        Ok(serde_json::json!({ "subagents": subagents }))
    }

    /// Handles a kernel-side `rlm_delete_subagent(id)` call -- parity
    /// with `rlm.delete_subagent()`, `rlm-runtime.md`: "accepts an exact
    /// child ID, active-session ID, session ID, or unique name."
    /// `active-session ID` and `session ID` are the same concept here (no
    /// separate slot-id layer, same simplification `handle_rlm_run`'s own
    /// doc comment already notes for `rlm_child_id`), so `id` is matched
    /// against a direct child's `session_id` first, falling back to an
    /// exact, unique `name` match. Only a *direct* child of this session
    /// may be deleted -- matching "parent-scoped": an unrelated or
    /// grandchild session id is rejected the same as an unknown one, not
    /// silently accepted. "Deletion cancels or closes the runtime... It
    /// does not erase the transcript or artifacts on disk" maps exactly
    /// onto this project's own `session stop` (`Request::SessionStop`):
    /// gracefully shuts the worker down, leaves `state.json`/
    /// `transcript.jsonl` untouched. No separate tombstone entry is
    /// written -- the stopped child's own persisted `status: Stopped` in
    /// `state.json`, visible via `handle_list_subagents`/`session list`
    /// from then on, already serves as the durable record that it was
    /// deleted rather than crashed or never run, without inventing a
    /// second status-tracking mechanism to keep in sync with the first.
    async fn handle_delete_subagent(
        &self,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let Some(id) = payload.get("id").and_then(|v| v.as_str()) else {
            return Ok(
                serde_json::json!({"error": "rlm.delete_subagent requires an \"id\" string"}),
            );
        };

        let socket_path = paths::daemon_socket_path(&self.state_root);
        let mut conn = transport::connect(Context::Daemon, socket_path).await?;
        conn.write_request(Context::Daemon, &Request::SessionList)
            .await?;
        let response = conn.read_response(Context::Daemon).await?;
        let sessions = match response {
            Some(crate::protocol::Response::SessionList { sessions }) => sessions,
            other => {
                return Err(HarnessError::protocol(
                    Context::Daemon,
                    format!(
                        "expected a session_list response to rlm.delete_subagent, got {other:?}"
                    ),
                ));
            }
        };
        let children: Vec<_> = sessions
            .into_iter()
            .filter(|s| s.parent_id.as_deref() == Some(self.state.session_id.as_str()))
            .collect();
        let target = children
            .iter()
            .find(|s| s.session_id == id)
            .or_else(|| children.iter().find(|s| s.name.as_deref() == Some(id)));
        let Some(target) = target else {
            return Ok(serde_json::json!({
                "error": format!("{id:?} is not a direct child of this session"),
            }));
        };
        let child_id = target.session_id.clone();
        let name = target.name.clone();

        let socket_path = paths::daemon_socket_path(&self.state_root);
        let mut conn = transport::connect(Context::Daemon, socket_path).await?;
        conn.write_request(
            Context::Daemon,
            &Request::SessionStop {
                session_id: child_id.clone(),
            },
        )
        .await?;
        let response = conn.read_response(Context::Daemon).await?;
        match response {
            Some(crate::protocol::Response::SessionStopAck { .. }) => {}
            Some(crate::protocol::Response::Error { message, .. }) => {
                return Ok(serde_json::json!({ "error": message }));
            }
            other => {
                return Err(HarnessError::protocol(
                    Context::Daemon,
                    format!(
                        "expected a session_stop_ack response to rlm.delete_subagent, got {other:?}"
                    ),
                ));
            }
        }

        Ok(serde_json::json!({
            "deleted": child_id,
            "name": name,
        }))
    }

    /// Parity with `rlm-runtime.md`'s asynchronous child-usage-
    /// attribution mechanism: "Prime Agent asynchronously folds the
    /// child's assistant usage and cost into the parent assistant turn
    /// that launched it," persisting "a `child_usage_attributed` entry
    /// containing: the target parent assistant message ID; the child
    /// usage being attributed; and the resulting aggregate usage." Called
    /// on this (the *parent's*) own worker via `Request::
    /// AttributeChildUsage`, sent by `daemon::Supervisor`'s own
    /// background poll once `child_id`'s own worker has stopped -- the
    /// closest real "the child's task is done" signal this project's
    /// architecture has, since RLM children are ordinary long-running
    /// sessions here, not a bounded one-shot subprocess the way
    /// `rlm-runtime.md`'s own runtime treats them. The daemon's own
    /// eligibility check (child stopped, parent `Active`) is trusted
    /// rather than re-verified here -- the same "the caller already
    /// decided this was due" trust `Request::ScheduleAdd`-fired
    /// continuation prompts already extend to `fire_due_schedules`.
    ///
    /// Idempotent and safe to call more than once for the same
    /// `child_id`: delivery is at-least-once in spirit (a redelivered
    /// poll, e.g. after a lost ack), so before doing anything else this
    /// scans this session's own transcript for an existing attribution of
    /// `child_id` and returns `Ok(false)` (no new entry) if one is
    /// already there -- the same "check my own durable state first"
    /// pattern that makes redundant delivery here safe rather than a
    /// duplicate-counting bug. Also returns `Ok(false)` (not an error)
    /// when `child_id` isn't actually a direct child of this session, or
    /// was admitted some other way than `rlm(...)` (no `parent_message_
    /// sequence` to attribute anything to) -- both are "nothing to do"
    /// outcomes, not failures, matching `Response::
    /// AttributeChildUsageAck`'s own doc comment.
    pub(crate) async fn attribute_child_usage(&mut self, child_id: &str) -> Result<bool> {
        if self.transcript.iter().any(|e| {
            e.child_usage_attributed
                .as_ref()
                .is_some_and(|a| a.child_session_id == child_id)
        }) {
            return Ok(false);
        }
        let child_dir = paths::session_dir(&self.state_root, child_id);
        let child_state = crate::catalog::read_session_state(Context::Session, &child_dir)?;
        if child_state.parent_id.as_deref() != Some(self.state.session_id.as_str()) {
            return Ok(false);
        }
        let Some(parent_message_sequence) = child_state.spawned_from_sequence else {
            return Ok(false);
        };
        let child_usage = read_transcript(&child_dir)?
            .into_iter()
            .filter_map(|e| e.usage)
            .fold(Usage::default(), |acc, u| acc + u);
        // "The resulting aggregate usage": every attribution already
        // recorded against this same parent message, plus this one.
        let aggregate_usage = self
            .transcript
            .iter()
            .filter_map(|e| e.child_usage_attributed.as_ref())
            .filter(|a| a.parent_message_sequence == parent_message_sequence)
            .fold(child_usage, |acc, a| acc + a.child_usage);
        self.append_child_usage_attribution(ChildUsageAttribution {
            child_session_id: child_id.to_string(),
            parent_message_sequence,
            child_usage,
            aggregate_usage,
        })
        .await?;
        Ok(true)
    }

    /// Handles a kernel-side `rlm_heartbeat()` call -- parity with
    /// `prime-agent`'s manual re-entry trigger, see `HEARTBEAT_MARKER`'s
    /// own doc comment. With no `Active` goal, explains and schedules
    /// nothing (same precondition `session_autonomous` itself has for
    /// its own continuation prompts). Otherwise, connects to this
    /// process's own daemon -- an ordinary client connection to
    /// `daemon.sock`, the same `transport`/`Request`/`Response`
    /// primitives every `client.rs` function already uses -- and asks it
    /// to fire a continuation prompt (`Request::ScheduleAdd`) rather than
    /// calling `self.prompt()` directly (which would recurse into this
    /// same in-flight `prompt()` call and interleave transcript entries
    /// out of order) or writing `schedules.json` directly (`schedule.rs`'s
    /// own doc comment: it has exactly one safe writer, the daemon's own
    /// background firing loop -- a second, unsynchronized writer racing
    /// that loop's own read-modify-write could lose or resurrect
    /// entries). The response read is best-effort: a request that's
    /// already been written and accepted by the daemon has very likely
    /// already taken effect even if this connection doesn't get to read
    /// the ack back.
    ///
    /// `every`, when `Some`, is parity with `prime-agent`'s
    /// `rlm_heartbeat.create(interval=...)`/`/heartbeat every <duration>`:
    /// a `ScheduleKind::Every { interval_ms }` instead of the default
    /// `ScheduleKind::Once { at_ms: now_ms() }` -- the exact same
    /// `ScheduleAdd` request either way, just a different `kind`, reusing
    /// `schedule.rs`'s own recurring-fire support rather than this
    /// project inventing a second, parallel "repeat this" mechanism. An
    /// invalid duration string degrades to an explanatory string, the
    /// same graceful "no goal"/"not active" shape the two checks above
    /// already have, rather than propagating a hard error out of the
    /// whole in-flight tool call. The resulting schedule is listed and
    /// canceled the same way any other one is -- `session schedule
    /// list`/`cancel <id> <schedule-id>` -- no separate heartbeat-specific
    /// management surface needed.
    async fn trigger_heartbeat(&self, every: Option<String>) -> Result<String> {
        let Some(goal) = &self.state.goal else {
            return Ok("(heartbeat ignored: no active goal set)".to_string());
        };
        if goal.status != GoalStatus::Active {
            return Ok("(heartbeat ignored: goal is not active)".to_string());
        }
        let text = format!("Continue working toward the goal: {}", goal.text);

        let kind = match &every {
            Some(interval_str) => match crate::cli::parse_duration_ms(interval_str) {
                Ok(interval_ms) => ScheduleKind::Every { interval_ms },
                Err(e) => {
                    return Ok(format!(
                        "(heartbeat ignored: invalid every duration {interval_str:?}: {e})"
                    ))
                }
            },
            None => ScheduleKind::Once { at_ms: now_ms() },
        };

        let socket_path = paths::daemon_socket_path(&self.state_root);
        let mut conn = transport::connect(Context::Daemon, socket_path).await?;
        conn.write_request(
            Context::Daemon,
            &Request::ScheduleAdd {
                session_id: self.state.session_id.clone(),
                text,
                kind,
            },
        )
        .await?;
        let _ =
            rusty_tokio::time::timeout(Duration::from_secs(5), conn.read_response(Context::Daemon))
                .await;
        Ok(match every {
            Some(interval_str) => {
                format!("(heartbeat scheduled: will continue toward the goal every {interval_str})")
            }
            None => "(heartbeat scheduled: will continue toward the goal shortly)".to_string(),
        })
    }

    /// Returns this session's cached `McpClient`, connecting one (against
    /// the `rp-server` sidecar `rp_server::read_port` finds recorded for
    /// this state root) the first time it's needed. `read_port` returning
    /// `None` would mean `--tools mcp` reached a worker whose sidecar
    /// was never started -- `daemon::Supervisor::handle_session_new`'s
    /// own `ensure_running` call for `tools == Some("mcp")` is what's
    /// supposed to prevent that, so treat it as the same "this is a bug
    /// in spawn ordering" condition `worker::build_provider` already
    /// does for a session with a `model` set.
    async fn mcp_client_or_connect(&mut self) -> Result<&crate::mcp_client::McpClient> {
        if self.mcp_client.is_none() {
            let port = crate::rp_server::read_port(&self.state_root).ok_or_else(|| {
                HarnessError::conflict(
                    Context::Provider,
                    "session has --tools mcp set but no rp-server sidecar is recorded -- \
                     this is a bug in daemon::Supervisor's spawn ordering",
                )
            })?;
            self.mcp_client = Some(crate::mcp_client::McpClient::connect(port).await?);
        }
        Ok(self.mcp_client.as_ref().expect("just set it above"))
    }

    /// Maps the persisted transcript into the turn shape a
    /// `ModelProvider` expects -- see `provider::ChatTurn`'s own doc
    /// comment for why this is a separate type from `TranscriptEntry`.
    /// When `state.compaction` is set, entries at or before its
    /// `compacted_up_to_sequence` are replaced by one synthetic
    /// `TurnRole::System` turn carrying the running summary instead of
    /// being sent verbatim -- `transcript.jsonl`/`self.transcript`
    /// themselves are untouched either way (see `CompactionState`'s own
    /// doc comment), so this is the only place compaction is visible to
    /// the provider. Also where `<state_dir>/AGENTS.md`/`CLAUDE.md` (see
    /// `read_context_file`) becomes visible, as an even earlier system
    /// turn -- read fresh on every call (no caching, no persisted state)
    /// so an edit takes effect on the very next prompt, same as
    /// `skills::discover`/`prompt_template::discover` already do for
    /// their own on-disk sources.
    fn build_turns(&self) -> Vec<ChatTurn> {
        let boundary = self
            .state
            .compaction
            .as_ref()
            .map(|c| c.compacted_up_to_sequence)
            .unwrap_or(0);
        let mut turns = Vec::new();
        if let Some(context) = read_context_file(&self.state_root) {
            turns.push(ChatTurn {
                role: TurnRole::System,
                content: Some(context),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }
        if let Some(compaction) = &self.state.compaction {
            turns.push(ChatTurn {
                role: TurnRole::System,
                content: Some(format!(
                    "Summary of earlier conversation (compacted to save context): {}",
                    compaction.summary
                )),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }
        turns.extend(
            self.transcript
                .iter()
                .filter(|e| e.sequence > boundary)
                .map(|entry| {
                    let role = match entry.role {
                        Role::User => TurnRole::User,
                        Role::Assistant => TurnRole::Assistant,
                        Role::System => TurnRole::System,
                        Role::Tool => TurnRole::Tool,
                    };
                    // An assistant tool-call-request entry is persisted with
                    // empty `text` (nothing user-visible to show) -- mirror
                    // that back to `None` rather than `Some("")`, matching
                    // `rp-server`'s own `content: null` convention for it.
                    let content = if entry.role == Role::Assistant && entry.tool_calls.is_some() {
                        None
                    } else {
                        Some(entry.text.clone())
                    };
                    ChatTurn {
                        role,
                        content,
                        tool_calls: entry.tool_calls.clone(),
                        tool_call_id: entry.tool_call_id.clone(),
                        name: entry.name.clone(),
                    }
                }),
        );
        turns
    }

    /// Checked once per round of `prompt`'s tool-calling loop, right
    /// before building the turns that go to the provider. A no-op for
    /// `EchoProvider` sessions (`state.model.is_none()` -- there's no
    /// real model to ask for a summary, and `EchoProvider` never risks a
    /// real context-window overflow anyway) and for a session whose
    /// current turns are still under `compact_trigger_tokens()`.
    async fn maybe_compact(&mut self) -> Result<()> {
        if self.state.model.is_none() {
            return Ok(());
        }
        let turns = self.build_turns();
        let total: usize = turns
            .iter()
            .map(|t| estimate_tokens(t.content.as_deref().unwrap_or("")))
            .sum();
        if total <= compact_trigger_tokens(&self.state_root) {
            return Ok(());
        }
        self.compact_now(None).await?;
        Ok(())
    }

    /// Forces compaction now, parity with `prime-agent /compact
    /// [instructions]`. Returns `(compacted, summary)`: `compacted` is
    /// false, and `summary` unchanged, for either of two honest no-op
    /// cases -- no `model` set (nothing to summarize with), or nothing
    /// past the already-compacted boundary is old enough to fold away
    /// yet (`find_compaction_fold_count` returns 0). Otherwise, asks
    /// `self.provider` itself to produce an updated running summary
    /// (the previous summary, if any, plus the newly-old turns), then
    /// records a `Role::System` transcript entry documenting that
    /// compaction happened -- visible in `session attach`/`session
    /// repl` the same way any other turn is, even though
    /// `transcript.jsonl` itself is never rewritten or truncated (see
    /// `CompactionState`'s own doc comment).
    pub async fn compact_now(
        &mut self,
        instructions: Option<String>,
    ) -> Result<(bool, Option<String>)> {
        if self.state.model.is_none() {
            return Ok((false, None));
        }
        let already_compacted_seq = self
            .state
            .compaction
            .as_ref()
            .map(|c| c.compacted_up_to_sequence)
            .unwrap_or(0);
        let candidates: Vec<TranscriptEntry> = self
            .transcript
            .iter()
            .filter(|e| e.sequence > already_compacted_seq)
            .cloned()
            .collect();
        let fold_count =
            find_compaction_fold_count(&candidates, compact_keep_recent_tokens(&self.state_root));
        if fold_count == 0 {
            return Ok((
                false,
                self.state.compaction.as_ref().map(|c| c.summary.clone()),
            ));
        }
        let to_fold = &candidates[..fold_count];
        let new_boundary_seq = to_fold
            .last()
            .expect("fold_count > 0 implies a last element")
            .sequence;

        let mut prompt_text = String::new();
        if let Some(prev) = &self.state.compaction {
            prompt_text.push_str("Previous summary of the conversation so far:\n");
            prompt_text.push_str(&prev.summary);
            prompt_text.push_str("\n\n");
        }
        prompt_text.push_str("Additional conversation to fold into that summary:\n");
        for entry in to_fold {
            prompt_text.push_str(&format!("{:?}: {}\n", entry.role, entry.text));
        }
        if let Some(instructions) = &instructions {
            prompt_text.push_str(&format!("\nFocus the summary on: {instructions}\n"));
        }
        prompt_text.push_str(
            "\nReply with only an updated, concise summary capturing the important facts, \
             decisions, and current state -- no preamble.",
        );
        let ask = [ChatTurn {
            role: TurnRole::User,
            content: Some(prompt_text),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }];
        // This call's own `usage` isn't recorded anywhere: it produces a
        // `Role::System` compaction-summary entry, not a `Role::
        // Assistant` reply to the user, and `TranscriptEntry::usage`/
        // child-usage attribution are both scoped to the latter (the
        // "parent assistant message" `rlm-runtime.md`'s own attribution
        // mechanism targets) -- tracking meta-call usage like this one is
        // a separate concern, not attempted here.
        let summary = match self.provider.respond(&ask, &[]).await?.reply {
            ProviderReply::Text(text) => text,
            ProviderReply::ToolCalls(_) => {
                "(compaction summary unavailable: model requested tools instead of summarizing)"
                    .to_string()
            }
        };

        self.state.compaction = Some(CompactionState {
            compacted_up_to_sequence: new_boundary_seq,
            summary: summary.clone(),
            compacted_at_ms: now_ms(),
        });
        self.write_state().await?;
        self.append(
            Role::System,
            format!(
                "(compacted {} turn{} into a running summary)",
                fold_count,
                if fold_count == 1 { "" } else { "s" }
            ),
            None,
            None,
            None,
            None,
        )
        .await?;
        Ok((true, Some(summary)))
    }

    async fn append(
        &mut self,
        role: Role,
        text: String,
        tool_calls: Option<Vec<ToolCallRequest>>,
        tool_call_id: Option<String>,
        name: Option<String>,
        usage: Option<Usage>,
    ) -> Result<TranscriptEntry> {
        self.append_entry(TranscriptEntry {
            sequence: self.state.last_sequence + 1,
            timestamp_ms: now_ms(),
            role,
            text,
            tool_calls,
            tool_call_id,
            name,
            usage,
            child_usage_attributed: None,
        })
        .await
    }

    /// A synthetic `Role::System` entry recording a child session's usage
    /// folded into one of this session's own assistant turns -- see
    /// [`ChildUsageAttribution`] and
    /// [`attribute_child_usage`](Self::attribute_child_usage).
    async fn append_child_usage_attribution(
        &mut self,
        attribution: ChildUsageAttribution,
    ) -> Result<TranscriptEntry> {
        self.append_entry(TranscriptEntry {
            sequence: self.state.last_sequence + 1,
            timestamp_ms: now_ms(),
            role: Role::System,
            text: String::new(),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            usage: None,
            child_usage_attributed: Some(attribution),
        })
        .await
    }

    /// Shared persistence tail for [`append`](Self::append)/
    /// [`append_child_usage_attribution`](Self::append_child_usage_attribution):
    /// both build a complete [`TranscriptEntry`] themselves (their shapes
    /// differ enough -- one keyed by `role`/`text`/tool-call fields, the
    /// other by `child_usage_attributed` -- that a single one-size-fits-
    /// all parameter list would need most of its parameters `None` on
    /// every call from *some* caller) and hand it here for the actual
    /// write.
    async fn append_entry(&mut self, entry: TranscriptEntry) -> Result<TranscriptEntry> {
        append_transcript_line(&self.session_dir, &entry).await?;
        self.transcript.push(entry.clone());
        self.state.last_sequence = entry.sequence;
        self.state.updated_at_ms = entry.timestamp_ms;
        self.write_state().await?;
        // No receivers is the ordinary "nobody attached right now" case,
        // not an error -- the transcript write above is what actually
        // makes this turn durable.
        let _ = self.events.send(SessionEvent::Turn {
            entry: entry.clone(),
        });
        Ok(entry)
    }

    async fn write_state(&self) -> Result<()> {
        write_state(&self.session_dir, &self.state).await
    }

    /// Parity with `prime-agent rename <agent> <name>`. Goes through the
    /// worker (rather than the daemon rewriting `state.json` directly)
    /// for the same reason `prompt`/`mark_stopped` do: this process is
    /// `state`'s one owner while it's running, and its own periodic
    /// `write_state` calls (e.g. after the next `prompt`) would silently
    /// clobber a rename applied out from under it.
    pub async fn rename(&mut self, name: Option<String>) -> Result<()> {
        self.state.name = name;
        self.state.updated_at_ms = now_ms();
        self.write_state().await
    }

    /// Parity with `prime-agent --goal`/`/goal`. `Pause`/`Resume`/
    /// `Complete` on a session with no current goal are accepted as
    /// no-ops rather than errors -- a caller racing a `Clear` (or one
    /// that simply mis-tracked state) gets back "no goal" (`None`)
    /// either way, the same observable outcome a rejected transition
    /// would have left it in, without needing a separate error path
    /// this project's own `Response::Error` "conflict" flag would just
    /// have to explain away.
    pub async fn update_goal(&mut self, action: GoalAction) -> Result<Option<GoalState>> {
        let now = now_ms();
        match action {
            GoalAction::Set { text } => {
                self.state.goal = Some(GoalState {
                    text,
                    status: GoalStatus::Active,
                    created_at_ms: now,
                    updated_at_ms: now,
                });
            }
            GoalAction::Clear => self.state.goal = None,
            GoalAction::Pause => {
                if let Some(goal) = &mut self.state.goal {
                    goal.status = GoalStatus::Paused;
                    goal.updated_at_ms = now;
                }
            }
            GoalAction::Resume => {
                if let Some(goal) = &mut self.state.goal {
                    goal.status = GoalStatus::Active;
                    goal.updated_at_ms = now;
                }
            }
            GoalAction::Complete => {
                if let Some(goal) = &mut self.state.goal {
                    goal.status = GoalStatus::Completed;
                    goal.updated_at_ms = now;
                }
            }
        }
        self.state.updated_at_ms = now;
        self.write_state().await?;
        Ok(self.state.goal.clone())
    }

    /// Parity with `prime-agent`'s Continual Harness (`/refine`). `Add`
    /// appends one note; `Rollback` restores `notes` to an earlier
    /// recorded version. Either way, the resulting `notes` gets appended
    /// to `history` as a fresh entry -- `history.last()` always mirrors
    /// `notes`, so a rollback becomes part of the auditable trail rather
    /// than erasing anything from it (see `HarnessSnapshot`'s own doc
    /// comment).
    pub async fn update_harness(&mut self, action: HarnessAction) -> Result<HarnessState> {
        let now = now_ms();
        let reason = match action {
            HarnessAction::Add { kind, text } => {
                let preview: String = text.chars().take(40).collect();
                self.state.harness.notes.push(HarnessNote {
                    id: new_harness_note_id(),
                    kind,
                    text,
                    added_at_ms: now,
                });
                format!("add {kind:?} note: {preview}")
            }
            HarnessAction::Rollback { index } => {
                let snapshot = self
                    .state
                    .harness
                    .history
                    .get(index)
                    .ok_or_else(|| {
                        HarnessError::conflict(
                            Context::Session,
                            format!(
                                "no history entry at index {index} ({} recorded)",
                                self.state.harness.history.len()
                            ),
                        )
                    })?
                    .clone();
                self.state.harness.notes = snapshot.notes;
                format!("rollback to history[{index}]")
            }
        };
        self.state.harness.history.push(HarnessSnapshot {
            notes: self.state.harness.notes.clone(),
            recorded_at_ms: now,
            reason,
        });
        self.state.updated_at_ms = now;
        self.write_state().await?;
        Ok(self.state.harness.clone())
    }

    pub async fn mark_stopped(&mut self) -> Result<()> {
        self.state.status = SessionStatus::Stopped;
        self.state.updated_at_ms = now_ms();
        self.write_state().await?;
        self.tool_runtime.shutdown().await?;
        let _ = self.events.send(SessionEvent::SessionEnded);
        Ok(())
    }
}

fn read_state(session_dir: &Path) -> Result<SessionState> {
    let path = paths::state_file_path(session_dir);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| HarnessError::io(Context::Session, Some(path.clone()), e))?;
    serde_json::from_str(&text).map_err(|e| HarnessError::json(Context::Session, Some(path), e))
}

/// Reads a source session's own persisted state and transcript directly
/// from disk -- no daemon/worker connection involved, the same "files
/// are the source of truth" reasoning `catalog::scan` already relies on
/// -- truncated to `at_sequence` if given. Used by `daemon::
/// handle_session_fork` to snapshot a fork point before seeding a
/// brand-new session directory with the result (see
/// [`seed_forked_session`]). Errors (a conflict, not a panic) if
/// `at_sequence` names a point past the transcript's real end --
/// computed as `max(state.last_sequence, transcript.last().sequence)`,
/// the same "state.json is only a best-effort cache" reasoning
/// `AgentSession::recover` already uses, so a source session whose
/// worker crashed between an `append` and its own `write_state` call
/// still reports its true last sequence here.
pub(crate) fn snapshot_for_fork(
    session_dir: &Path,
    at_sequence: Option<u64>,
) -> Result<(SessionState, Vec<TranscriptEntry>)> {
    let state = read_state(session_dir)?;
    let mut transcript = read_transcript(session_dir)?;
    if let Some(at_sequence) = at_sequence {
        let real_last_sequence = state
            .last_sequence
            .max(transcript.last().map(|e| e.sequence).unwrap_or(0));
        if at_sequence > real_last_sequence {
            return Err(HarnessError::conflict(
                Context::Session,
                format!(
                    "no transcript entry at or before sequence {at_sequence} \
                     (this session's last sequence is {real_last_sequence})"
                ),
            ));
        }
        transcript.retain(|e| e.sequence <= at_sequence);
    }
    Ok((state, transcript))
}

/// Seeds a brand-new session directory with `entries` (a prefix of some
/// other session's own transcript, from [`snapshot_for_fork`]) plus a
/// fresh `state.json` carrying forward `source`'s model/thinking/tools/
/// runtime configuration -- deliberately NOT `goal`/`harness`, whose
/// narrative content is only accurate as of `source`'s *current* full
/// history, not necessarily this fork's truncated one (see
/// `ForkedFrom`'s own doc comment).
///
/// `worker::spawn`'s `WorkerMode::Resume` reads this back via
/// `AgentSession::recover`, the same as resuming any other stopped
/// session -- there's no `WorkerMode::New`-shaped path that could take a
/// non-empty starting transcript (`AgentSession::create` always starts
/// `last_sequence` at 0 and never touches `transcript.jsonl`), so this
/// writes exactly what a normal session's own `append`/`write_state`
/// calls would have produced over time, then lets `recover`'s ordinary
/// full-replay pick it up like any other session a worker is resuming.
pub(crate) async fn seed_forked_session(
    state_root: &Path,
    new_session_id: &str,
    name: Option<String>,
    source: &SessionState,
    entries: Vec<TranscriptEntry>,
    forked_from: ForkedFrom,
) -> Result<()> {
    let session_dir = paths::session_dir(state_root, new_session_id);
    paths::ensure_dir(Context::Session, &session_dir)?;
    let last_sequence = entries.last().map(|e| e.sequence).unwrap_or(0);
    for entry in &entries {
        append_transcript_line(&session_dir, entry).await?;
    }
    let now = now_ms();
    let state = SessionState {
        session_id: new_session_id.to_string(),
        name,
        status: SessionStatus::Active,
        worker_pid: None,
        generation: 0,
        last_sequence,
        created_at_ms: now,
        updated_at_ms: now,
        model: source.model.clone(),
        goal: None,
        harness: HarnessState::default(),
        parent_id: None,
        thinking: source.thinking.clone(),
        tools: source.tools.clone(),
        runtime: source.runtime.clone(),
        compaction: None,
        forked_from: Some(forked_from),
        // Same "fresh, standalone session" treatment `daemon::
        // handle_session_fork`'s own `NewSessionMeta` construction gives
        // this via `AgentSession::create`'s defaults -- a fork isn't
        // tied into the source's own recursion tree.
        rlm_depth: 0,
        rlm_max_depth: DEFAULT_RLM_MAX_DEPTH,
        // Same reasoning as `rlm_depth`/`rlm_max_depth` above -- a fork
        // isn't a child `rlm(...)` admitted, so it has no parent message
        // to ever attribute usage back to.
        spawned_from_sequence: None,
    };
    write_state(&session_dir, &state).await
}

async fn write_state(session_dir: &Path, state: &SessionState) -> Result<()> {
    let path = paths::state_file_path(session_dir);
    let state = state.clone();
    let json = serde_json::to_string_pretty(&state)
        .map_err(|e| HarnessError::json(Context::Session, Some(path.clone()), e))?;
    let join = rusty_tokio::spawn_blocking(move || {
        // Write-to-temp-then-rename: a crash mid-write must never leave
        // `state.json` truncated/partial, since recovery reads it
        // directly (the transcript is the real source of truth, but a
        // corrupt pointer file would still fail an otherwise-clean
        // recovery for no reason).
        let tmp_path = path.with_extension("json.tmp");
        std::fs::write(&tmp_path, json.as_bytes())
            .and_then(|_| std::fs::rename(&tmp_path, &path))
            .map_err(|e| HarnessError::io(Context::Session, Some(path), e))
    })
    .await;
    join.map_err(|_| HarnessError::protocol(Context::Session, "state write task panicked"))?
}

fn read_transcript(session_dir: &Path) -> Result<Vec<TranscriptEntry>> {
    let path = paths::transcript_path(session_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(&path)
        .map_err(|e| HarnessError::io(Context::Session, Some(path.clone()), e))?;
    let reader = std::io::BufReader::new(file);
    let mut entries = Vec::new();
    for (line_no, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| HarnessError::io(Context::Session, Some(path.clone()), e))?;
        if line.trim().is_empty() {
            // A trailing blank line (e.g. from a partially-flushed final
            // write) is not a corrupt entry -- skip rather than fail
            // recovery over it.
            continue;
        }
        let entry: TranscriptEntry = serde_json::from_str(&line).map_err(|e| {
            HarnessError::protocol(
                Context::Session,
                format!("transcript.jsonl line {}: {e}", line_no + 1),
            )
        })?;
        entries.push(entry);
    }
    Ok(entries)
}

async fn append_transcript_line(session_dir: &Path, entry: &TranscriptEntry) -> Result<()> {
    let path = paths::transcript_path(session_dir);
    let line = serde_json::to_string(entry)
        .map_err(|e| HarnessError::json(Context::Session, Some(path.clone()), e))?;
    let join = rusty_tokio::spawn_blocking(move || {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| HarnessError::io(Context::Session, Some(path.clone()), e))?;
        writeln!(file, "{line}").map_err(|e| HarnessError::io(Context::Session, Some(path), e))?;
        file.flush()
            .map_err(|e| HarnessError::io(Context::Session, None, e))
    })
    .await;
    join.map_err(|_| HarnessError::protocol(Context::Session, "transcript append task panicked"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(sequence: u64, text: &str) -> TranscriptEntry {
        TranscriptEntry {
            sequence,
            timestamp_ms: 0,
            role: Role::User,
            text: text.to_string(),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            usage: None,
            child_usage_attributed: None,
        }
    }

    fn temp_state_root(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rpa-session-test-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_context_file_returns_none_when_neither_file_exists() {
        let root = temp_state_root("context-none");
        assert_eq!(read_context_file(&root), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn read_context_file_finds_agents_md() {
        let root = temp_state_root("context-agents");
        std::fs::write(root.join("AGENTS.md"), "be concise").unwrap();
        assert_eq!(read_context_file(&root).as_deref(), Some("be concise"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn read_context_file_falls_back_to_claude_md() {
        let root = temp_state_root("context-claude");
        std::fs::write(root.join("CLAUDE.md"), "use tabs").unwrap();
        assert_eq!(read_context_file(&root).as_deref(), Some("use tabs"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn read_context_file_prefers_agents_md_over_claude_md() {
        let root = temp_state_root("context-both");
        std::fs::write(root.join("AGENTS.md"), "from agents").unwrap();
        std::fs::write(root.join("CLAUDE.md"), "from claude").unwrap();
        assert_eq!(read_context_file(&root).as_deref(), Some("from agents"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn read_context_file_treats_a_whitespace_only_file_as_missing() {
        let root = temp_state_root("context-blank");
        std::fs::write(root.join("AGENTS.md"), "   \n\n  ").unwrap();
        assert_eq!(read_context_file(&root), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// End-to-end proof (no daemon/worker needed -- `AgentSession` is a
    /// plain Rust type) that a state-root `AGENTS.md` actually reaches
    /// `build_turns`'s output, not just `read_context_file` in
    /// isolation.
    #[rusty_tokio::test]
    async fn build_turns_prepends_the_context_file_as_a_system_turn() {
        let root = temp_state_root("build-turns-context");
        std::fs::write(root.join("AGENTS.md"), "be terse").unwrap();

        let session = AgentSession::create(
            &root,
            "sess-context-test".to_string(),
            NewSessionMeta::default(),
            Box::new(crate::provider::EchoProvider),
            Box::new(crate::tool_runtime::NoopToolRuntime),
        )
        .await
        .expect("session creation should succeed");

        let turns = session.build_turns();
        assert_eq!(turns.len(), 1, "got: {turns:?}");
        assert_eq!(turns[0].role, TurnRole::System);
        assert_eq!(turns[0].content.as_deref(), Some("be terse"));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[rusty_tokio::test]
    async fn build_turns_has_no_system_turn_when_no_context_file_exists() {
        let root = temp_state_root("build-turns-no-context");

        let session = AgentSession::create(
            &root,
            "sess-no-context-test".to_string(),
            NewSessionMeta::default(),
            Box::new(crate::provider::EchoProvider),
            Box::new(crate::tool_runtime::NoopToolRuntime),
        )
        .await
        .expect("session creation should succeed");

        assert!(session.build_turns().is_empty());

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Parity with `rlm-runtime.md`'s `AgentSession.runRlmChild()` step 1
    /// ("Check `RLM_DEPTH < RLM_MAX_DEPTH`"): proven directly against
    /// `handle_host_request`/`handle_rlm_run` with no daemon/kernel
    /// involved at all, since the depth check happens before either is
    /// ever touched -- same "direct, deterministic proof" reasoning the
    /// real-kernel tests use for kernel-side behavior, applied here to
    /// the purely in-memory half of the mechanism.
    #[rusty_tokio::test]
    async fn handle_rlm_run_rejects_admission_once_the_depth_limit_is_reached() {
        let root = temp_state_root("rlm-depth-limit");

        let session = AgentSession::create(
            &root,
            "sess-depth-test".to_string(),
            NewSessionMeta {
                rlm_depth: Some(1),
                rlm_max_depth: Some(1),
                ..NewSessionMeta::default()
            },
            Box::new(crate::provider::EchoProvider),
            Box::new(crate::tool_runtime::NoopToolRuntime),
        )
        .await
        .expect("session creation should succeed");

        let response = session
            .handle_host_request("rlm.run", serde_json::json!({"task": "do something"}))
            .await
            .expect("handle_host_request should not itself error");

        let error = response
            .get("error")
            .and_then(|v| v.as_str())
            .expect("rejected admission should carry an \"error\" field");
        assert!(
            error.contains("recursion depth limit reached"),
            "got: {error:?}"
        );
        assert!(error.contains("RLM_DEPTH=1"), "got: {error:?}");
        assert!(error.contains("RLM_MAX_DEPTH=1"), "got: {error:?}");

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Parity with `rlm-runtime.md`'s child-usage-attribution mechanism,
    /// proven the same "no daemon/kernel needed" way as the depth-limit
    /// test above: `attribute_child_usage` only ever touches this
    /// session's own in-memory transcript plus plain reads of another
    /// session's already-durable `state.json`/`transcript.jsonl`, so it's
    /// fully exercisable with two directly-constructed `AgentSession`s.
    #[rusty_tokio::test]
    async fn attribute_child_usage_folds_the_childs_usage_into_a_new_parent_entry() {
        let root = temp_state_root("attribute-child-usage");

        let mut parent = AgentSession::create(
            &root,
            "parent-1".to_string(),
            NewSessionMeta::default(),
            Box::new(crate::provider::EchoProvider),
            Box::new(crate::tool_runtime::NoopToolRuntime),
        )
        .await
        .expect("parent creation should succeed");
        // Simulate the assistant tool-calls turn that admitted the child
        // -- `handle_rlm_run` captures `self.state.last_sequence` at
        // exactly this point.
        let launching_entry = parent
            .append(Role::Assistant, String::new(), None, None, None, None)
            .await
            .expect("seeding the launching assistant entry should succeed");

        let mut child = AgentSession::create(
            &root,
            "child-1".to_string(),
            NewSessionMeta {
                parent_id: Some("parent-1".to_string()),
                spawned_from_sequence: Some(launching_entry.sequence),
                ..NewSessionMeta::default()
            },
            Box::new(crate::provider::EchoProvider),
            Box::new(crate::tool_runtime::NoopToolRuntime),
        )
        .await
        .expect("child creation should succeed");
        child
            .append(
                Role::Assistant,
                "first".to_string(),
                None,
                None,
                None,
                Some(Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                }),
            )
            .await
            .unwrap();
        child
            .append(
                Role::Assistant,
                "second".to_string(),
                None,
                None,
                None,
                Some(Usage {
                    prompt_tokens: 3,
                    completion_tokens: 2,
                    total_tokens: 5,
                }),
            )
            .await
            .unwrap();
        drop(child);

        let attributed = parent
            .attribute_child_usage("child-1")
            .await
            .expect("attribution should not error");
        assert!(attributed);

        let attribution_entry = parent
            .transcript
            .last()
            .expect("an attribution entry should have been appended");
        assert_eq!(attribution_entry.role, Role::System);
        let attribution = attribution_entry
            .child_usage_attributed
            .as_ref()
            .expect("the appended entry should carry a child_usage_attributed payload");
        assert_eq!(attribution.child_session_id, "child-1");
        assert_eq!(
            attribution.parent_message_sequence,
            launching_entry.sequence
        );
        assert_eq!(
            attribution.child_usage,
            Usage {
                prompt_tokens: 13,
                completion_tokens: 7,
                total_tokens: 20,
            }
        );
        assert_eq!(attribution.aggregate_usage, attribution.child_usage);

        // Idempotent: a redelivered request is a safe no-op, not a
        // second entry.
        let attributed_again = parent
            .attribute_child_usage("child-1")
            .await
            .expect("a redundant attribution attempt should not error");
        assert!(!attributed_again);
        assert_eq!(
            parent
                .transcript
                .iter()
                .filter(|e| e.child_usage_attributed.is_some())
                .count(),
            1,
            "a redundant attribution attempt must not append a second entry"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A `session spawn`-admitted child (`parent_id` set, but never
    /// `rlm(...)`-admitted, so no `spawned_from_sequence`) has no parent
    /// message to attribute usage to -- a no-op, not an error.
    #[rusty_tokio::test]
    async fn attribute_child_usage_is_a_no_op_for_a_non_rlm_admitted_child() {
        let root = temp_state_root("attribute-child-usage-non-rlm");

        let mut parent = AgentSession::create(
            &root,
            "parent-2".to_string(),
            NewSessionMeta::default(),
            Box::new(crate::provider::EchoProvider),
            Box::new(crate::tool_runtime::NoopToolRuntime),
        )
        .await
        .expect("parent creation should succeed");

        AgentSession::create(
            &root,
            "child-2".to_string(),
            NewSessionMeta {
                parent_id: Some("parent-2".to_string()),
                spawned_from_sequence: None,
                ..NewSessionMeta::default()
            },
            Box::new(crate::provider::EchoProvider),
            Box::new(crate::tool_runtime::NoopToolRuntime),
        )
        .await
        .expect("child creation should succeed");

        let attributed = parent
            .attribute_child_usage("child-2")
            .await
            .expect("attribution should not error");
        assert!(!attributed);
        assert!(parent
            .transcript
            .iter()
            .all(|e| e.child_usage_attributed.is_none()));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn extract_heartbeat_marker_returns_none_when_the_marker_is_absent() {
        assert_eq!(
            extract_heartbeat_marker("just some ordinary output\n"),
            None
        );
    }

    #[test]
    fn extract_heartbeat_marker_finds_a_bare_one_shot_call() {
        let stdout = format!("before\n{HEARTBEAT_MARKER}\nafter\n");
        let (every, without_marker) = extract_heartbeat_marker(&stdout).unwrap();
        assert_eq!(every, "");
        assert_eq!(without_marker, "before\n\nafter\n");
    }

    #[test]
    fn extract_heartbeat_marker_finds_an_every_argument() {
        let stdout = format!("{HEARTBEAT_MARKER}10m\n");
        let (every, without_marker) = extract_heartbeat_marker(&stdout).unwrap();
        assert_eq!(every, "10m");
        assert_eq!(without_marker, "\n");
    }

    #[test]
    fn extract_heartbeat_marker_handles_a_marker_with_no_trailing_newline() {
        let stdout = format!("{HEARTBEAT_MARKER}1h");
        let (every, without_marker) = extract_heartbeat_marker(&stdout).unwrap();
        assert_eq!(every, "1h");
        assert_eq!(without_marker, "");
    }

    #[test]
    fn estimate_tokens_is_roughly_four_chars_per_token() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        // Rounds down -- an approximate heuristic, not an exact count.
        assert_eq!(estimate_tokens("abc"), 0);
    }

    #[test]
    fn find_compaction_fold_count_folds_nothing_under_the_keep_recent_budget() {
        let candidates = vec![entry(1, "short"), entry(2, "also short")];
        assert_eq!(find_compaction_fold_count(&candidates, 1_000), 0);
    }

    #[test]
    fn find_compaction_fold_count_folds_the_oldest_entries_past_the_keep_recent_budget() {
        // Each entry is "abcd" -> 1 estimated token. A keep-recent budget
        // of 2 tokens should keep the newest 2 entries verbatim and fold
        // the oldest 3 away.
        let candidates: Vec<TranscriptEntry> = (1..=5).map(|seq| entry(seq, "abcd")).collect();
        assert_eq!(find_compaction_fold_count(&candidates, 2), 3);
    }

    #[test]
    fn find_compaction_fold_count_folds_everything_when_even_the_newest_exceeds_budget() {
        // A keep-recent budget of 0 is exceeded by the single newest
        // entry alone, so every candidate folds -- an honest degenerate
        // case, not a special-cased minimum.
        let candidates: Vec<TranscriptEntry> = (1..=5).map(|seq| entry(seq, "abcd")).collect();
        assert_eq!(find_compaction_fold_count(&candidates, 0), 5);
    }

    #[test]
    fn find_compaction_fold_count_empty_candidates_folds_nothing() {
        assert_eq!(find_compaction_fold_count(&[], 100), 0);
    }

    /// Guards the two tests below: both mutate the *same* process-wide
    /// env vars `compact_trigger_tokens`/`compact_keep_recent_tokens`
    /// read, so they can't run concurrently with each other (or with a
    /// stray reader) the way ordinary `#[test]`s otherwise would under
    /// `cargo test`'s default parallelism.
    static COMPACT_ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn compact_trigger_tokens_falls_back_to_settings_json_when_env_var_unset() {
        let _guard = COMPACT_ENV_GUARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::remove_var("RUSTY_PRIME_AGENT_COMPACT_TRIGGER_TOKENS");
        let root = temp_state_root("compact-trigger-settings");
        std::fs::write(
            root.join("settings.json"),
            r#"{"compact_trigger_tokens": 999}"#,
        )
        .unwrap();
        assert_eq!(compact_trigger_tokens(&root), 999);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn compact_trigger_tokens_env_var_wins_over_settings_json() {
        let _guard = COMPACT_ENV_GUARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::set_var("RUSTY_PRIME_AGENT_COMPACT_TRIGGER_TOKENS", "111");
        let root = temp_state_root("compact-trigger-env-wins");
        std::fs::write(
            root.join("settings.json"),
            r#"{"compact_trigger_tokens": 999}"#,
        )
        .unwrap();
        assert_eq!(compact_trigger_tokens(&root), 111);
        std::env::remove_var("RUSTY_PRIME_AGENT_COMPACT_TRIGGER_TOKENS");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn compact_keep_recent_tokens_falls_back_to_settings_json_when_env_var_unset() {
        let _guard = COMPACT_ENV_GUARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::remove_var("RUSTY_PRIME_AGENT_COMPACT_KEEP_RECENT_TOKENS");
        let root = temp_state_root("compact-keep-settings");
        std::fs::write(
            root.join("settings.json"),
            r#"{"compact_keep_recent_tokens": 42}"#,
        )
        .unwrap();
        assert_eq!(compact_keep_recent_tokens(&root), 42);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn compact_tokens_with_no_settings_json_and_no_env_var_use_the_hardcoded_default() {
        let _guard = COMPACT_ENV_GUARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::remove_var("RUSTY_PRIME_AGENT_COMPACT_TRIGGER_TOKENS");
        std::env::remove_var("RUSTY_PRIME_AGENT_COMPACT_KEEP_RECENT_TOKENS");
        let root = temp_state_root("compact-tokens-defaults");
        assert_eq!(
            compact_trigger_tokens(&root),
            DEFAULT_COMPACT_TRIGGER_TOKENS
        );
        assert_eq!(
            compact_keep_recent_tokens(&root),
            DEFAULT_COMPACT_KEEP_RECENT_TOKENS
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[rusty_tokio::test]
    async fn snapshot_for_fork_truncates_the_transcript_at_the_given_sequence() {
        let root = temp_state_root("fork-snapshot-truncate");
        let mut session = AgentSession::create(
            &root,
            "source".to_string(),
            NewSessionMeta::default(),
            Box::new(crate::provider::EchoProvider),
            Box::new(crate::tool_runtime::NoopToolRuntime),
        )
        .await
        .unwrap();
        session.prompt("first".to_string()).await.unwrap();
        session.prompt("second".to_string()).await.unwrap();

        let session_dir = paths::session_dir(&root, "source");
        let (state, entries) = snapshot_for_fork(&session_dir, Some(2)).unwrap();
        assert_eq!(state.session_id, "source");
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.sequence <= 2));

        let (_, all_entries) = snapshot_for_fork(&session_dir, None).unwrap();
        assert_eq!(all_entries.len(), 4);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[rusty_tokio::test]
    async fn snapshot_for_fork_rejects_a_sequence_past_the_real_end() {
        let root = temp_state_root("fork-snapshot-past-end");
        let mut session = AgentSession::create(
            &root,
            "source".to_string(),
            NewSessionMeta::default(),
            Box::new(crate::provider::EchoProvider),
            Box::new(crate::tool_runtime::NoopToolRuntime),
        )
        .await
        .unwrap();
        session.prompt("only turn".to_string()).await.unwrap();

        let session_dir = paths::session_dir(&root, "source");
        let err = snapshot_for_fork(&session_dir, Some(999)).unwrap_err();
        assert!(err.to_string().contains("999"), "got: {err}");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[rusty_tokio::test]
    async fn seed_forked_session_produces_a_session_recover_can_replay() {
        let root = temp_state_root("fork-seed-recover");
        let mut source = AgentSession::create(
            &root,
            "source".to_string(),
            NewSessionMeta {
                model: Some("ollama/qwen2.5:0.5b".to_string()),
                thinking: Some("low".to_string()),
                ..NewSessionMeta::default()
            },
            Box::new(crate::provider::EchoProvider),
            Box::new(crate::tool_runtime::NoopToolRuntime),
        )
        .await
        .unwrap();
        source.prompt("hello".to_string()).await.unwrap();

        let source_dir = paths::session_dir(&root, "source");
        let (source_state, entries) = snapshot_for_fork(&source_dir, None).unwrap();
        seed_forked_session(
            &root,
            "forked",
            Some("my fork".to_string()),
            &source_state,
            entries,
            ForkedFrom {
                session_id: "source".to_string(),
                at_sequence: 2,
            },
        )
        .await
        .unwrap();

        let recovered = AgentSession::recover(
            &root,
            "forked",
            Box::new(crate::provider::EchoProvider),
            Box::new(crate::tool_runtime::NoopToolRuntime),
        )
        .await
        .unwrap();
        assert_eq!(recovered.transcript.len(), 2);
        assert_eq!(recovered.state.name.as_deref(), Some("my fork"));
        // Configuration carries forward from the source...
        assert_eq!(
            recovered.state.model.as_deref(),
            Some("ollama/qwen2.5:0.5b")
        );
        assert_eq!(recovered.state.thinking.as_deref(), Some("low"));
        // ...but narrative state deliberately does not, see `ForkedFrom`'s
        // own doc comment.
        assert!(recovered.state.goal.is_none());
        assert_eq!(
            recovered
                .state
                .forked_from
                .as_ref()
                .map(|f| f.session_id.as_str()),
            Some("source")
        );
        assert_eq!(
            recovered.state.forked_from.as_ref().map(|f| f.at_sequence),
            Some(2)
        );

        std::fs::remove_dir_all(&root).unwrap();
    }
}
