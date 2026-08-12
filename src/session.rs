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
    GoalAction, GoalState, GoalStatus, HarnessAction, HarnessNote, HarnessSnapshot, HarnessState,
    Request, Role, ScheduleKind, SessionEvent, SessionState, SessionStatus, ToolCallRequest,
    TranscriptEntry,
};
use crate::provider::{ChatTurn, ModelProvider, ProviderReply, ToolDef, TurnRole};
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
}

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
        self.append(Role::User, text, None, None, None).await?;
        let tools = self.enabled_tool_defs().await?;

        const MAX_TOOL_ROUNDS: usize = 8;
        for _ in 0..MAX_TOOL_ROUNDS {
            let turns = self.build_turns();
            match self.provider.respond(&turns, &tools).await? {
                ProviderReply::Text(reply) => {
                    return self.append(Role::Assistant, reply, None, None, None).await;
                }
                ProviderReply::ToolCalls(calls) => {
                    self.append(
                        Role::Assistant,
                        String::new(),
                        Some(calls.clone()),
                        None,
                        None,
                    )
                    .await?;
                    for call in calls {
                        let result = self.execute_tool_call(&call.name, &call.arguments).await?;
                        self.append(Role::Tool, result, None, Some(call.id), Some(call.name))
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
    async fn execute_python_tool_call(&mut self, arguments: &str) -> Result<String> {
        let value: serde_json::Value = match serde_json::from_str(arguments) {
            Ok(v) => v,
            Err(e) => return Ok(format!("error: invalid arguments JSON: {e}")),
        };
        let code = match value["code"].as_str() {
            Some(c) => c.to_string(),
            None => return Ok("error: missing required `code` argument".to_string()),
        };
        let outcome = self.tool_runtime.execute(&code).await?;
        let heartbeat_requested = outcome.stdout.contains(HEARTBEAT_MARKER);
        let mut text = if heartbeat_requested {
            outcome.stdout.replace(HEARTBEAT_MARKER, "")
        } else {
            outcome.stdout
        };
        if let Some(result) = outcome.result {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&result);
        }
        if heartbeat_requested {
            let status = self.trigger_heartbeat().await?;
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

    /// Handles a kernel-side `rlm_heartbeat()` call -- parity with
    /// `prime-agent`'s manual re-entry trigger, see `HEARTBEAT_MARKER`'s
    /// own doc comment. With no `Active` goal, explains and schedules
    /// nothing (same precondition `session_autonomous` itself has for
    /// its own continuation prompts). Otherwise, connects to this
    /// process's own daemon -- an ordinary client connection to
    /// `daemon.sock`, the same `transport`/`Request`/`Response`
    /// primitives every `client.rs` function already uses -- and asks it
    /// to fire a one-shot continuation prompt (`Request::ScheduleAdd`,
    /// `ScheduleKind::Once { at_ms: now_ms() }`, the exact "near-
    /// immediate one-shot" pattern `client::session_spawn` already
    /// established) rather than calling `self.prompt()` directly (which
    /// would recurse into this same in-flight `prompt()` call and
    /// interleave transcript entries out of order) or writing
    /// `schedules.json` directly (`schedule.rs`'s own doc comment: it has
    /// exactly one safe writer, the daemon's own background firing loop
    /// -- a second, unsynchronized writer racing that loop's own
    /// read-modify-write could lose or resurrect entries). The response
    /// read is best-effort: a request that's already been written and
    /// accepted by the daemon has very likely already taken effect even
    /// if this connection doesn't get to read the ack back.
    async fn trigger_heartbeat(&self) -> Result<String> {
        let Some(goal) = &self.state.goal else {
            return Ok("(heartbeat ignored: no active goal set)".to_string());
        };
        if goal.status != GoalStatus::Active {
            return Ok("(heartbeat ignored: goal is not active)".to_string());
        }
        let text = format!("Continue working toward the goal: {}", goal.text);

        let socket_path = paths::daemon_socket_path(&self.state_root);
        let mut conn = transport::connect(Context::Daemon, socket_path).await?;
        conn.write_request(
            Context::Daemon,
            &Request::ScheduleAdd {
                session_id: self.state.session_id.clone(),
                text,
                kind: ScheduleKind::Once { at_ms: now_ms() },
            },
        )
        .await?;
        let _ =
            rusty_tokio::time::timeout(Duration::from_secs(5), conn.read_response(Context::Daemon))
                .await;
        Ok("(heartbeat scheduled: will continue toward the goal shortly)".to_string())
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
    fn build_turns(&self) -> Vec<ChatTurn> {
        self.transcript
            .iter()
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
            })
            .collect()
    }

    async fn append(
        &mut self,
        role: Role,
        text: String,
        tool_calls: Option<Vec<ToolCallRequest>>,
        tool_call_id: Option<String>,
        name: Option<String>,
    ) -> Result<TranscriptEntry> {
        let entry = TranscriptEntry {
            sequence: self.state.last_sequence + 1,
            timestamp_ms: now_ms(),
            role,
            text,
            tool_calls,
            tool_call_id,
            name,
        };
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
