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
    /// Private transport only: supervisor -> worker, asking it to persist
    /// its final state and exit cleanly (used by `daemon shutdown` and
    /// `SessionStop`).
    WorkerShutdown,
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
    WorkerShutdownAck,
}

/// One line of the attach event stream, after `SessionAttachStarted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    /// The durable recovery baseline (daemon.md: "the attach snapshot is
    /// the durable recovery baseline"), sent once, first: the full
    /// transcript replayed from disk plus the current session state.
    Snapshot {
        state: SessionState,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub role: Role,
    pub text: String,
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
}
