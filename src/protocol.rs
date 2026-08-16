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
    /// **Private transport only.** The mandatory preamble on every
    /// supervisor -> worker connection that isn't a bare [`Request::Ping`]:
    /// the worker serves nothing else until it has seen this and matched
    /// `supervisor` exactly against its own current fence. See
    /// `crate::fence` for the whole mechanism and why it exists.
    ///
    /// **Acceptance is silent.** There is no positive acknowledgement to
    /// wait for -- the supervisor writes this line and its real request
    /// back to back, keeping the exchange at one round trip (see
    /// `daemon::Supervisor::connect_worker` for the measurement behind
    /// that). A rejection arrives as a `conflict: true`
    /// [`Response::Error`] naming both identities, in place of whatever
    /// reply the following request would have produced.
    ///
    /// Deliberately *not* accepted on the public transport: a client has
    /// no business presenting a supervisor identity, and `daemon::
    /// Supervisor::handle_public_connection` rejects it the same way it
    /// rejects any other private-only variant.
    WorkerAuth {
        supervisor: crate::fence::SupervisorIdentity,
    },
    /// **Private transport only.** A replacement supervisor taking over a
    /// still-live worker it did not itself spawn (`daemon::Supervisor::
    /// recover_on_startup` -> `adopt_worker`). Unlike [`Request::WorkerAuth`]
    /// this *advances* the fence, so it costs two things: presenting the
    /// worker's own `worker_token` (read off the owner-only fence file,
    /// which is what proves the caller is at least the same OS user with
    /// access to this state root), and a supervisor identity whose
    /// counter is strictly greater than the fence's current one (which is
    /// what stops a *stale* supervisor -- who also has the token -- from
    /// simply taking the worker back). Answered with
    /// [`Response::WorkerAdopted`].
    WorkerAdopt {
        worker_token: String,
        supervisor: crate::fence::SupervisorIdentity,
    },
    DaemonStatus,
    /// `force: false` (the default, `daemon shutdown` with no flag) is
    /// the original behavior unchanged: send `Request::WorkerShutdown`
    /// to every `Active` session's worker and wait for each ack before
    /// tearing down the daemon's own sockets. `force: true` (`daemon
    /// shutdown --force`) skips that round trip entirely -- useful when
    /// a worker has wedged and its ack would otherwise hang the whole
    /// shutdown. Skipped workers are not killed, just not waited on:
    /// they keep running headless, exactly the same "supervisor gone,
    /// worker still alive" state this project's own crash recovery
    /// (`is_worker_alive`/`resolve_worker`) already has to and does
    /// handle for an actual crash, so a forced shutdown leaves nothing
    /// in a state the rest of the daemon can't already cope with.
    DaemonShutdown {
        force: bool,
    },
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
        /// Set only by `session::AgentSession::handle_rlm_run` (a
        /// kernel-callable `rlm(...)` admission), never by `client::
        /// session_spawn`/`session_new`/a fork: see `protocol::
        /// SessionState::spawned_from_sequence`'s own doc comment for
        /// what this is and why it has to travel over the wire rather
        /// than being resolved server-side the way `rlm_depth`/
        /// `rlm_max_depth` are.
        spawned_from_sequence: Option<u64>,
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
        /// Parity with a bounded slice of `prime-agent`'s image-paste
        /// feature -- see `TranscriptEntry::images`'s own doc comment.
        /// `None`/empty for every existing caller (the ordinary
        /// text-only prompt path); populated only by the REPL's own
        /// `/file`/`@`-reference image handling and `session prompt
        /// --image <path>`.
        images: Option<Vec<String>>,
        /// Bounded first slice of idempotent replay protection
        /// (`CLAIMS_AUDIT.md`'s own "Idempotent replay protection for
        /// in-flight requests" entry): a caller-supplied id opting this
        /// one prompt into dedup. `None` (every existing caller except
        /// `session prompt --request-id <id>`) means no protection at
        /// all, the same as before this field existed -- a client that
        /// never retries doesn't need it. When `Some`, the *worker*
        /// (`AgentSession::prompt_with_images_and_request_id`) keeps a
        /// small in-memory (not durable -- lost on worker crash/restart,
        /// a separately larger step) cache of recently-seen ids; a
        /// second `SessionPrompt` carrying an id already in that cache
        /// returns the exact same `TranscriptEntry` again instead of
        /// enqueuing a second prompt -- what a caller retrying after a
        /// timed-out/dropped connection needs to avoid double-sending,
        /// without a `daemon.md`-style durable `clientId + commandId`
        /// journal.
        #[serde(default)]
        request_id: Option<String>,
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
    /// Parity with `prime-agent /compact [instructions]`: force
    /// compaction now, regardless of whether the automatic token
    /// threshold (`session::AgentSession::maybe_compact`) has been
    /// crossed. Valid on both transports, forwarded to the owning worker
    /// unchanged, same reasoning as `SessionRename`. `instructions`
    /// mirrors `prime-agent`'s own optional focus text (e.g. "focus on
    /// the auth refactor, remember the exact migration command"), folded
    /// into the summarization prompt when given. A no-op (not an error)
    /// on a session with no `model` set (`EchoProvider` has nothing to
    /// summarize with) or with nothing old enough to fold away yet --
    /// see `Response::SessionCompactAck::compacted`.
    SessionCompact {
        session_id: String,
        instructions: Option<String>,
    },
    /// Bounded parity with a slice of `prime-agent`'s "steering" --
    /// see `PARITY.md`'s own "cancel primitive" entry for the full
    /// design and what this deliberately can't do (abort a model call
    /// already in flight to a real provider's HTTP endpoint; that would
    /// need cooperative cancellation deep in `ModelProvider::respond`
    /// itself, out of scope here). Sets a flag
    /// (`session::AgentSession::cancel_flag`) the worker's own
    /// tool-calling loop checks between rounds -- a multi-round session
    /// (RLM/MCP tool use) genuinely stops early at its next safe
    /// checkpoint instead of continuing to a natural finish or the
    /// `MAX_TOOL_ROUNDS` cap; a single-round session (plain text reply,
    /// `EchoProvider`) has no round boundary to check at, so this is
    /// effectively a no-op for it. Valid on both transports, forwarded
    /// to the owning worker unchanged, same reasoning as
    /// `SessionCompact` -- but handled by the worker *without* taking
    /// the session's own lock (unlike every sibling request here), since
    /// an in-flight prompt already holds that lock for its whole
    /// duration; waiting for it would defeat the entire point.
    SessionInterrupt {
        session_id: String,
    },
    /// Invokes an extension-registered command (`pi.register_command`,
    /// bounded parity with a slice of `prime-agent`'s extension system --
    /// see `extensions.rs`'s own module doc comment). Valid on both
    /// transports, forwarded to the owning worker unchanged, same
    /// reasoning as `SessionRename`/`SessionCompact`. Not a top-level
    /// CLI subcommand -- only `client::session_repl`'s own fallback (a
    /// `/foo args...` line that matched none of the built-in slash
    /// commands) ever sends this, since extension commands only exist
    /// once a running kernel has actually registered them.
    SessionExtensionCommand {
        session_id: String,
        command: String,
        args: String,
    },
    /// Parity with `session-format.md`'s active-leaf concept: redirects
    /// `session_id`'s own `SessionState::active_leaf_sequence` to
    /// `sequence` -- the entry the *next* append continues from. Valid on
    /// both transports, forwarded to the owning worker unchanged, same
    /// reasoning as `SessionRename`/`SessionCompact`. Rejects a `sequence`
    /// that doesn't name a real entry in this session's own transcript
    /// (`session::AgentSession::set_active_leaf`'s own doc comment). Not
    /// itself a transcript entry or a client-facing CLI/REPL command yet
    /// -- see `PARITY.md`'s intra-session-branching entry for what this
    /// increment covers versus `/tree` navigation, a later one.
    SessionSetActiveLeaf {
        session_id: String,
        sequence: u64,
    },
    /// Parity with `session-format.md`'s `BranchSummaryEntry` -- see
    /// [`BranchSummary`]'s own doc comment and `session::AgentSession::
    /// branch_summarize`. Valid on both transports, forwarded to the
    /// owning worker unchanged, same reasoning as `SessionSetActiveLeaf`.
    /// `branch_leaf_sequence` names the branch to summarize by its own
    /// tip; a sequence that doesn't name a real transcript entry is a
    /// conflict, one that's already part of the currently active chain
    /// is a no-op (see `Response::SessionBranchSummarizeAck::summarized`).
    SessionBranchSummarize {
        session_id: String,
        branch_leaf_sequence: u64,
    },
    /// `session fork <id> [--at N]` -- bounded parity with a slice of
    /// `prime-agent`'s `/fork` (see `PARITY.md`'s "Tree-structured
    /// session data model" entry for exactly what's NOT attempted:
    /// `/clone`). Public transport only, unlike
    /// `SessionRename`/
    /// `SessionCompact` -- this creates a brand-new, independent session
    /// (its own directory, own worker) rather than mutating
    /// `session_id`'s, so there's no "owning worker" to forward it to;
    /// the daemon handles it directly, the same way `SessionNew` does.
    /// The new session's starting transcript is a copy of `session_id`'s
    /// own transcript up through `at_sequence` (or the whole thing, if
    /// `None`) -- session-level forking reusing this project's existing
    /// session-creation machinery, not intra-session branching. See
    /// `ForkedFrom`'s own doc comment for what does and doesn't carry
    /// forward onto the new session.
    SessionFork {
        session_id: String,
        at_sequence: Option<u64>,
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
    /// Private transport only: supervisor -> a *parent's* own worker,
    /// asking it to fold `child_id`'s usage into the parent turn that
    /// admitted it (`session::AgentSession::attribute_child_usage`).
    /// Sent by `daemon::Supervisor`'s own background poll (mirroring
    /// `fire_due_schedules`'s cadence) once `child_id`'s worker has
    /// stopped -- see `PARITY.md`'s child-usage-attribution entry for the
    /// full mechanism, including why this is the daemon *forwarding* a
    /// request rather than writing to the parent's `transcript.jsonl`
    /// itself. Idempotent: the parent's own handler checks its own
    /// transcript for an existing attribution of `child_id` first, so a
    /// redundant delivery (a retried poll, at-least-once semantics) is a
    /// safe no-op, not a duplicate entry.
    AttributeChildUsage {
        child_id: String,
    },
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
    /// The worker accepted a [`Request::WorkerAdopt`] and advanced its
    /// fence to the presented identity. `previous` is the identity that
    /// was displaced, purely so the adopting supervisor can log what it
    /// took over from -- `None` when the worker was *unfenced* until now
    /// (a session predating the fence mechanism, being converged onto one
    /// by an in-place upgrade), where there is genuinely no predecessor
    /// rather than a predecessor equal to the new owner.
    WorkerAdopted {
        previous: Option<crate::fence::SupervisorIdentity>,
    },
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
    /// `compacted` is false for the two no-op cases `SessionCompact`'s
    /// own doc comment describes (no `model`, or nothing old enough to
    /// fold away) -- still a success response, not `Response::Error`,
    /// the same "an accurate no-op beats a manufactured error" reasoning
    /// `ScheduleCancelAck::found` already uses. `summary` is the new
    /// (or unchanged, if `compacted` is false) running summary text, for
    /// a caller that wants to display it.
    SessionCompactAck {
        compacted: bool,
        summary: Option<String>,
    },
    /// A plain acknowledgment that the flag was set -- see
    /// `Request::SessionInterrupt`'s own doc comment for exactly what
    /// setting it can and can't stop. No `interrupted: bool` field:
    /// this worker deliberately never locks the session to check whether
    /// a prompt is actually in flight before acking (that would mean
    /// waiting behind the very thing it's trying to interrupt), so there
    /// is no truthful "yes, something was cancelled" fact available at
    /// ack time to report.
    SessionInterruptAck,
    /// `output` is a friendly "unknown extension command: /foo" message
    /// (not a `Response::Error`) when `command` names nothing
    /// registered -- the same "accurate no-op beats a manufactured
    /// error" reasoning `SessionCompactAck`/`ScheduleCancelAck::found`
    /// already use, since an unrecognized `/foo` typed in the REPL is
    /// normal, recoverable input.
    SessionExtensionCommandResult {
        output: String,
    },
    SessionSetActiveLeafAck {
        active_leaf_sequence: u64,
    },
    /// `summarized` is false for the no-op case
    /// `Request::SessionBranchSummarize`'s own doc comment describes
    /// (no model configured, or `branch_leaf_sequence` is already part
    /// of the active chain) -- same "accurate no-op beats a manufactured
    /// error" reasoning `SessionCompactAck`/`ScheduleCancelAck::found`
    /// already use.
    SessionBranchSummarizeAck {
        summarized: bool,
        summary: Option<String>,
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
    /// `attributed` is `false` for either idempotency reason
    /// `attribute_child_usage` can return early on (already attributed;
    /// `child_id` isn't actually this session's own child; `child_id` has
    /// no `spawned_from_sequence`, i.e. wasn't `rlm(...)`-admitted) --
    /// `daemon::Supervisor`'s poller logs but never treats `false` as an
    /// error, since every one of those is an expected, harmless outcome.
    AttributeChildUsageAck {
        attributed: bool,
    },
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
    /// Boxed for the same reason `Snapshot::state` is: `TranscriptEntry`
    /// (now carrying `images` on top of its original fields) tripped
    /// clippy's `large_enum_variant` against `SessionEvent`'s other,
    /// much smaller variants.
    Turn { entry: Box<TranscriptEntry> },
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

/// One model call's real token accounting, straight off `rp-server`'s
/// own OpenAI-shaped `/v1/chat/completions` response body (`usage:
/// {prompt_tokens, completion_tokens, total_tokens}` -- `rp-server`'s own
/// `core::types::Usage` has two further cache-token fields this project
/// has no use for and doesn't carry). `EchoProvider` never produces one
/// (there's no real model call to account for). See `provider::
/// parse_response` for where this gets read off the wire, and
/// `TranscriptEntry::usage`/`ChildUsageAttribution` for where it's
/// persisted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

impl std::ops::Add for Usage {
    type Output = Usage;
    fn add(self, other: Usage) -> Usage {
        Usage {
            prompt_tokens: self.prompt_tokens + other.prompt_tokens,
            completion_tokens: self.completion_tokens + other.completion_tokens,
            total_tokens: self.total_tokens + other.total_tokens,
        }
    }
}

/// Parity with `rlm-runtime.md`'s child-usage-attribution mechanism: "the
/// parent transcript persists a `child_usage_attributed` entry
/// containing: the target parent assistant message ID; the child usage
/// being attributed; and the resulting aggregate usage." This project
/// addresses transcript entries by `sequence`, not a separate message-id
/// concept, so `parent_message_sequence` is that document's "target
/// parent assistant message ID" -- specifically, the `Role::Assistant`
/// tool-call entry whose `execute_python` call invoked `rlm(...)` and
/// admitted this child (see `session::AgentSession::handle_rlm_run`).
/// `aggregate_usage` is the running total attributed to that same
/// `parent_message_sequence` across every child admitted from it so far
/// (there can be more than one, since one assistant turn's Python cell
/// may call `rlm(...)` more than once) -- see `session::AgentSession::
/// attribute_child_usage` for how it's computed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildUsageAttribution {
    pub child_session_id: String,
    pub parent_message_sequence: u64,
    pub child_usage: Usage,
    pub aggregate_usage: Usage,
}

/// Parity with `session-format.md`'s `BranchSummaryEntry` -- previously
/// tracked as depending on "the tree structure," which now exists (see
/// `TranscriptEntry::parent_sequence`/`SessionState::
/// active_leaf_sequence`). Same shape decision as [`ChildUsageAttribution`]:
/// a flat optional field on `TranscriptEntry`, not a separate typed
/// message-union class -- see `session::AgentSession::branch_summarize`
/// for how it's produced (a manual, on-demand summary of a branch other
/// than the one currently active, not something every leaf switch
/// generates automatically).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchSummary {
    /// The summarized branch's own tip at the time it was summarized --
    /// not necessarily still true later, if that branch keeps growing.
    pub branch_leaf_sequence: u64,
    /// How many of that branch's own entries (back to its divergence
    /// point from the chain active at summarization time) went into the
    /// summary.
    pub entry_count: u32,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub role: Role,
    pub text: String,
    /// Parity with a bounded slice of `prime-agent`'s image-paste
    /// feature -- see `PARITY.md`'s "Interactive TUI: image paste
    /// support" entry for the full story, including why `rp-server`'s
    /// own multimodal wire types were already real (the missing half was
    /// entirely this project's own text-only shapes). Each entry is a
    /// `data:<mime>;base64,<...>` URI -- the exact shape `rp-server`'s
    /// `ContentPart::ImageUrl` (and, through it, every backend it fronts)
    /// already accepts inline, so no new wire shape was needed on that
    /// side, only this field on this project's own persisted entry.
    /// `#[serde(default)]` for the same pre-existing-transcript reason
    /// every field added after Phase 1 has it.
    #[serde(default)]
    pub images: Option<Vec<String>>,
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
    /// Set only on a `Role::Assistant` entry backed by a real model call
    /// (`provider::RustyProviderModel`, never `EchoProvider`): that
    /// call's own token accounting, straight off `rp-server`'s response.
    /// `#[serde(default)]` so transcripts written before this field
    /// existed still parse.
    #[serde(default)]
    pub usage: Option<Usage>,
    /// Set only on a synthetic `Role::System` entry recording a child
    /// session's usage folded into one of this session's own assistant
    /// turns -- see [`ChildUsageAttribution`] and `session::AgentSession::
    /// attribute_child_usage`. `#[serde(default)]` for the same
    /// pre-existing-transcript reason as `usage`.
    #[serde(default)]
    pub child_usage_attributed: Option<ChildUsageAttribution>,
    /// Parity with `session-format.md`'s `parentId`: which entry (by its
    /// own `sequence`) this one continues from, set once at append time
    /// to whatever `SessionState::active_leaf_sequence` was at that
    /// moment -- ordinary linear conversation flow always has this equal
    /// to "the previous entry," and a real branch appears only once
    /// `session::AgentSession::set_active_leaf` has redirected the active
    /// leaf to an earlier point before the next append. `None` means one
    /// of two things, disambiguated by `sequence`: a genuine root
    /// (`sequence == 1`, the very first entry a session ever gets), or a
    /// legacy entry written before this field existed (`sequence > 1`,
    /// `#[serde(default)]`) -- `session::AgentSession::active_chain`'s own
    /// walk treats the latter as an *implicit* link to `sequence - 1`
    /// (the flat order every pre-existing transcript already has),
    /// staying fully backward-compatible without ever rewriting
    /// `transcript.jsonl` to backfill real values into old entries.
    #[serde(default)]
    pub parent_sequence: Option<u64>,
    /// Set only on a synthetic `Role::System` entry recording a manual,
    /// on-demand summary of a branch other than the one currently active
    /// -- see [`BranchSummary`] and `session::AgentSession::
    /// branch_summarize`. `#[serde(default)]` for the same
    /// pre-existing-transcript reason every field added after Phase 1
    /// has it. Boxed for the same reason `SessionEvent::Snapshot` boxes
    /// its own `SessionState`: `BranchSummary`'s `String` pushes
    /// `TranscriptEntry` (and every enum that embeds one, e.g.
    /// `SessionEvent::Turn`) noticeably larger for a field that's
    /// `None` on the overwhelming majority of entries.
    #[serde(default)]
    pub branch_summary: Option<Box<BranchSummary>>,
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
    /// Paired with `worker_pid`, and written at the same moments: an
    /// opaque `procutil::start_fingerprint` reading for that pid, taken
    /// by the worker for *itself* as it takes ownership.
    ///
    /// Exists so a later liveness check can tell "that worker is still
    /// running" from "that pid number now belongs to something else"
    /// (`procutil::is_same_process`) -- parity with `prime-agent`'s own
    /// PID-reuse-safe lease-owner check (`R-WRK-14`). `#[serde(default)]`
    /// so a `state.json` written before this field existed still parses,
    /// as `None`, which reduces liveness to the bare pid check it always
    /// was rather than failing recovery.
    #[serde(default)]
    pub worker_start_fingerprint: Option<String>,
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
    /// See `CompactionState`'s own doc comment. `#[serde(default)]` for
    /// the same pre-existing-`state.json` reason every other field added
    /// after Phase 1 has it.
    #[serde(default)]
    pub compaction: Option<CompactionState>,
    /// See `ForkedFrom`'s own doc comment. `#[serde(default)]` for the
    /// same pre-existing-`state.json` reason every other field added
    /// after Phase 1 has it.
    #[serde(default)]
    pub forked_from: Option<ForkedFrom>,
    /// Parity with `rlm-runtime.md`'s `RLM_DEPTH`: how many `rlm(...)`
    /// admissions separate this session from the nearest root session
    /// with no `parent_id` of its own -- `0` for a root session, `parent.
    /// rlm_depth + 1` for a session created by `AgentSession::
    /// handle_rlm_run`. Computed server-side in `daemon::
    /// handle_session_new` (a parent lookup, the same place `parent_id`
    /// itself is already validated), not client-settable -- `#[serde(
    /// default)]` is for the same pre-existing-`state.json` reason every
    /// other field added after Phase 1 has it, not because a client is
    /// ever expected to omit it on purpose.
    #[serde(default)]
    pub rlm_depth: u32,
    /// Parity with `rlm-runtime.md`'s `RLM_MAX_DEPTH`: "the inherited
    /// maximum depth" -- a root session resolves this once (`
    /// RUSTY_PRIME_AGENT_RLM_MAX_DEPTH`, default `1`, matching
    /// `rlm-runtime.md`'s own stated default), and every descendant
    /// created via `rlm(...)` inherits the exact same value rather than
    /// re-resolving it, so raising the limit for one recursive tree
    /// doesn't require raising it globally. `#[serde(default)]` for the
    /// same reason `rlm_depth` has it; the field-level default here
    /// (`0`, from `u32::default()`) is never the value actually
    /// persisted for a real session -- `daemon::handle_session_new`
    /// always resolves a real one before a worker is ever spawned.
    #[serde(default)]
    pub rlm_max_depth: u32,
    /// Set only for a session admitted through `rlm(...)`
    /// (`session::AgentSession::handle_rlm_run`): the `sequence` of the
    /// parent's own `Role::Assistant` tool-call entry whose
    /// `execute_python` call invoked `rlm(...)` and admitted this
    /// session -- `rlm-runtime.md`'s "the target parent assistant message
    /// ID," addressed by `sequence` rather than a separate message-id
    /// concept the way this project already addresses every other
    /// transcript entry. Unlike `rlm_depth`/`rlm_max_depth`, this can't be
    /// computed server-side by the daemon: only the spawning worker knows
    /// its own `last_sequence` at admission time, so it travels over the
    /// wire on `Request::SessionNew` instead. `None` for every
    /// non-`rlm`-admitted session (plain `session new`, `session spawn`,
    /// a fork). See `session::AgentSession::attribute_child_usage` for
    /// how this is later read back to fold this child's own usage into
    /// that parent message.
    #[serde(default)]
    pub spawned_from_sequence: Option<u64>,
    /// Parity with `session-format.md`'s "sessions... form a tree
    /// structure via `id`/`parentId` fields, enabling in-place
    /// branching": which transcript entry (by its own `sequence`, this
    /// project's stand-in for a separate message-id concept -- see
    /// `TranscriptEntry::parent_sequence`'s own doc comment) is the
    /// current tip of the conversation `AgentSession::prompt`/
    /// `build_turns` continue from. `None` only for a genuinely empty
    /// transcript (no entries appended yet) or a session whose own
    /// `state.json` predates this field -- `AgentSession::recover`
    /// reconciles the latter the same way it already does for
    /// `last_sequence`. `#[serde(default)]` for that same pre-existing-
    /// `state.json` reason every other field added after Phase 1 has it.
    #[serde(default)]
    pub active_leaf_sequence: Option<u64>,
}

/// Provenance for a session created by `session fork <id> [--at N]`
/// (`Request::SessionFork`) -- a *session-level* fork, not intra-session
/// branching: a fork is a brand-new session (own directory, own
/// `state.json`/`transcript.jsonl`, own worker) whose initial transcript
/// is a copy of `session_id`'s own transcript up through `at_sequence`.
/// Distinct from `SessionState::parent_id` (recursive subagents,
/// `session spawn`): that field relates whole sessions by ownership
/// ("this session was spawned to work on behalf of that one"); this one
/// relates them by shared transcript history ("this session's early
/// turns are a verbatim copy of that one's"). Deliberately *not* used to
/// carry `goal`/`harness` forward onto the forked session -- both are
/// narrative fields whose accuracy depends on the *full* history they
/// were last updated against, which a truncated copy may not match, so
/// a fork starts with neither and only carries forward configuration
/// (`model`/`thinking`/`tools`/`runtime`) that doesn't have that
/// problem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkedFrom {
    pub session_id: String,
    pub at_sequence: u64,
}

/// A session's running compaction summary, parity with `prime-agent`'s
/// automatic context compaction (`packages/coding-agent/docs/
/// compaction.md`). Never mutates `transcript.jsonl` -- that file stays
/// exactly what `session.rs`'s own module doc comment already promises,
/// "the single source of truth", full and untouched; compaction only
/// changes what `session::AgentSession::build_turns` sends to the
/// provider on the *next* prompt; `session attach`/`session repl` still
/// show every turn that ever happened. Re-summarized (not appended to)
/// each time compaction fires again: the new summarization call is
/// given the previous `summary` as context plus whatever newly-old turns
/// crossed the keep-recent boundary since, so `summary` always covers
/// everything up through `compacted_up_to_sequence` in one piece, not a
/// growing chain of separate summaries to re-read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionState {
    /// Transcript entries with `sequence <= compacted_up_to_sequence`
    /// are folded into `summary` rather than sent to the provider
    /// verbatim; entries after it are unaffected.
    pub compacted_up_to_sequence: u64,
    pub summary: String,
    pub compacted_at_ms: u64,
    /// The free-text focus `compact_now`'s own caller supplied (`session
    /// compact <id> [instructions...]`/`/compact [instructions]`), if
    /// any -- previously received as a parameter and folded into the
    /// summarization prompt but never actually stored anywhere, so a
    /// caller (or a later compaction round re-summarizing on top of this
    /// one) had no way to see what focus, if any, produced the current
    /// `summary`. `#[serde(default)]` so a `state.json` persisted before
    /// this field existed still deserializes -- reads as `None`, the
    /// same as if no instructions were ever given, not a hard error.
    #[serde(default)]
    pub instructions: Option<String>,
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
    /// See `ForkedFrom`'s own doc comment. `None` for a session that
    /// wasn't created by `session fork`.
    pub forked_from: Option<ForkedFrom>,
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
