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
    /// See `Request::SessionNew::model`'s own doc comment. Always
    /// supplied by the daemon at spawn time -- for `New` it's whatever
    /// the client's request asked for, for `Resume`/`Recover` it's
    /// `state.model.clone()` read back from `state.json` (`daemon::
    /// Supervisor::ensure_worker_running`), never re-resolved from this
    /// process's own environment. That keeps a session's backend fixed
    /// for its whole lifetime even if the daemon's own environment
    /// changes across a restart.
    pub model: Option<String>,
    /// Parity with `prime-agent --goal`: only meaningful for
    /// `WorkerMode::New` (a fresh session seeding its own goal at
    /// creation) -- `daemon::Supervisor::ensure_worker_running` never
    /// supplies one for `Resume`/`Recover`, since a session's goal by
    /// then already lives in its own persisted `state.json`, the same
    /// way `name`/`model` do.
    pub goal: Option<String>,
}

/// Builds this worker's `ModelProvider`. `model.is_none()` (the ordinary
/// case) is `EchoProvider`, no `rp_server` involvement at all. Otherwise,
/// the `rp-server` sidecar was already started by the supervisor before
/// this worker was ever spawned (`daemon::Supervisor::ensure_worker_running`
/// calls `rp_server::ensure_running` first whenever a session's `model`
/// is `Some`) -- `rp_server::read_port` finding nothing recorded here
/// would mean that invariant broke, worth failing loudly on rather than
/// silently falling back to `EchoProvider`.
fn build_provider(
    state_root: &Path,
    model: Option<String>,
) -> Result<Box<dyn crate::provider::ModelProvider>> {
    let Some(model) = model else {
        return Ok(Box::new(EchoProvider));
    };
    let port = crate::rp_server::read_port(state_root).ok_or_else(|| {
        HarnessError::conflict(
            Context::Provider,
            "session has a model set but no rp-server sidecar is recorded -- \
             this is a bug in daemon::Supervisor's spawn ordering",
        )
    })?;
    Ok(Box::new(crate::provider::RustyProviderModel::new(
        port, model,
    )))
}

/// The worker process entrypoint (`harness __worker-main`).
pub async fn run(args: WorkerArgs) -> Result<()> {
    let mut tool_runtime = Box::new(NoopToolRuntime);
    tool_runtime.start().await?;
    let provider = build_provider(&args.state_root, args.model.clone())?;

    let session = match args.mode {
        WorkerMode::New => {
            AgentSession::create(
                &args.state_root,
                args.session_id.clone(),
                args.name.clone(),
                args.model.clone(),
                args.goal.clone(),
                provider,
                tool_runtime,
            )
            .await?
        }
        WorkerMode::Resume => {
            AgentSession::recover(&args.state_root, &args.session_id, provider, tool_runtime)
                .await?
        }
        WorkerMode::Recover => {
            let mut session =
                AgentSession::recover(&args.state_root, &args.session_id, provider, tool_runtime)
                    .await?;
            session.emit_recovery_marker(
                "worker recovered after a crash; transcript restored from disk",
            );
            session
        }
    };
    let session = Arc::new(Mutex::new(session));

    let socket_path = paths::worker_socket_path(&args.state_root, &args.session_id);
    paths::ensure_dir(
        Context::Worker,
        socket_path.parent().expect("socket path has a parent"),
    )?;
    // 20s, not 5s -- see `daemon::run`'s identical bump for `daemon.sock`
    // and its own doc comment for the CI evidence behind the number.
    let mut listener =
        transport::Listener::bind_with_retry(Context::Worker, socket_path, Duration::from_secs(20))
            .await?;

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

async fn handle_private_connection(
    session: Arc<Mutex<AgentSession>>,
    mut conn: LineStream,
) -> Result<()> {
    let request = match conn.read_request(Context::Worker).await? {
        Some(r) => r,
        None => return Ok(()),
    };
    match request {
        Request::Ping => conn.write_response(Context::Worker, &Response::Pong).await,
        Request::SessionAttach { .. } => {
            let (session_id, snapshot, pending_marker, mut events) = {
                let mut guard = session.lock().await;
                (
                    guard.state.session_id.clone(),
                    guard.snapshot_event(),
                    guard.take_pending_recovery_marker(),
                    guard.subscribe(),
                )
            };
            conn.write_response(
                Context::Worker,
                &Response::SessionAttachStarted { session_id },
            )
            .await?;
            conn.write_event(Context::Worker, &snapshot).await?;
            if let Some(marker) = pending_marker {
                conn.write_event(Context::Worker, &marker).await?;
            }
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
            conn.write_response(Context::Worker, &Response::SessionPromptAck { entry })
                .await
        }
        Request::SessionRename { name, .. } => {
            session.lock().await.rename(name.clone()).await?;
            conn.write_response(Context::Worker, &Response::SessionRenameAck { name })
                .await
        }
        Request::GoalUpdate { action, .. } => {
            let goal = session.lock().await.update_goal(action).await?;
            conn.write_response(Context::Worker, &Response::GoalUpdateAck { goal })
                .await
        }
        Request::HarnessUpdate { action, .. } => {
            // Unlike `GoalUpdate` (never fails -- every `GoalAction` is a
            // no-op on a missing goal rather than an error), `Rollback`
            // to an out-of-range history index is a genuine, expected
            // condition that must reach the client as a proper
            // `Response::Error`, not just propagate via `?` and silently
            // drop this connection (the fate of an unhandled `Err` from
            // this whole match, per this function's own doc comment).
            match session.lock().await.update_harness(action).await {
                Ok(state) => {
                    conn.write_response(Context::Worker, &Response::HarnessUpdateAck { state })
                        .await
                }
                Err(err) if err.is_conflict() => {
                    conn.write_response(
                        Context::Worker,
                        &Response::Error {
                            message: err.to_string(),
                            conflict: true,
                        },
                    )
                    .await
                }
                Err(err) => Err(err),
            }
        }
        Request::WorkerShutdown => {
            session.lock().await.mark_stopped().await?;
            conn.write_response(Context::Worker, &Response::WorkerShutdownAck)
                .await?;
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
pub async fn spawn(
    exe_path: &Path,
    state_root: &Path,
    session_id: &str,
    mode: WorkerMode,
    name: Option<String>,
    model: Option<String>,
    goal: Option<String>,
) -> Result<u32> {
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
    if let Some(model) = &model {
        cmd.arg("--model").arg(model);
    }
    if let Some(goal) = &goal {
        cmd.arg("--goal").arg(goal);
    }
    // stderr goes to a log file, same reasoning as `client::daemon_start`'s
    // identical redirect: a worker that panics or exits before binding
    // its private socket would otherwise fail completely silently.
    let session_dir = paths::session_dir(state_root, session_id);
    paths::ensure_dir(Context::Worker, &session_dir)?;
    let log_path = paths::worker_log_path(&session_dir);
    let log_file = std::fs::File::create(&log_path)
        .map_err(|e| HarnessError::io(Context::Worker, Some(log_path), e))?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file));
    procutil::prepare_detached(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| HarnessError::io(Context::Worker, Some(exe_path.to_owned()), e))?;
    let pid = child.id();
    // The worker outlives this *process* by design (`prepare_detached`'s
    // `setsid`/`DETACHED_PROCESS`), but on Unix `setsid` alone does not
    // reparent the child away from this supervisor the way a full
    // double-fork daemonization would -- the kernel still considers the
    // worker this process's child until something here calls `wait` on
    // it, so a worker that dies while the supervisor is still running
    // becomes a zombie under it, not silently reaped. That zombie still
    // answers `kill(pid, 0)` successfully (POSIX: a zombie pid is very
    // much still "alive" for that check), which would make
    // `catalog::effective_status`'s crash detection never fire --
    // `tests/worker_crash_recovery.rs`'s own repro. So the `Child` is
    // handed to a fire-and-forget reaper task instead of dropped: it
    // does nothing but wait, has no effect on "detached" (that's
    // `setsid`'s doing, not whether anything here calls `wait`), and if
    // the *supervisor* is the one that dies first, the worker is simply
    // reparented to init, which reaps it the ordinary way -- this task
    // only ever matters for the worker-dies-first ordering.
    rusty_tokio::spawn(async move {
        let _ = child.wait().await;
    });
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
