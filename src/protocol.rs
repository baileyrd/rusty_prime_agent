//! Wire protocol: JSONL-framed request/response, shared by both the
//! public (client <-> supervisor) and private (supervisor <-> worker)
//! transports (see `crate::transport`).
//!
//! Framing contract, deliberately simple for Phase 1 (see
//! `ARCHITECTURE.md` "IPC Model"): each connection carries exactly one
//! [`Request`] line, then either exactly one [`Response`] line
//! (non-streaming requests, connection then closes) or one [`Response`]
//! line followed by zero or more [`SessionEvent`] lines until the stream
//! ends (streaming requests -- currently only `SessionAttach`). There is
//! no bidirectional multiplexing on one connection: a client that wants
//! to both attach and issue another command opens a second connection.
//! This sidesteps needing a duplicate/non-blocking read+write split on
//! `platform::net`'s object-safe `&mut self` stream traits, which expose
//! no `try_clone`.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Valid on both transports (public and private): a bare
    /// liveness/readiness probe with no side effects, answered with
    /// [`Response::Pong`]. A successful `connect()` alone is not proof
    /// anything is actually being served on the other end -- a listen
    /// endpoint can accept a connection into its backlog for a brief
    /// window even after its owning process is gone (observed in this
    /// project's own `tests/supervisor_restart_recovery.rs`: a freshly
    /// force-killed supervisor's stale socket could still complete a
    /// client `connect()`, with nothing left to ever answer it) --
    /// `Ping`/`Pong` is what actually distinguishes "reachable" from
    /// "genuinely serving requests", used by both `daemon_start`'s
    /// idempotency check and every `wait_ready` retry loop.
    Ping,
    DaemonStatus,
    DaemonShutdown,
    SessionNew {
        /// Optional human-readable label; the session id itself is always
        /// generated server-side.
        name: Option<String>,
        /// Parity with `prime-agent --model provider/id`: a
        /// `"provider/model"` string (e.g. `"ollama/qwen2.5:0.5b"`,
        /// `"anthropic/claude-sonnet-5"`) selecting a real backend routed
        /// through `rusty_provider`'s `rp-server` (see
        /// `provider::RustyProviderModel`/`rp_server`). `None` keeps
        /// `EchoProvider`, still the default. Fixed for this session's
        /// whole lifetime once set -- recorded in `SessionState::model`
        /// so a respawned worker (resume/recover) reconstructs the same
        /// backend without the caller having to repeat it.
        model: Option<String>,
        /// Parity with `prime-agent --goal`: seeds this session's
        /// persistent goal at creation time. `None` means no goal yet --
        /// `Request::GoalUpdate`'s `Set` action is how a goal gets added
        /// to (or replaced on) an already-existing session.
        goal: Option<String>,
        /// Parity with `prime-agent`'s recursive subagents (`rlm(...)`,
        /// `receiver_role="parent"/"child"`): set only by `client::
        /// session_spawn`'s own composition (`session spawn`), never by
        /// an ordinary `session new` -- see that command's own doc
        /// comment for why a plain top-level session has no parent to
        /// name. Fixed for this session's whole lifetime, same as
        /// `model`/`goal`.
        parent_id: Option<String>,
        /// Parity with `prime-agent --thinking <level>`: requests a
        /// visible reasoning/thinking trace from `model`, when set
        /// (`rp-server`'s `ChatRequest.reasoning.effort` -- see
        /// `provider::RustyProviderModel`). One of `"low"`/`"medium"`/
        /// `"high"`, `rp-server`'s own `effort` vocabulary. `None` (the
        /// default) means no reasoning requested; irrelevant for
        /// `EchoProvider` sessions. Fixed for this session's whole
        /// lifetime, same as `model`/`goal`.
        thinking: Option<String>,
        /// Opt-in built-in tool set offered to `model` on every prompt
        /// (`session new --tools read`), parity with `prime-agent`'s own
        /// tool-calling loop -- see `session::AgentSession::prompt`'s own
        /// doc comment for the round-trip this drives. `Some("read")` is
        /// the only value accepted today (`tools::read_only_tool_defs`);
        /// `None` (the default) offers no tools at all, leaving every
        /// existing session's behavior completely unaffected. Fixed for
        /// this session's whole lifetime, same as `model`/`thinking`.
        tools: Option<String>,
        /// Parity with `prime-agent`'s RLM programming model: selects
        /// `tool_runtime::ToolRuntime`'s real backend for this session
        /// (`session new --runtime ipython`), a real IPython kernel
        /// subprocess this session's own turns can run code against --
        /// see `ipython_runtime`. `None` (the default) keeps
        /// `NoopToolRuntime`, leaving every existing session's behavior
        /// unaffected. Deliberately a separate concept from `tools`
        /// (`ToolRuntime` is the model-facing *code execution
        /// environment* boundary; `tools` is the OpenAI-style
        /// tool-calling loop against `rp-server`) -- see
        /// `ARCHITECTURE.md`'s "ToolRuntime Trait Boundary" section for
        /// why the two are kept distinct rather than merged into one
        /// flag. Like `thinking` (not `tools`/`goal`/`parent_id`), always
        /// supplied by the daemon at worker-spawn time, since
        /// `worker::run` must pick a `ToolRuntime` implementation before
        /// `AgentSession::create`/`recover` even exist to read
        /// `state.tools` back from.
        runtime: Option<String>,
    },
    SessionAttach {
        session_id: String,
    },
    SessionList,
    /// Not in the brief's minimal CLI surface by name, but required to
    /// exercise the fake echo provider end to end (Non-Goals: "stub with
    /// a fake provider that echoes turns") and to give session recovery
    /// tests real transcript content to recover.
    SessionPrompt {
        session_id: String,
        text: String,
    },
    /// Parity with `prime-agent stop <agent>`: gracefully shut down one
    /// session's worker without touching any other session or the
    /// daemon itself. Idempotent -- stopping a session that is already
    /// `Stopped` or `Crashed` (no live worker to shut down) still
    /// succeeds, since the end state ("no worker running for this
    /// session") already holds.
    SessionStop {
        session_id: String,
    },
    /// Parity with `prime-agent rename <agent> <name>`. Valid on both
    /// transports: the public request is forwarded to the owning
    /// worker's private connection unchanged, the same way
    /// `SessionPrompt` is -- `name: None` clears a session's display
    /// name back to unnamed.
    SessionRename {
        session_id: String,
        name: Option<String>,
    },
    /// Parity with `prime-agent schedule add`: registers a prompt the
    /// daemon itself will inject into `session_id` later, without any
    /// client attached -- see `schedule`'s own module doc comment for
    /// the firing loop. Public transport only; the daemon fires a
    /// schedule the same way it relays an ordinary client
    /// `SessionPrompt`, not by sending this request to the worker.
    ScheduleAdd {
        session_id: String,
        text: String,
        kind: ScheduleKind,
    },
    /// Parity with `prime-agent schedule list`.
    ScheduleList {
        session_id: String,
    },
    /// Parity with `prime-agent schedule cancel`.
    ScheduleCancel {
        session_id: String,
        schedule_id: String,
    },
    /// Parity with `prime-agent --goal`/`/goal`: mutates a session's
    /// persistent goal. Valid on both transports, forwarded to the
    /// owning worker unchanged, the same way `SessionRename` is -- a
    /// goal is part of a session's mutable state, and the worker is that
    /// state's one owner while it's running (see `SessionRename`'s own
    /// doc comment for why a direct daemon-side write would race).
    GoalUpdate {
        session_id: String,
        action: GoalAction,
    },
    /// Parity with `/goal`'s status display. Read-only, so answered
    /// directly from the persisted `state.json` rather than round-
    /// tripping through the worker -- the same reasoning `SessionList`
    /// itself already uses.
    GoalShow {
        session_id: String,
    },
    /// Parity with `prime-agent`'s Continual Harness (`/refine`): mutates
    /// a session's durable supplemental harness state (`HarnessState`).
    /// Valid on both transports, forwarded to the owning worker
    /// unchanged, same reasoning as `GoalUpdate`. `session_refine`
    /// (client-side orchestration, see `client.rs`) is the one caller
    /// that issues `Add` on its own behalf, on the model's proposal
    /// rather than a user's -- there's no separate wire-level "refine"
    /// request, since composing existing requests was enough.
    HarnessUpdate {
        session_id: String,
        action: HarnessAction,
    },
    /// Read-only, answered directly from `state.json`, same reasoning as
    /// `GoalShow`.
    HarnessShow {
        session_id: String,
    },
    /// Private transport only: supervisor -> worker, asking it to persist
    /// its final state and exit cleanly (used by `daemon shutdown` and
    /// `SessionStop`).
    WorkerShutdown,
}

/// When a [`ScheduleEntry`] fires. Parity with `prime-agent schedule
/// add`'s one-shot-vs-recurring split (that command's own `--at`/
/// `--every`-shaped options, simplified to the two cases this project's
/// firing loop actually needs to distinguish).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleKind {
    /// Fires once, at `at_ms`, then removes itself.
    Once { at_ms: u64 },
    /// Fires every `interval_ms`, indefinitely, until canceled.
    Every { interval_ms: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Pong,
    Error {
        message: String,
        /// True for an expected, structured condition (e.g.
        /// `session_already_active`) the client should present directly
        /// rather than treat as a bug.
        conflict: bool,
    },
    DaemonStatus {
        protocol_version: u32,
        pid: u32,
        generation: u64,
        sessions_active: usize,
    },
    DaemonShutdownAck,
    SessionNew {
        session_id: String,
    },
    SessionList {
        sessions: Vec<SessionSummary>,
    },
    SessionPromptAck {
        entry: TranscriptEntry,
    },
    /// Sent immediately, before any [`SessionEvent`] line, so the client
    /// can distinguish "attach accepted, snapshot incoming" from a
    /// terminal [`Response::Error`].
    SessionAttachStarted {
        session_id: String,
    },
    /// `already_stopped` is true when there was no live worker to shut
    /// down in the first place (already `Stopped`/`Crashed`) -- still a
    /// success, but lets the CLI print an accurate message instead of
    /// implying a worker was just torn down.
    SessionStopAck {
        already_stopped: bool,
    },
    SessionRenameAck {
        name: Option<String>,
    },
    ScheduleAdded {
        schedule_id: String,
    },
    ScheduleList {
        entries: Vec<ScheduleEntry>,
    },
    /// `found` is false when `schedule_id` didn't match anything for
    /// that session -- still not an error (canceling something already
    /// gone/fired achieves the caller's actual goal), just reported
    /// accurately rather than claimed as a real cancellation.
    ScheduleCancelAck {
        found: bool,
    },
    GoalUpdateAck {
        goal: Option<GoalState>,
    },
    GoalShow {
        goal: Option<GoalState>,
    },
    HarnessUpdateAck {
        state: HarnessState,
    },
    HarnessShow {
        state: HarnessState,
    },
    WorkerShutdownAck,
}

/// Parity with `prime-agent --goal`/`/goal`. `Clear` removes the goal
/// entirely (back to `None`); `Pause`/`Resume`/`Complete` are no-ops
/// (structurally valid, but leave the goal untouched) when there is no
/// current goal to transition -- see `session::AgentSession::update_goal`'s
/// own doc comment for why that's a deliberate choice, not an oversight.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum GoalAction {
    /// Replaces any existing goal with a fresh `Active` one.
    Set {
        text: String,
    },
    Pause,
    Resume,
    Complete,
    Clear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Completed,
}

/// A session's persistent goal (`SessionState::goal`) -- parity with
/// `prime-agent`'s "keeps an objective and its progress active across
/// turns until it is completed, paused, or cleared." Bounded-mode
/// autonomous continuation itself (`--autonomous*`) is a separate,
/// out-of-scope concern (`PARITY.md`) -- this type is just the durable
/// state a future continuation policy would read, not that policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalState {
    pub text: String,
    pub status: GoalStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Parity with `prime-agent`'s Continual Harness paper abstraction
/// (`arxiv.org/abs/2605.09998`): "stores supplemental prompts, memories,
/// skill descriptions, and reusable subagent specifications." Subagent
/// specifications are left out -- they're tied to recursive subagents
/// (`PARITY.md`), a separate out-of-scope concern -- so this covers the
/// first three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessNoteKind {
    Prompt,
    Memory,
    SkillDescription,
}

/// One entry of durable supplemental harness state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessNote {
    pub id: String,
    pub kind: HarnessNoteKind,
    pub text: String,
    pub added_at_ms: u64,
}

/// A recorded version of `HarnessState::notes` -- parity with
/// `prime-agent`'s "recorded refinement history." Every successful
/// `HarnessAction` (`Add` or `Rollback` alike) appends one of these, so
/// `history.last()` always mirrors the current `notes` and a rollback
/// itself becomes part of the auditable trail rather than erasing any of
/// it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessSnapshot {
    pub notes: Vec<HarnessNote>,
    pub recorded_at_ms: u64,
    /// A short human-readable description of what produced this
    /// snapshot (e.g. `"add memory note"`, `"refine: <preview>"`,
    /// `"rollback to history[2]"`) -- `session harness list`'s own
    /// history display, not machine-parsed.
    pub reason: String,
}

/// A session's durable Continual Harness state (`SessionState::harness`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HarnessState {
    #[serde(default)]
    pub notes: Vec<HarnessNote>,
    #[serde(default)]
    pub history: Vec<HarnessSnapshot>,
}

/// Parity with `/refine`'s own two operations: applying a small update,
/// and (since every update is recorded) reverting to an earlier one.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HarnessAction {
    Add {
        kind: HarnessNoteKind,
        text: String,
    },
    /// `index` into `HarnessState::history` (0-based, oldest first) --
    /// restores `notes` to exactly that recorded version.
    Rollback {
        index: usize,
    },
}

/// One line of the attach event stream, after `SessionAttachStarted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    /// The durable recovery baseline (daemon.md: "the attach snapshot is
    /// the durable recovery baseline"), sent once, first: the full
    /// transcript replayed from disk plus the current session state.
    Snapshot {
        // Boxed: `SessionState` (now carrying `goal`/`harness`/
        // `parent_id` on top of its original fields) makes this variant
        // more than 5x the size of `SessionEvent`'s next-largest one
        // (`Turn`), which clippy's `large_enum_variant` flags -- every
        // `SessionEvent` on the wire pays that difference whether it's a
        // `Snapshot` or not, since an enum's stack size is its largest
        // variant's.
        state: Box<SessionState>,
        transcript: Vec<TranscriptEntry>,
    },
    /// One new transcript entry appended after the snapshot was taken.
    Turn { entry: TranscriptEntry },
    /// A visible marker appended after recovering from a worker crash
    /// (daemon.md: "appends a visible recovery marker to the
    /// transcript").
    RecoveryMarker { message: String, at_ms: u64 },
    /// The worker reached a terminal state (shut down cleanly); the
    /// stream ends after this.
    SessionEnded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
    /// A tool's result, sent back to the model as its own turn -- see
    /// `session::AgentSession::prompt`'s tool-calling loop.
    Tool,
}

/// One tool call the model requested, in `rp-server`'s own OpenAI-shaped
/// wire format: `arguments` is the raw, model-generated JSON-arguments
/// string, passed through to `tools::execute` unparsed rather than
/// validated against `provider::ToolDef::parameters`'s schema here --
/// same "skip JSON-Schema validation, let the callee reject malformed
/// input" reasoning as this project's planned MCP client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub role: Role,
    pub text: String,
    /// Set only on a `Role::Assistant` entry that's a tool-call request
    /// (`text` is empty in that case) -- `#[serde(default)]` so
    /// `transcript.jsonl` files written before Increment 3 still parse.
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallRequest>>,
    /// Set only on a `Role::Tool` entry: which `ToolCallRequest::id` this
    /// is the result of. `#[serde(default)]` for the same
    /// pre-existing-transcript reason as `tool_calls`.
    #[serde(default)]
    pub tool_call_id: Option<String>,
    /// Set only on a `Role::Tool` entry: the tool's name, mirrored onto
    /// the entry so a transcript reader doesn't have to cross-reference
    /// the matching `tool_calls` entry to know which tool ran.
    /// `#[serde(default)]` for the same reason.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// A worker process is (believed to be) running for this session.
    Active,
    /// The worker exited cleanly (`WorkerShutdown`/`SessionEnded`).
    Stopped,
    /// The last known worker pid is no longer alive and no clean
    /// shutdown was recorded -- recovered on next attach.
    Crashed,
}

/// The small, fast-to-read recovery-pointer file (`state.json`) that
/// sits beside each session's transcript. See `ARCHITECTURE.md`
/// "Session State File Format" for why this is a pointer file plus a
/// full-replay transcript, not a periodic snapshot + tail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: String,
    pub name: Option<String>,
    pub status: SessionStatus,
    pub worker_pid: Option<u32>,
    /// Bumped every time a new worker process takes ownership of this
    /// session id (fresh spawn or crash-recovery respawn). Event cursors
    /// in the attach stream are `(generation, sequence)` pairs, mirroring
    /// the prime-agent reference architecture this project follows.
    pub generation: u64,
    pub last_sequence: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// See `Request::SessionNew::model`'s own doc comment. `#[serde(default)]`
    /// so a `state.json` written before this field existed still parses
    /// (as `None`, i.e. `EchoProvider`) rather than failing recovery.
    #[serde(default)]
    pub model: Option<String>,
    /// See `GoalState`'s own doc comment. `#[serde(default)]` for the
    /// same pre-existing-`state.json` reason `model` has it.
    #[serde(default)]
    pub goal: Option<GoalState>,
    /// See `HarnessState`'s own doc comment. `#[serde(default)]` for the
    /// same pre-existing-`state.json` reason `model`/`goal` have it.
    #[serde(default)]
    pub harness: HarnessState,
    /// See `Request::SessionNew::parent_id`'s own doc comment.
    /// `#[serde(default)]` for the same pre-existing-`state.json` reason
    /// `model`/`goal`/`harness` have it.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// See `Request::SessionNew::thinking`'s own doc comment.
    /// `#[serde(default)]` for the same pre-existing-`state.json` reason
    /// `model`/`goal`/`harness`/`parent_id` have it.
    #[serde(default)]
    pub thinking: Option<String>,
    /// See `Request::SessionNew::tools`'s own doc comment.
    /// `#[serde(default)]` for the same pre-existing-`state.json` reason
    /// `model`/`goal`/`harness`/`parent_id`/`thinking` have it.
    #[serde(default)]
    pub tools: Option<String>,
    /// See `Request::SessionNew::runtime`'s own doc comment.
    /// `#[serde(default)]` for the same pre-existing-`state.json` reason
    /// `model`/`goal`/`harness`/`parent_id`/`thinking`/`tools` have it.
    #[serde(default)]
    pub runtime: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub name: Option<String>,
    pub status: SessionStatus,
    pub last_sequence: u64,
    pub updated_at_ms: u64,
    /// The recorded worker pid, as last written by whichever worker
    /// process currently (or most recently) owned this session --
    /// `None` only for a session whose `state.json` predates this field
    /// (never true for a session created by this project's own `session
    /// new`, which always records one). Parity with `prime-agent
    /// agents`/`list`, which surface each agent's worker process.
    pub worker_pid: Option<u32>,
    /// See `SessionState::generation`'s own doc comment.
    pub generation: u64,
    /// See `Request::SessionNew::model`'s own doc comment. `None` means
    /// this session uses `EchoProvider`.
    pub model: Option<String>,
    /// See `GoalState`'s own doc comment.
    pub goal: Option<GoalState>,
    /// See `Request::SessionNew::parent_id`'s own doc comment. `session
    /// children <id>`/`session spawn`'s model-inheritance both read this
    /// straight off `session list` rather than needing a dedicated
    /// request.
    pub parent_id: Option<String>,
    /// See `Request::SessionNew::thinking`'s own doc comment.
    pub thinking: Option<String>,
    /// See `Request::SessionNew::tools`'s own doc comment.
    pub tools: Option<String>,
    /// See `Request::SessionNew::runtime`'s own doc comment.
    pub runtime: Option<String>,
}

/// One registered schedule entry, persisted per-session (see `schedule`'s
/// own module doc comment) and returned by `ScheduleList`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleEntry {
    pub schedule_id: String,
    pub text: String,
    pub kind: ScheduleKind,
    /// When this entry is next due -- for `Once`, equal to `kind`'s own
    /// `at_ms`; for `Every`, advances by `interval_ms` each time it
    /// fires, skipping forward past `now` if the daemon was down for a
    /// while rather than firing a burst of catch-up prompts.
    pub next_fire_ms: u64,
    pub created_at_ms: u64,
}
