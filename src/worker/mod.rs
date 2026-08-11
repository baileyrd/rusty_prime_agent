//! The session worker: one OS process per root session tree (Required
//! Behavior: "client disconnect does not stop the worker" / "Worker
//! crash ... recovers in-flight session state from disk"). Owns exactly
//! one [`crate::session::AgentSession`], serves the private worker
//! transport (`worker.sock`), and is driven entirely by the supervisor
//! -- it never talks to a public client directly.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusty_tokio::sync::Mutex;

use crate::error::{Context, HarnessError, Result};
use crate::paths;
use crate::procutil;
use crate::protocol::{Request, Response, SessionEvent};
use crate::provider::EchoProvider;
use crate::session::AgentSession;
use crate::tool_runtime::{NoopToolRuntime, ToolRuntime};
use crate::transport::{self, LineStream};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerMode {
    /// Brand-new session; no prior transcript/state on disk.
    New,
    /// Clean resume of a session a previous worker exited normally from.
    Resume,
    /// Resume after finding the recorded worker pid dead: the same
    /// full-transcript-replay path as `Resume`, but followed by an
    /// audible [`SessionEvent::RecoveryMarker`].
    Recover,
}

impl WorkerMode {
    fn as_arg(self) -> &'static str {
        match self {
            WorkerMode::New => "new",
            WorkerMode::Resume => "resume",
            WorkerMode::Recover => "recover",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "new" => Ok(WorkerMode::New),
            "resume" => Ok(WorkerMode::Resume),
            "recover" => Ok(WorkerMode::Recover),
            other => Err(HarnessError::Usage {
                message: format!("unknown worker mode `{other}`"),
            }),
        }
    }
}

pub struct WorkerArgs {
    pub session_id: String,
    pub state_root: PathBuf,
    pub mode: WorkerMode,
    pub name: Option<String>,
}

/// The worker process entrypoint (`harness __worker-main`).
pub async fn run(args: WorkerArgs) -> Result<()> {
    let mut tool_runtime = Box::new(NoopToolRuntime);
    tool_runtime.start().await?;
    let provider = Box::new(EchoProvider);

    let session = match args.mode {
        WorkerMode::New => {
            AgentSession::create(&args.state_root, args.session_id.clone(), args.name.clone(), provider, tool_runtime).await?
        }
        WorkerMode::Resume => AgentSession::recover(&args.state_root, &args.session_id, provider, tool_runtime).await?,
        WorkerMode::Recover => {
            let session = AgentSession::recover(&args.state_root, &args.session_id, provider, tool_runtime).await?;
            session.emit_recovery_marker("worker recovered after a crash; transcript restored from disk");
            session
        }
    };
    let session = Arc::new(Mutex::new(session));

    let socket_path = paths::worker_socket_path(&args.state_root, &args.session_id);
    paths::ensure_dir(Context::Worker, socket_path.parent().expect("socket path has a parent"))?;
    let mut listener = transport::Listener::bind_with_retry(Context::Worker, socket_path, Duration::from_secs(5)).await?;

    loop {
        let conn = listener.accept(Context::Worker).await?;
        let session = session.clone();
        rusty_tokio::spawn(async move {
            if let Err(err) = handle_private_connection(session, conn).await {
                // One bad connection (malformed request, peer vanished
                // mid-write) must not take the whole worker down --
                // that would defeat the entire point of a per-session
                // process. Visible on the worker's own stderr, which
                // Phase 1 leaves inherited/`Null`ed per the spawn
                // policy in `spawn` below.
                eprintln!("worker[{}]: connection error: {err}", std::process::id());
            }
        });
    }
}

async fn handle_private_connection(session: Arc<Mutex<AgentSession>>, mut conn: LineStream) -> Result<()> {
    let request = match conn.read_request(Context::Worker).await? {
        Some(r) => r,
        None => return Ok(()),
    };
    match request {
        Request::Ping => conn.write_response(Context::Worker, &Response::Pong).await,
        Request::SessionAttach { .. } => {
            let (session_id, snapshot, mut events) = {
                let guard = session.lock().await;
                (guard.state.session_id.clone(), guard.snapshot_event(), guard.subscribe())
            };
            conn.write_response(Context::Worker, &Response::SessionAttachStarted { session_id })
                .await?;
            conn.write_event(Context::Worker, &snapshot).await?;
            loop {
                match events.recv().await {
                    Ok(event) => {
                        let ended = matches!(event, SessionEvent::SessionEnded);
                        conn.write_event(Context::Worker, &event).await?;
                        if ended {
                            break;
                        }
                    }
                    // A slow reader that fell behind the broadcast
                    // buffer: it already has the full snapshot as its
                    // recovery baseline (daemon.md's own rule -- "the
                    // attach snapshot is the durable recovery
                    // baseline"), so the honest move is to end this
                    // stream rather than silently skip turns.
                    Err(rusty_tokio::sync::broadcast::RecvError::Lagged(_)) => break,
                    Err(rusty_tokio::sync::broadcast::RecvError::Closed) => break,
                }
            }
            Ok(())
        }
        Request::SessionPrompt { text, .. } => {
            let entry = session.lock().await.prompt(text).await?;
            conn.write_response(Context::Worker, &Response::SessionPromptAck { entry }).await
        }
        Request::WorkerShutdown => {
            session.lock().await.mark_stopped().await?;
            conn.write_response(Context::Worker, &Response::WorkerShutdownAck).await?;
            // Blunt but honest: nothing else in this process needs a
            // graceful drain (no other in-flight state to flush -- the
            // transcript/state writes above already fsync'd via
            // `spawn_blocking`), and plumbing a cooperative shutdown
            // signal through the accept loop for a single-purpose
            // worker process buys correctness this design already has
            // another way -- a killed-instead-of-exited worker is
            // exactly the "worker crash" case this project's recovery
            // path already has to handle regardless.
            std::process::exit(0);
        }
        other => Err(HarnessError::protocol(
            Context::Worker,
            format!("unexpected request on the private worker transport: {other:?}"),
        )),
    }
}

/// Supervisor-side: launch a detached worker process for `session_id`
/// via `rusty_tokio::process::Command` + [`procutil::prepare_detached`]
/// -- see that function's own doc comment for exactly what "detached"
/// means per platform. No process-group/Job-Object placement at spawn
/// time: this project's own `daemon shutdown` only ever needs the
/// graceful path (`Request::WorkerShutdown` over the private socket),
/// which needs no kill primitive at all, and Phase 1's worker spawns no
/// child processes of its own to need a *tree*-kill for -- a plain
/// single-pid kill (`procutil`'s test-only counterpart in
/// `tests/common`) is already the right shape for "simulate this one
/// process crashing".
pub async fn spawn(exe_path: &Path, state_root: &Path, session_id: &str, mode: WorkerMode, name: Option<String>) -> Result<u32> {
    use rusty_tokio::process::{Command, Stdio};

    let cwd = std::env::current_dir().map_err(|e| HarnessError::io(Context::Worker, None, e))?;

    let mut cmd = Command::new(exe_path);
    cmd.current_dir(&cwd)
        .arg("__worker-main")
        .arg("--session-id")
        .arg(session_id)
        .arg("--state-root")
        .arg(state_root)
        .arg("--mode")
        .arg(mode.as_arg());
    if let Some(name) = &name {
        cmd.arg("--name").arg(name);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    procutil::prepare_detached(&mut cmd);

    let child = cmd
        .spawn()
        .map_err(|e| HarnessError::io(Context::Worker, Some(exe_path.to_owned()), e))?;
    let pid = child.id();
    // The worker outlives this handle by design (`prepare_detached`);
    // nothing in this process ever calls `wait()`/`kill()` on it -- the
    // OS process keeps running detached (dropping without waiting just
    // orphans it, `kill_on_drop` defaults to `false`), and its pid is
    // what `state.json` (written by the worker itself moments after
    // this) records as the recovery pointer.
    drop(child);
    Ok(pid)
}

/// Poll for the worker's private socket to become genuinely ready (not
/// just connectable -- see `transport::probe`'s doc comment), for up to
/// `timeout`. Used right after [`spawn`] so the supervisor's response
/// to the client (`SessionNew`, or the first `SessionAttach` after a
/// recovery respawn) is only sent once the worker can actually be
/// reached.
pub async fn wait_ready(socket_path: &Path, timeout: Duration) -> Result<()> {
    transport::wait_ready(Context::Worker, socket_path.to_path_buf(), timeout).await
}
