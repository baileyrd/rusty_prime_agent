//! The daemon supervisor: owns the public socket, routes client
//! requests, and recovers session state after its own restart or a
//! worker crash -- without ever treating a particular terminal client as
//! the owner of that state (Required Behavior).
//!
//! Deliberately thin: it never executes providers, tools, or transcript
//! writes itself (all of that lives in `crate::worker`/`crate::session`)
//! -- it only decides *which* worker a request goes to, spawning or
//! recovering one first when needed, then relays bytes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusty_tokio::sync::Mutex;

use crate::catalog;
use crate::error::{Context, HarnessError, Result};
use crate::paths;
use crate::procutil;
use crate::protocol::{Request, Response, SessionEvent, SessionState, SessionStatus};
use crate::transport::{self, LineStream};
use crate::worker::{self, WorkerMode};

/// How long `session new` / a recovery respawn will wait for the new
/// worker's private socket to become connectable before giving up. Kept
/// larger than `worker::spawn`'s own internal `bind_with_retry` budget
/// (20s -- see that call site's doc comment) for the same reason
/// `client::DAEMON_READY_TIMEOUT` is kept larger than the supervisor's.
const WORKER_READY_TIMEOUT: Duration = Duration::from_secs(30);

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
        worker::spawn(
            &self.exe_path,
            &self.state_root,
            session_id,
            mode,
            state.name.clone(),
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
            Request::DaemonShutdown => self.handle_daemon_shutdown(&mut conn).await,
            Request::SessionNew { name } => self.handle_session_new(&mut conn, name).await,
            Request::SessionList => self.handle_session_list(&mut conn).await,
            Request::SessionAttach { session_id } => {
                self.handle_session_attach(&mut conn, session_id).await
            }
            Request::SessionPrompt { session_id, text } => {
                self.handle_session_prompt(&mut conn, session_id, text)
                    .await
            }
            Request::SessionStop { session_id } => {
                self.handle_session_stop(&mut conn, session_id).await
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

    async fn handle_daemon_shutdown(&self, conn: &mut LineStream) -> Result<()> {
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
        conn.write_response(Context::Daemon, &Response::DaemonShutdownAck)
            .await?;
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

    async fn handle_session_new(&self, conn: &mut LineStream, name: Option<String>) -> Result<()> {
        let session_id = crate::session::new_session_id();
        let session_dir = paths::session_dir(&self.state_root, &session_id);
        paths::ensure_dir(Context::Session, &session_dir)?;
        if let Err(err) = worker::spawn(
            &self.exe_path,
            &self.state_root,
            &session_id,
            WorkerMode::New,
            name,
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

    async fn handle_session_list(&self, conn: &mut LineStream) -> Result<()> {
        let sessions = catalog::scan(&self.state_root)?;
        conn.write_response(Context::Daemon, &Response::SessionList { sessions })
            .await
    }

    /// Shared by `SessionAttach`/`SessionPrompt`: validate the session
    /// exists, recover/resume its worker if needed, and report either
    /// path as a structured `session_already_active`-shaped
    /// [`Response::Error`] rather than a bug when it fails -- matching
    /// the reference protocol's own "structured errors for recoverable
    /// cases" contract.
    async fn resolve_worker(
        &self,
        conn: &mut LineStream,
        session_id: &str,
    ) -> Result<Option<PathBuf>> {
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
    ) -> Result<()> {
        let socket_path = match self.resolve_worker(conn, &session_id).await? {
            Some(p) => p,
            None => return Ok(()),
        };
        let mut private = transport::connect(Context::Worker, socket_path).await?;
        private
            .write_request(
                Context::Worker,
                &Request::SessionPrompt { session_id, text },
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
}

fn is_worker_alive(state: &SessionState) -> Result<bool> {
    use crate::error::IoResultExt;
    match state.worker_pid {
        None => Ok(false),
        Some(pid) => procutil::is_alive(pid).ctx(Context::Worker),
    }
}
