//! The daemon supervisor: owns the public socket, routes client
//! requests, and recovers session state after its own restart or a
//! worker crash -- without ever treating a particular terminal client as
//! the owner of that state (Required Behavior).
//!
//! Deliberately thin: it never executes providers, tools, or transcript
//! writes itself (all of that lives in `crate::worker`/`crate::session`)
//! -- it only decides *which* worker a request goes to, spawning or
//! recovering one first when needed, then relays bytes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusty_tokio::sync::Mutex;

use crate::catalog;
use crate::error::{Context, HarnessError, Result};
use crate::paths;
use crate::procutil;
use crate::protocol::{
    ForkedFrom, Request, Response, SessionEvent, SessionState, SessionStatus, SessionSummary,
};
use crate::transport::{self, LineStream};
use crate::worker::{self, WorkerMode};

/// How long `session new` / a recovery respawn will wait for the new
/// worker's private socket to become connectable before giving up. Kept
/// larger than `worker::spawn`'s own internal `bind_with_retry` budget
/// (20s -- see that call site's doc comment) for the same reason
/// `client::DAEMON_READY_TIMEOUT` is kept larger than the supervisor's.
const WORKER_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the background schedule-firing loop wakes to check every
/// session's `schedules.json` for due entries. Coarse enough not to spin
/// (a real client interaction is what most sessions actually care about
/// responding to promptly, not this), fine enough that a schedule fires
/// within a few seconds of its due time rather than minutes.
const SCHEDULE_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Recorded in `daemon.pid` across restarts so a replacement supervisor
/// (Required Behavior's crash-recovery path) has a generation number to
/// hand out, mirroring the reference architecture's worker generations.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DaemonPidFile {
    pid: u32,
    generation: u64,
}

pub struct Supervisor {
    state_root: PathBuf,
    exe_path: PathBuf,
    pid: u32,
    generation: u64,
    /// Serializes "check liveness, spawn/recover if needed" per the
    /// whole supervisor (coarse-grained, not per-session): Phase 1's
    /// traffic volume does not need finer locking, and a single lock is
    /// what actually prevents the double-spawn race two concurrent
    /// `SessionAttach`/`SessionPrompt` calls for the same crashed
    /// session would otherwise hit. Stands in for the reference
    /// architecture's per-canonical-path session lease.
    spawn_lock: Mutex<()>,
}

/// The supervisor process entrypoint (`harness __supervisor-main`).
pub async fn run(state_root: PathBuf, exe_path: PathBuf) -> Result<()> {
    paths::ensure_dir(Context::Daemon, &state_root)?;
    paths::ensure_dir(Context::Daemon, &paths::sessions_dir(&state_root))?;
    let generation = record_daemon_pid(&state_root)?;

    let supervisor = Arc::new(Supervisor {
        state_root: state_root.clone(),
        exe_path,
        pid: std::process::id(),
        generation,
        spawn_lock: Mutex::new(()),
    });

    supervisor.recover_on_startup().await;

    // Parity with `prime-agent schedule`: a background task that polls
    // every session's `schedules.json` and fires due entries as ordinary
    // internal `SessionPrompt`s -- see `schedule`'s own module doc
    // comment. Runs for the supervisor's whole lifetime, independent of
    // whether any client is attached to anything.
    {
        let supervisor = supervisor.clone();
        rusty_tokio::spawn(async move {
            loop {
                rusty_tokio::time::sleep(SCHEDULE_POLL_INTERVAL).await;
                supervisor.fire_due_schedules().await;
                // Same cadence, same loop -- parity with `rlm-runtime.md`'s
                // "asynchronously folds the child's ... usage" (see
                // `attribute_pending_child_usage`'s own doc comment).
                supervisor.attribute_pending_child_usage().await;
            }
        });
    }

    // 20s, not a shorter window: rebinding right after force-killing a
    // supervisor that had also made an outbound connection to a
    // worker's private socket is a real Windows AF_UNIX teardown race
    // -- confirmed via real windows-latest CI across several rounds of
    // upstream rustils fixes and, ultimately, `transport::Listener::
    // bind_with_retry`'s own `probe()`-based fallback (see that
    // function's doc comment and docs/decision-request-af-unix-stale-
    // reclaim-race.md in the rustils repo for the full trace). `client::
    // DAEMON_READY_TIMEOUT` is kept strictly larger than this so the
    // CLI doesn't give up on `wait_ready` before this retry loop has
    // had its full budget.
    let mut listener = match transport::Listener::bind_with_retry(
        Context::Daemon,
        paths::daemon_socket_path(&state_root),
        Duration::from_secs(20),
    )
    .await
    {
        Ok(l) => l,
        Err(e) => return Err(e),
    };
    loop {
        let conn = listener.accept(Context::Daemon).await?;
        let supervisor = supervisor.clone();
        rusty_tokio::spawn(async move {
            if let Err(err) = supervisor.handle_public_connection(conn).await {
                eprintln!("daemon: connection error: {err}");
            }
        });
    }
}

fn record_daemon_pid(state_root: &Path) -> Result<u64> {
    let path = paths::daemon_pid_path(state_root);
    let previous_generation = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<DaemonPidFile>(&text).ok())
        .map(|f| f.generation)
        .unwrap_or(0);
    let generation = previous_generation + 1;
    let content = DaemonPidFile {
        pid: std::process::id(),
        generation,
    };
    let json = serde_json::to_string_pretty(&content)
        .map_err(|e| HarnessError::json(Context::Daemon, Some(path.clone()), e))?;
    std::fs::write(&path, json).map_err(|e| HarnessError::io(Context::Daemon, Some(path), e))?;
    Ok(generation)
}

impl Supervisor {
    /// Required Behavior: "supervisor restart recovers in-flight session
    /// state from disk". Best-effort and non-fatal per session -- one
    /// session's respawn failing must not stop the supervisor from
    /// coming up and serving every other session.
    async fn recover_on_startup(&self) {
        let summaries = match catalog::scan(&self.state_root) {
            Ok(s) => s,
            Err(err) => {
                eprintln!("daemon: startup recovery scan failed: {err}");
                return;
            }
        };
        for summary in summaries
            .into_iter()
            .filter(|s| s.status == SessionStatus::Active)
        {
            if let Err(err) = self.ensure_worker_running(&summary.session_id).await {
                eprintln!(
                    "daemon: startup recovery of session {} failed: {err}",
                    summary.session_id
                );
            }
        }
    }

    /// Ensures a live worker exists for `session_id`, spawning a fresh
    /// process (`Recover`/`Resume` mode, per the persisted status) only
    /// when the recorded pid is no longer alive. Returns its private
    /// socket path. Never spawns a session that doesn't already exist on
    /// disk -- `SessionNew` is the only path that creates one.
    async fn ensure_worker_running(&self, session_id: &str) -> Result<PathBuf> {
        let _guard = self.spawn_lock.lock().await;
        let session_dir = paths::session_dir(&self.state_root, session_id);
        let socket_path = paths::worker_socket_path(&self.state_root, session_id);
        let state = catalog::read_session_state(Context::Daemon, &session_dir)?;

        let alive = is_worker_alive(&state)?;
        if alive {
            return Ok(socket_path);
        }

        let mode = if state.status == SessionStatus::Stopped {
            WorkerMode::Resume
        } else {
            WorkerMode::Recover
        };
        // Same sidecar this session used before, not whatever this
        // supervisor generation's own environment happens to say --
        // `state.model` is the persisted source of truth (see
        // `WorkerArgs::model`'s own doc comment).
        if state.model.is_some() {
            crate::rp_server::ensure_running(&self.state_root).await?;
        }
        worker::spawn(
            &self.exe_path,
            &self.state_root,
            session_id,
            mode,
            crate::session::NewSessionMeta {
                name: state.name.clone(),
                model: state.model.clone(),
                // `goal`/`parent_id`/`tools` are never re-seeded on
                // revival -- a session's own `state.json` already
                // carries them by the time it's resumed/recovered, the
                // same as `name`/`model`.
                goal: None,
                parent_id: None,
                tools: None,
                // `thinking`/`runtime` follow `model`'s "always supplied"
                // treatment (see `WorkerArgs::thinking`/`WorkerArgs::runtime`'s
                // own doc comments) -- re-read from persisted state, not
                // re-seeded.
                thinking: state.thinking.clone(),
                runtime: state.runtime.clone(),
                // `rlm_depth`/`rlm_max_depth` follow the same
                // "always supplied, re-read from persisted state"
                // treatment as `thinking`/`runtime` -- a session's own
                // depth in its recursion tree doesn't change across a
                // respawn.
                rlm_depth: Some(state.rlm_depth),
                rlm_max_depth: Some(state.rlm_max_depth),
                // Same "re-read from persisted state, not re-seeded"
                // treatment -- whether/where this session was `rlm(...)`-
                // admitted from doesn't change across a respawn either.
                spawned_from_sequence: state.spawned_from_sequence,
            },
        )
        .await?;
        worker::wait_ready(&socket_path, WORKER_READY_TIMEOUT).await?;
        Ok(socket_path)
    }

    async fn handle_public_connection(&self, mut conn: LineStream) -> Result<()> {
        let request = match conn.read_request(Context::Daemon).await? {
            Some(r) => r,
            None => return Ok(()),
        };
        match request {
            Request::Ping => conn.write_response(Context::Daemon, &Response::Pong).await,
            Request::DaemonStatus => self.handle_daemon_status(&mut conn).await,
            Request::DaemonShutdown { force } => {
                self.handle_daemon_shutdown(&mut conn, force).await
            }
            Request::SessionNew {
                name,
                model,
                goal,
                parent_id,
                spawned_from_sequence,
                thinking,
                tools,
                runtime,
            } => {
                self.handle_session_new(
                    &mut conn,
                    crate::session::NewSessionMeta {
                        name,
                        model,
                        goal,
                        parent_id,
                        thinking,
                        tools,
                        runtime,
                        // Resolved by `handle_session_new` itself (a
                        // parent lookup, or the env-var-fallback root
                        // case) -- not part of the wire `Request::
                        // SessionNew`, since a client never sets these
                        // directly.
                        rlm_depth: None,
                        rlm_max_depth: None,
                        // Unlike `rlm_depth`/`rlm_max_depth`, this one IS
                        // part of the wire shape -- only the spawning
                        // worker knows it, the daemon can't derive it
                        // (see `protocol::SessionState::
                        // spawned_from_sequence`'s own doc comment) --
                        // so it's forwarded straight through, not
                        // resolved here.
                        spawned_from_sequence,
                    },
                )
                .await
            }
            Request::SessionList => self.handle_session_list(&mut conn).await,
            Request::SessionAttach { session_id } => {
                self.handle_session_attach(&mut conn, session_id).await
            }
            Request::SessionPrompt {
                session_id,
                text,
                images,
                request_id,
            } => {
                self.handle_session_prompt(&mut conn, session_id, text, images, request_id)
                    .await
            }
            Request::SessionStop { session_id } => {
                self.handle_session_stop(&mut conn, session_id).await
            }
            Request::SessionRename { session_id, name } => {
                self.handle_session_rename(&mut conn, session_id, name)
                    .await
            }
            Request::SessionCompact {
                session_id,
                instructions,
            } => {
                self.handle_session_compact(&mut conn, session_id, instructions)
                    .await
            }
            Request::SessionInterrupt { session_id } => {
                self.handle_session_interrupt(&mut conn, session_id).await
            }
            Request::SessionExtensionCommand {
                session_id,
                command,
                args,
            } => {
                self.handle_session_extension_command(&mut conn, session_id, command, args)
                    .await
            }
            Request::SessionSetActiveLeaf {
                session_id,
                sequence,
            } => {
                self.handle_session_set_active_leaf(&mut conn, session_id, sequence)
                    .await
            }
            Request::SessionBranchSummarize {
                session_id,
                branch_leaf_sequence,
            } => {
                self.handle_session_branch_summarize(&mut conn, session_id, branch_leaf_sequence)
                    .await
            }
            Request::SessionFork {
                session_id,
                at_sequence,
                name,
            } => {
                self.handle_session_fork(&mut conn, session_id, at_sequence, name)
                    .await
            }
            Request::ScheduleAdd {
                session_id,
                text,
                kind,
            } => {
                self.handle_schedule_add(&mut conn, session_id, text, kind)
                    .await
            }
            Request::ScheduleList { session_id } => {
                self.handle_schedule_list(&mut conn, session_id).await
            }
            Request::ScheduleCancel {
                session_id,
                schedule_id,
            } => {
                self.handle_schedule_cancel(&mut conn, session_id, schedule_id)
                    .await
            }
            Request::GoalUpdate { session_id, action } => {
                self.handle_goal_update(&mut conn, session_id, action).await
            }
            Request::GoalShow { session_id } => self.handle_goal_show(&mut conn, session_id).await,
            Request::HarnessUpdate { session_id, action } => {
                self.handle_harness_update(&mut conn, session_id, action)
                    .await
            }
            Request::HarnessShow { session_id } => {
                self.handle_harness_show(&mut conn, session_id).await
            }
            Request::WorkerShutdown => {
                conn.write_response(
                    Context::Daemon,
                    &Response::Error {
                        message: "WorkerShutdown is only valid on the private worker transport"
                            .into(),
                        conflict: false,
                    },
                )
                .await
            }
            Request::AttributeChildUsage { .. } => {
                conn.write_response(
                    Context::Daemon,
                    &Response::Error {
                        message: "AttributeChildUsage is only valid on the private worker \
                                  transport"
                            .into(),
                        conflict: false,
                    },
                )
                .await
            }
        }
    }

    async fn handle_daemon_status(&self, conn: &mut LineStream) -> Result<()> {
        let sessions_active = catalog::scan(&self.state_root)?
            .iter()
            .filter(|s| s.status == SessionStatus::Active)
            .count();
        conn.write_response(
            Context::Daemon,
            &Response::DaemonStatus {
                protocol_version: crate::protocol::PROTOCOL_VERSION,
                pid: self.pid,
                generation: self.generation,
                sessions_active,
            },
        )
        .await
    }

    async fn handle_daemon_shutdown(&self, conn: &mut LineStream, force: bool) -> Result<()> {
        if !force {
            let sessions = catalog::scan(&self.state_root)?;
            for summary in sessions
                .iter()
                .filter(|s| s.status == SessionStatus::Active)
            {
                let socket_path = paths::worker_socket_path(&self.state_root, &summary.session_id);
                if let Ok(mut private) = transport::connect(Context::Worker, socket_path).await {
                    let _ = private
                        .write_request(Context::Worker, &Request::WorkerShutdown)
                        .await;
                    let _ = private.read_response(Context::Worker).await;
                }
            }
        }
        conn.write_response(Context::Daemon, &Response::DaemonShutdownAck)
            .await?;
        // No-op if no sidecar was ever started for this state root (the
        // ordinary EchoProvider case) -- see `rp_server::shutdown`'s own
        // doc comment.
        crate::rp_server::shutdown(&self.state_root);
        let _ = std::fs::remove_file(paths::daemon_socket_path(&self.state_root));
        let _ = std::fs::remove_file(paths::daemon_pid_path(&self.state_root));
        // Same blunt-but-honest exit as the worker's own `WorkerShutdown`
        // handler: every durable write above has already completed and
        // the ack is already flushed to the client by the time this
        // runs, and this project's recovery path already has to handle
        // an abruptly-gone supervisor (a worker crash is recovered the
        // same way a supervisor crash between requests would be).
        std::process::exit(0);
    }

    async fn handle_session_new(
        &self,
        conn: &mut LineStream,
        mut meta: crate::session::NewSessionMeta,
    ) -> Result<()> {
        // An explicit `--model` always wins; RUSTY_PRIME_AGENT_MODEL is
        // only a fallback default for callers that don't pass one (e.g.
        // scripting against a daemon started with a fixed default
        // model). Resolved server-side, not by the CLI, so it's the
        // daemon's own environment that decides, not whichever
        // environment happened to invoke `session new`.
        meta.model = meta
            .model
            .or_else(|| std::env::var("RUSTY_PRIME_AGENT_MODEL").ok());
        // Parity with `rlm-runtime.md`'s `RLM_DEPTH`/`RLM_MAX_DEPTH`:
        // resolved here, server-side, the same place `parent_id` is
        // already validated -- a child inherits the parent's own
        // `rlm_max_depth` (not re-resolved from this env var), and its
        // `rlm_depth` is exactly one more than the parent's. A root
        // session (no `parent_id`) starts at depth 0 with a freshly
        // resolved max, the same `RUSTY_PRIME_AGENT_MODEL`-style
        // env-var-fallback treatment `model` just got above.
        if let Some(parent_id) = &meta.parent_id {
            let parent_dir = paths::session_dir(&self.state_root, parent_id);
            if !paths::state_file_path(&parent_dir).exists() {
                return conn
                    .write_response(
                        Context::Daemon,
                        &Response::Error {
                            message: format!("unknown parent session {parent_id}"),
                            conflict: true,
                        },
                    )
                    .await;
            }
            let parent_state = catalog::read_session_state(Context::Daemon, &parent_dir)?;
            meta.rlm_depth = Some(parent_state.rlm_depth + 1);
            meta.rlm_max_depth = Some(parent_state.rlm_max_depth);
        } else {
            meta.rlm_depth = Some(0);
            meta.rlm_max_depth = Some(
                std::env::var("RUSTY_PRIME_AGENT_RLM_MAX_DEPTH")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(crate::session::DEFAULT_RLM_MAX_DEPTH),
            );
        }
        let session_id = crate::session::new_session_id();
        let session_dir = paths::session_dir(&self.state_root, &session_id);
        paths::ensure_dir(Context::Session, &session_dir)?;
        // `--tools mcp` needs a running sidecar for its MCP gateway
        // even when this session has no `--model` set (`EchoProvider`,
        // no chat completions at all) -- the tools live on `rp-server`
        // itself, independent of which (if any) provider a prompt
        // actually routes to.
        if meta.model.is_some() || meta.tools.as_deref() == Some("mcp") {
            if let Err(err) = crate::rp_server::ensure_running(&self.state_root).await {
                return conn
                    .write_response(
                        Context::Daemon,
                        &Response::Error {
                            message: format!("failed to start rp-server sidecar: {err}"),
                            conflict: false,
                        },
                    )
                    .await;
            }
        }
        if let Err(err) = worker::spawn(
            &self.exe_path,
            &self.state_root,
            &session_id,
            WorkerMode::New,
            meta,
        )
        .await
        {
            return conn
                .write_response(
                    Context::Daemon,
                    &Response::Error {
                        message: format!("failed to start worker: {err}"),
                        conflict: false,
                    },
                )
                .await;
        }
        let socket_path = paths::worker_socket_path(&self.state_root, &session_id);
        if let Err(err) = worker::wait_ready(&socket_path, WORKER_READY_TIMEOUT).await {
            return conn
                .write_response(
                    Context::Daemon,
                    &Response::Error {
                        message: format!("worker did not become ready: {err}"),
                        conflict: false,
                    },
                )
                .await;
        }
        conn.write_response(Context::Daemon, &Response::SessionNew { session_id })
            .await
    }

    /// `session fork <id> [--at N]` -- see `protocol::Request::
    /// SessionFork`'s own doc comment for the design. Handled directly
    /// by the daemon, not forwarded to `session_id`'s own worker: unlike
    /// `SessionRename`/`SessionCompact` (which mutate an existing
    /// session the worker already owns), this reads `session_id`'s
    /// transcript straight off disk (`session::snapshot_for_fork` -- the
    /// same "files are the source of truth" reasoning `catalog::scan`
    /// already relies on, so this works whether or not `session_id`'s
    /// own worker happens to be running right now) and spawns a brand-
    /// new, independent worker for the result, the same shape
    /// `handle_session_new` already uses.
    async fn handle_session_fork(
        &self,
        conn: &mut LineStream,
        session_id: String,
        at_sequence: Option<u64>,
        name: Option<String>,
    ) -> Result<()> {
        let source_dir = paths::session_dir(&self.state_root, &session_id);
        if !paths::state_file_path(&source_dir).exists() {
            return conn
                .write_response(
                    Context::Daemon,
                    &Response::Error {
                        message: format!("unknown session {session_id}"),
                        conflict: true,
                    },
                )
                .await;
        }
        let (source_state, entries) =
            match crate::session::snapshot_for_fork(&source_dir, at_sequence) {
                Ok(snapshot) => snapshot,
                Err(err) => {
                    return conn
                        .write_response(
                            Context::Daemon,
                            &Response::Error {
                                message: err.to_string(),
                                conflict: true,
                            },
                        )
                        .await
                }
            };
        let forked_from = ForkedFrom {
            session_id: session_id.clone(),
            at_sequence: entries.last().map(|e| e.sequence).unwrap_or(0),
        };
        let new_session_id = crate::session::new_session_id();
        if let Err(err) = crate::session::seed_forked_session(
            &self.state_root,
            &new_session_id,
            name.clone(),
            &source_state,
            entries,
            forked_from,
        )
        .await
        {
            return conn
                .write_response(
                    Context::Daemon,
                    &Response::Error {
                        message: format!("failed to seed forked session: {err}"),
                        conflict: false,
                    },
                )
                .await;
        }
        // Same reasoning as `handle_session_new`: `--tools mcp` needs a
        // running sidecar even with no `--model` set.
        if source_state.model.is_some() || source_state.tools.as_deref() == Some("mcp") {
            if let Err(err) = crate::rp_server::ensure_running(&self.state_root).await {
                return conn
                    .write_response(
                        Context::Daemon,
                        &Response::Error {
                            message: format!("failed to start rp-server sidecar: {err}"),
                            conflict: false,
                        },
                    )
                    .await;
            }
        }
        // `WorkerMode::Resume`, not `New`: `seed_forked_session` already
        // wrote a real `state.json`/`transcript.jsonl` for this session
        // id, the same shape any other stopped session has -- `Resume`
        // is `AgentSession::recover` without the crash-recovery marker
        // `Recover` would (misleadingly) append, exactly right for a
        // session that was never actually running before now.
        if let Err(err) = worker::spawn(
            &self.exe_path,
            &self.state_root,
            &new_session_id,
            WorkerMode::Resume,
            crate::session::NewSessionMeta {
                name,
                model: source_state.model.clone(),
                goal: None,
                parent_id: None,
                thinking: source_state.thinking.clone(),
                tools: source_state.tools.clone(),
                runtime: source_state.runtime.clone(),
                // Same "no `parent_id`" treatment as the two fields
                // above it -- a fork is a fresh, standalone session, not
                // tied into the source's own recursion tree, so it
                // starts at depth 0 with its own freshly-resolved max
                // (`AgentSession::create`'s `unwrap_or` defaults) rather
                // than inheriting `source_state`'s.
                rlm_depth: None,
                rlm_max_depth: None,
                // Same reasoning as `rlm_depth`/`rlm_max_depth` above --
                // a fork isn't a child `rlm(...)` admitted, so it has no
                // parent message to ever attribute usage back to.
                spawned_from_sequence: None,
            },
        )
        .await
        {
            return conn
                .write_response(
                    Context::Daemon,
                    &Response::Error {
                        message: format!("failed to start worker: {err}"),
                        conflict: false,
                    },
                )
                .await;
        }
        let socket_path = paths::worker_socket_path(&self.state_root, &new_session_id);
        if let Err(err) = worker::wait_ready(&socket_path, WORKER_READY_TIMEOUT).await {
            return conn
                .write_response(
                    Context::Daemon,
                    &Response::Error {
                        message: format!("worker did not become ready: {err}"),
                        conflict: false,
                    },
                )
                .await;
        }
        conn.write_response(
            Context::Daemon,
            &Response::SessionNew {
                session_id: new_session_id,
            },
        )
        .await
    }

    async fn handle_session_list(&self, conn: &mut LineStream) -> Result<()> {
        let sessions = catalog::scan(&self.state_root)?;
        conn.write_response(Context::Daemon, &Response::SessionList { sessions })
            .await
    }

    async fn handle_schedule_add(
        &self,
        conn: &mut LineStream,
        session_id: String,
        text: String,
        kind: crate::protocol::ScheduleKind,
    ) -> Result<()> {
        let session_id = self.resolve_session_id(&session_id);
        let session_dir = paths::session_dir(&self.state_root, &session_id);
        if !paths::state_file_path(&session_dir).exists() {
            return conn
                .write_response(
                    Context::Daemon,
                    &Response::Error {
                        message: format!("unknown session {session_id}"),
                        conflict: true,
                    },
                )
                .await;
        }
        let schedule_id = crate::schedule::add(&session_dir, text, kind)?;
        conn.write_response(Context::Daemon, &Response::ScheduleAdded { schedule_id })
            .await
    }

    async fn handle_schedule_list(&self, conn: &mut LineStream, session_id: String) -> Result<()> {
        let session_id = self.resolve_session_id(&session_id);
        let session_dir = paths::session_dir(&self.state_root, &session_id);
        if !paths::state_file_path(&session_dir).exists() {
            return conn
                .write_response(
                    Context::Daemon,
                    &Response::Error {
                        message: format!("unknown session {session_id}"),
                        conflict: true,
                    },
                )
                .await;
        }
        let entries = crate::schedule::read_all(&session_dir)?;
        conn.write_response(Context::Daemon, &Response::ScheduleList { entries })
            .await
    }

    async fn handle_schedule_cancel(
        &self,
        conn: &mut LineStream,
        session_id: String,
        schedule_id: String,
    ) -> Result<()> {
        let session_id = self.resolve_session_id(&session_id);
        let session_dir = paths::session_dir(&self.state_root, &session_id);
        if !paths::state_file_path(&session_dir).exists() {
            return conn
                .write_response(
                    Context::Daemon,
                    &Response::Error {
                        message: format!("unknown session {session_id}"),
                        conflict: true,
                    },
                )
                .await;
        }
        let found = crate::schedule::cancel(&session_dir, &schedule_id)?;
        conn.write_response(Context::Daemon, &Response::ScheduleCancelAck { found })
            .await
    }

    /// The background firing loop's own entry point (`daemon::run`'s
    /// spawned task) -- best-effort and non-fatal per session/entry, the
    /// same reasoning as `recover_on_startup`: one session's schedule
    /// misbehaving must not stop every other session's from firing.
    async fn fire_due_schedules(&self) {
        let summaries = match catalog::scan(&self.state_root) {
            Ok(s) => s,
            Err(err) => {
                eprintln!("daemon: schedule scan failed: {err}");
                return;
            }
        };
        let now = paths::now_ms();
        for summary in summaries {
            let session_dir = paths::session_dir(&self.state_root, &summary.session_id);
            let due = match crate::schedule::take_due(&session_dir, now) {
                Ok(d) => d,
                Err(err) => {
                    eprintln!(
                        "daemon: schedule check failed for session {}: {err}",
                        summary.session_id
                    );
                    continue;
                }
            };
            for (schedule_id, text) in due {
                if let Err(err) = self.fire_one_schedule(&summary.session_id, text).await {
                    eprintln!(
                        "daemon: schedule {schedule_id} for session {} failed to fire: {err}",
                        summary.session_id
                    );
                }
            }
        }
    }

    /// Parity with `rlm-runtime.md`'s "Prime Agent asynchronously folds
    /// the child's assistant usage and cost into the parent assistant
    /// turn that launched it" -- background, automatic, no explicit user
    /// action needed, mirroring `fire_due_schedules`'s own cadence and
    /// "scan, act, log-and-continue on a per-item failure" shape. For
    /// every session with a `parent_id` whose own worker is no longer
    /// alive (the closest real "the child's task is done" signal this
    /// project's architecture has -- see `session::AgentSession::
    /// attribute_child_usage`'s own doc comment), forwards `Request::
    /// AttributeChildUsage` to the *parent's* own worker, but only when
    /// that parent is itself `Active` right now -- an inactive parent is
    /// left for a later poll to catch once it's running again, rather
    /// than attempted and logged as a failure every cycle forever.
    ///
    /// A known, accepted inefficiency, not a correctness bug: there's no
    /// separate "already attempted" bookkeeping here, so a long-stopped
    /// child of a continuously-`Active` parent gets a harmless redundant
    /// delivery attempt every cycle for as long as the parent stays up,
    /// even after `attribute_child_usage`'s own idempotency check has
    /// already absorbed the real attribution -- see `PARITY.md`'s own
    /// entry for why this was accepted rather than engineered around.
    async fn attribute_pending_child_usage(&self) {
        let summaries = match catalog::scan(&self.state_root) {
            Ok(s) => s,
            Err(err) => {
                eprintln!("daemon: child-usage-attribution scan failed: {err}");
                return;
            }
        };
        let by_id: HashMap<&str, &SessionSummary> = summaries
            .iter()
            .map(|s| (s.session_id.as_str(), s))
            .collect();
        for child in &summaries {
            let Some(parent_id) = &child.parent_id else {
                continue;
            };
            if child.status == SessionStatus::Active {
                continue;
            }
            let Some(parent) = by_id.get(parent_id.as_str()) else {
                continue;
            };
            if parent.status != SessionStatus::Active {
                continue;
            }
            if let Err(err) = self
                .attribute_one_child_usage(parent_id, &child.session_id)
                .await
            {
                eprintln!(
                    "daemon: child-usage attribution for child {} (parent {parent_id}) \
                     failed: {err}",
                    child.session_id
                );
            }
        }
    }

    /// Forwards one `Request::AttributeChildUsage` to `parent_id`'s own
    /// private worker socket -- the daemon never writes to a session's
    /// `transcript.jsonl`/`state.json` itself (see `ARCHITECTURE.md`'s
    /// "only a session's own worker owns its persisted state"
    /// invariant); this is that same relay pattern `fire_one_schedule`
    /// already uses for `SessionPrompt`.
    async fn attribute_one_child_usage(&self, parent_id: &str, child_id: &str) -> Result<()> {
        let socket_path = paths::worker_socket_path(&self.state_root, parent_id);
        let mut private = transport::connect(Context::Worker, socket_path).await?;
        private
            .write_request(
                Context::Worker,
                &Request::AttributeChildUsage {
                    child_id: child_id.to_string(),
                },
            )
            .await?;
        private.read_response(Context::Worker).await?;
        Ok(())
    }

    /// Fires one due schedule entry as an ordinary internal
    /// `SessionPrompt` -- reviving the worker first if needed, the same
    /// as any client-issued prompt would. No `conn` to report a failure
    /// to (nobody is attached), so `fire_due_schedules` just logs it.
    async fn fire_one_schedule(&self, session_id: &str, text: String) -> Result<()> {
        let socket_path = self.ensure_worker_running(session_id).await?;
        let mut private = transport::connect(Context::Worker, socket_path).await?;
        private
            .write_request(
                Context::Worker,
                &Request::SessionPrompt {
                    session_id: session_id.to_string(),
                    text,
                    images: None,
                    request_id: None,
                },
            )
            .await?;
        private
            .read_response(Context::Worker)
            .await?
            .ok_or_else(|| {
                HarnessError::protocol(
                    Context::Worker,
                    "worker closed before responding to scheduled prompt",
                )
            })?;
        Ok(())
    }

    /// Resume-by-partial-ID convenience -- see `PARITY.md`'s "Bounded
    /// candidates batch 3" entry. `partial` is returned unchanged
    /// whenever it already names a real session directly (the fast,
    /// common path, and also what keeps a full id from ever being
    /// second-guessed even if it happens to be a literal prefix of some
    /// other session's own id -- vanishingly unlikely given `new_session_
    /// id`'s own nanosecond-timestamp-plus-pid shape, but checked first
    /// regardless rather than assumed impossible) or whenever it
    /// resolves, via `catalog::scan`, to *exactly* one real session id
    /// starting with it. Zero matches or more than one both fall through
    /// to returning `partial` itself unresolved -- every caller already
    /// has its own "unknown session" error path for that string, so
    /// there's no need for this function to duplicate it (an ambiguous
    /// prefix and a genuinely unknown one end up reported identically,
    /// a real but bounded imprecision, not a distinction this bounded
    /// slice attempts).
    fn resolve_session_id(&self, partial: &str) -> String {
        let session_dir = paths::session_dir(&self.state_root, partial);
        if paths::state_file_path(&session_dir).exists() {
            return partial.to_string();
        }
        let Ok(sessions) = catalog::scan(&self.state_root) else {
            return partial.to_string();
        };
        let mut matches = sessions
            .into_iter()
            .map(|s| s.session_id)
            .filter(|id| id.starts_with(partial));
        match (matches.next(), matches.next()) {
            (Some(only), None) => only,
            _ => partial.to_string(),
        }
    }

    /// Shared by `SessionAttach`/`SessionPrompt`: validate the session
    /// exists, recover/resume its worker if needed, and report either
    /// path as a structured `session_already_active`-shaped
    /// [`Response::Error`] rather than a bug when it fails -- matching
    /// the reference protocol's own "structured errors for recoverable
    /// cases" contract. `session_id` is resolved through `resolve_
    /// session_id` first, so every one of this function's ten callers
    /// gets partial-id resolution for free.
    async fn resolve_worker(
        &self,
        conn: &mut LineStream,
        session_id: &str,
    ) -> Result<Option<PathBuf>> {
        let session_id = &self.resolve_session_id(session_id);
        let session_dir = paths::session_dir(&self.state_root, session_id);
        if !paths::state_file_path(&session_dir).exists() {
            conn.write_response(
                Context::Daemon,
                &Response::Error {
                    message: format!("unknown session {session_id}"),
                    conflict: true,
                },
            )
            .await?;
            return Ok(None);
        }
        match self.ensure_worker_running(session_id).await {
            Ok(path) => Ok(Some(path)),
            Err(err) => {
                conn.write_response(
                    Context::Daemon,
                    &Response::Error {
                        message: format!("could not reach worker for session {session_id}: {err}"),
                        conflict: false,
                    },
                )
                .await?;
                Ok(None)
            }
        }
    }

    async fn handle_session_attach(&self, conn: &mut LineStream, session_id: String) -> Result<()> {
        let socket_path = match self.resolve_worker(conn, &session_id).await? {
            Some(p) => p,
            None => return Ok(()),
        };
        let mut private = transport::connect(Context::Worker, socket_path).await?;
        private
            .write_request(Context::Worker, &Request::SessionAttach { session_id })
            .await?;
        let response = private
            .read_response(Context::Worker)
            .await?
            .ok_or_else(|| {
                HarnessError::protocol(Context::Worker, "worker closed before responding to attach")
            })?;
        let started = matches!(response, Response::SessionAttachStarted { .. });
        conn.write_response(Context::Daemon, &response).await?;
        if !started {
            return Ok(());
        }
        while let Some(event) = private.read_event(Context::Worker).await? {
            let ended = matches!(event, SessionEvent::SessionEnded);
            conn.write_event(Context::Daemon, &event).await?;
            if ended {
                break;
            }
        }
        Ok(())
    }

    /// Parity with `prime-agent stop <agent>`. Deliberately does not go
    /// through `ensure_worker_running`/`resolve_worker` -- those exist to
    /// *revive* a session's worker on demand, the opposite of what
    /// stopping one should do. Held under `spawn_lock` for the same
    /// reason `ensure_worker_running` is: without it, a `SessionStop`
    /// racing a concurrent `SessionAttach`/`SessionPrompt`'s respawn
    /// could observe "no live worker" just before the other request
    /// finishes spawning one, then never stop it.
    async fn handle_session_stop(&self, conn: &mut LineStream, session_id: String) -> Result<()> {
        let session_id = self.resolve_session_id(&session_id);
        let session_dir = paths::session_dir(&self.state_root, &session_id);
        if !paths::state_file_path(&session_dir).exists() {
            return conn
                .write_response(
                    Context::Daemon,
                    &Response::Error {
                        message: format!("unknown session {session_id}"),
                        conflict: true,
                    },
                )
                .await;
        }
        let _guard = self.spawn_lock.lock().await;
        let state = catalog::read_session_state(Context::Daemon, &session_dir)?;
        if !is_worker_alive(&state)? {
            return conn
                .write_response(
                    Context::Daemon,
                    &Response::SessionStopAck {
                        already_stopped: true,
                    },
                )
                .await;
        }
        let socket_path = paths::worker_socket_path(&self.state_root, &session_id);
        if let Ok(mut private) = transport::connect(Context::Worker, socket_path).await {
            let _ = private
                .write_request(Context::Worker, &Request::WorkerShutdown)
                .await;
            let _ = private.read_response(Context::Worker).await;
        }
        conn.write_response(
            Context::Daemon,
            &Response::SessionStopAck {
                already_stopped: false,
            },
        )
        .await
    }

    async fn handle_session_prompt(
        &self,
        conn: &mut LineStream,
        session_id: String,
        text: String,
        images: Option<Vec<String>>,
        request_id: Option<String>,
    ) -> Result<()> {
        let socket_path = match self.resolve_worker(conn, &session_id).await? {
            Some(p) => p,
            None => return Ok(()),
        };
        let mut private = transport::connect(Context::Worker, socket_path).await?;
        private
            .write_request(
                Context::Worker,
                &Request::SessionPrompt {
                    session_id,
                    text,
                    images,
                    request_id,
                },
            )
            .await?;
        let response = private
            .read_response(Context::Worker)
            .await?
            .ok_or_else(|| {
                HarnessError::protocol(Context::Worker, "worker closed before responding to prompt")
            })?;
        conn.write_response(Context::Daemon, &response).await
    }

    async fn handle_session_rename(
        &self,
        conn: &mut LineStream,
        session_id: String,
        name: Option<String>,
    ) -> Result<()> {
        let socket_path = match self.resolve_worker(conn, &session_id).await? {
            Some(p) => p,
            None => return Ok(()),
        };
        let mut private = transport::connect(Context::Worker, socket_path).await?;
        private
            .write_request(
                Context::Worker,
                &Request::SessionRename { session_id, name },
            )
            .await?;
        let response = private
            .read_response(Context::Worker)
            .await?
            .ok_or_else(|| {
                HarnessError::protocol(Context::Worker, "worker closed before responding to rename")
            })?;
        conn.write_response(Context::Daemon, &response).await
    }

    async fn handle_session_set_active_leaf(
        &self,
        conn: &mut LineStream,
        session_id: String,
        sequence: u64,
    ) -> Result<()> {
        let socket_path = match self.resolve_worker(conn, &session_id).await? {
            Some(p) => p,
            None => return Ok(()),
        };
        let mut private = transport::connect(Context::Worker, socket_path).await?;
        private
            .write_request(
                Context::Worker,
                &Request::SessionSetActiveLeaf {
                    session_id,
                    sequence,
                },
            )
            .await?;
        let response = private
            .read_response(Context::Worker)
            .await?
            .ok_or_else(|| {
                HarnessError::protocol(
                    Context::Worker,
                    "worker closed before responding to set_active_leaf",
                )
            })?;
        conn.write_response(Context::Daemon, &response).await
    }

    async fn handle_session_branch_summarize(
        &self,
        conn: &mut LineStream,
        session_id: String,
        branch_leaf_sequence: u64,
    ) -> Result<()> {
        let socket_path = match self.resolve_worker(conn, &session_id).await? {
            Some(p) => p,
            None => return Ok(()),
        };
        let mut private = transport::connect(Context::Worker, socket_path).await?;
        private
            .write_request(
                Context::Worker,
                &Request::SessionBranchSummarize {
                    session_id,
                    branch_leaf_sequence,
                },
            )
            .await?;
        let response = private
            .read_response(Context::Worker)
            .await?
            .ok_or_else(|| {
                HarnessError::protocol(
                    Context::Worker,
                    "worker closed before responding to branch_summarize",
                )
            })?;
        conn.write_response(Context::Daemon, &response).await
    }

    async fn handle_session_compact(
        &self,
        conn: &mut LineStream,
        session_id: String,
        instructions: Option<String>,
    ) -> Result<()> {
        let socket_path = match self.resolve_worker(conn, &session_id).await? {
            Some(p) => p,
            None => return Ok(()),
        };
        let mut private = transport::connect(Context::Worker, socket_path).await?;
        private
            .write_request(
                Context::Worker,
                &Request::SessionCompact {
                    session_id,
                    instructions,
                },
            )
            .await?;
        let response = private
            .read_response(Context::Worker)
            .await?
            .ok_or_else(|| {
                HarnessError::protocol(
                    Context::Worker,
                    "worker closed before responding to compact",
                )
            })?;
        conn.write_response(Context::Daemon, &response).await
    }

    /// Relays `Request::SessionInterrupt` to the owning worker unchanged
    /// -- same `resolve_worker`/connect/forward/relay shape every other
    /// session-scoped request here already has. See that request's own
    /// doc comment for why the *worker's* own handler deliberately never
    /// takes the session lock; nothing about this daemon-side relay
    /// changes as a result -- it's a plain request/response round trip
    /// like any other from this side.
    async fn handle_session_interrupt(
        &self,
        conn: &mut LineStream,
        session_id: String,
    ) -> Result<()> {
        let socket_path = match self.resolve_worker(conn, &session_id).await? {
            Some(p) => p,
            None => return Ok(()),
        };
        let mut private = transport::connect(Context::Worker, socket_path).await?;
        private
            .write_request(Context::Worker, &Request::SessionInterrupt { session_id })
            .await?;
        let response = private
            .read_response(Context::Worker)
            .await?
            .ok_or_else(|| {
                HarnessError::protocol(
                    Context::Worker,
                    "worker closed before responding to interrupt",
                )
            })?;
        conn.write_response(Context::Daemon, &response).await
    }

    async fn handle_session_extension_command(
        &self,
        conn: &mut LineStream,
        session_id: String,
        command: String,
        args: String,
    ) -> Result<()> {
        let socket_path = match self.resolve_worker(conn, &session_id).await? {
            Some(p) => p,
            None => return Ok(()),
        };
        let mut private = transport::connect(Context::Worker, socket_path).await?;
        private
            .write_request(
                Context::Worker,
                &Request::SessionExtensionCommand {
                    session_id,
                    command,
                    args,
                },
            )
            .await?;
        let response = private
            .read_response(Context::Worker)
            .await?
            .ok_or_else(|| {
                HarnessError::protocol(
                    Context::Worker,
                    "worker closed before responding to extension command",
                )
            })?;
        conn.write_response(Context::Daemon, &response).await
    }

    async fn handle_goal_update(
        &self,
        conn: &mut LineStream,
        session_id: String,
        action: crate::protocol::GoalAction,
    ) -> Result<()> {
        let socket_path = match self.resolve_worker(conn, &session_id).await? {
            Some(p) => p,
            None => return Ok(()),
        };
        let mut private = transport::connect(Context::Worker, socket_path).await?;
        private
            .write_request(Context::Worker, &Request::GoalUpdate { session_id, action })
            .await?;
        let response = private
            .read_response(Context::Worker)
            .await?
            .ok_or_else(|| {
                HarnessError::protocol(
                    Context::Worker,
                    "worker closed before responding to goal update",
                )
            })?;
        conn.write_response(Context::Daemon, &response).await
    }

    async fn handle_goal_show(&self, conn: &mut LineStream, session_id: String) -> Result<()> {
        let session_id = self.resolve_session_id(&session_id);
        let session_dir = paths::session_dir(&self.state_root, &session_id);
        if !paths::state_file_path(&session_dir).exists() {
            return conn
                .write_response(
                    Context::Daemon,
                    &Response::Error {
                        message: format!("unknown session {session_id}"),
                        conflict: true,
                    },
                )
                .await;
        }
        let state = catalog::read_session_state(Context::Daemon, &session_dir)?;
        conn.write_response(Context::Daemon, &Response::GoalShow { goal: state.goal })
            .await
    }

    async fn handle_harness_update(
        &self,
        conn: &mut LineStream,
        session_id: String,
        action: crate::protocol::HarnessAction,
    ) -> Result<()> {
        let socket_path = match self.resolve_worker(conn, &session_id).await? {
            Some(p) => p,
            None => return Ok(()),
        };
        let mut private = transport::connect(Context::Worker, socket_path).await?;
        private
            .write_request(
                Context::Worker,
                &Request::HarnessUpdate { session_id, action },
            )
            .await?;
        let response = private
            .read_response(Context::Worker)
            .await?
            .ok_or_else(|| {
                HarnessError::protocol(
                    Context::Worker,
                    "worker closed before responding to harness update",
                )
            })?;
        conn.write_response(Context::Daemon, &response).await
    }

    async fn handle_harness_show(&self, conn: &mut LineStream, session_id: String) -> Result<()> {
        let session_id = self.resolve_session_id(&session_id);
        let session_dir = paths::session_dir(&self.state_root, &session_id);
        if !paths::state_file_path(&session_dir).exists() {
            return conn
                .write_response(
                    Context::Daemon,
                    &Response::Error {
                        message: format!("unknown session {session_id}"),
                        conflict: true,
                    },
                )
                .await;
        }
        let state = catalog::read_session_state(Context::Daemon, &session_dir)?;
        conn.write_response(
            Context::Daemon,
            &Response::HarnessShow {
                state: state.harness,
            },
        )
        .await
    }
}

fn is_worker_alive(state: &SessionState) -> Result<bool> {
    use crate::error::IoResultExt;
    match state.worker_pid {
        None => Ok(false),
        Some(pid) => procutil::is_alive(pid).ctx(Context::Worker),
    }
}
