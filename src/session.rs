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

use rusty_tokio::sync::broadcast;

use crate::error::{Context, HarnessError, Result};
use crate::paths::{self, now_ms};
use crate::protocol::{Role, SessionEvent, SessionState, SessionStatus, TranscriptEntry};
use crate::provider::ModelProvider;
use crate::tool_runtime::ToolRuntime;

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
    provider: Box<dyn ModelProvider>,
    /// Held, started, and shut down (see `mark_stopped`) like a real
    /// backend would be, but never called from `prompt()` -- Phase 1 has
    /// no tool-execution requirement (non-goal). Wiring `execute` into
    /// the turn loop is Phase 2's job, once a real kernel backend exists
    /// to call it against.
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

impl AgentSession {
    /// Create a brand-new session: fresh id, empty transcript, generation
    /// 1. Persists the initial `state.json` before returning.
    pub async fn create(
        state_root: &Path,
        session_id: String,
        name: Option<String>,
        provider: Box<dyn ModelProvider>,
        tool_runtime: Box<dyn ToolRuntime>,
    ) -> Result<Self> {
        let session_dir = paths::session_dir(state_root, &session_id);
        paths::ensure_dir(Context::Session, &session_dir)?;
        let now = now_ms();
        let state = SessionState {
            session_id,
            name,
            status: SessionStatus::Active,
            worker_pid: Some(std::process::id()),
            generation: 1,
            last_sequence: 0,
            created_at_ms: now,
            updated_at_ms: now,
        };
        let session = AgentSession {
            state,
            transcript: Vec::new(),
            session_dir,
            provider,
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
            provider,
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
            state: self.state.clone(),
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

    /// Append a user turn, ask the (fake) provider for a reply, append
    /// its assistant turn, and return that assistant entry as the
    /// prompt's ack.
    pub async fn prompt(&mut self, text: String) -> Result<TranscriptEntry> {
        self.append(Role::User, text.clone()).await?;
        let reply = self.provider.respond(&text).await?;
        self.append(Role::Assistant, reply).await
    }

    async fn append(&mut self, role: Role, text: String) -> Result<TranscriptEntry> {
        let entry = TranscriptEntry {
            sequence: self.state.last_sequence + 1,
            timestamp_ms: now_ms(),
            role,
            text,
        };
        append_transcript_line(&self.session_dir, &entry).await?;
        self.transcript.push(entry.clone());
        self.state.last_sequence = entry.sequence;
        self.state.updated_at_ms = entry.timestamp_ms;
        self.write_state().await?;
        // No receivers is the ordinary "nobody attached right now" case,
        // not an error -- the transcript write above is what actually
        // makes this turn durable.
        let _ = self.events.send(SessionEvent::Turn { entry: entry.clone() });
        Ok(entry)
    }

    async fn write_state(&self) -> Result<()> {
        write_state(&self.session_dir, &self.state).await
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
    let text = std::fs::read_to_string(&path).map_err(|e| HarnessError::io(Context::Session, Some(path.clone()), e))?;
    serde_json::from_str(&text).map_err(|e| HarnessError::json(Context::Session, Some(path), e))
}

async fn write_state(session_dir: &Path, state: &SessionState) -> Result<()> {
    let path = paths::state_file_path(session_dir);
    let state = state.clone();
    let json = serde_json::to_string_pretty(&state).map_err(|e| HarnessError::json(Context::Session, Some(path.clone()), e))?;
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
    let file = std::fs::File::open(&path).map_err(|e| HarnessError::io(Context::Session, Some(path.clone()), e))?;
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
    let line = serde_json::to_string(entry).map_err(|e| HarnessError::json(Context::Session, Some(path.clone()), e))?;
    let join = rusty_tokio::spawn_blocking(move || {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| HarnessError::io(Context::Session, Some(path.clone()), e))?;
        writeln!(file, "{line}").map_err(|e| HarnessError::io(Context::Session, Some(path), e))?;
        file.flush().map_err(|e| HarnessError::io(Context::Session, None, e))
    })
    .await;
    join.map_err(|_| HarnessError::protocol(Context::Session, "transcript append task panicked"))?
}
