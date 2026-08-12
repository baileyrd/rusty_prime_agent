//! Client-side helpers: connect to the daemon's public socket (starting
//! it first if needed), send one [`Request`], and render the
//! [`Response`]/event stream to stdout. This is "a bare one-shot CLI
//! over the same IPC boundary the future TUI will use" (Phase 1
//! non-goal on TUI polish) -- no interactive rendering, just plain text.

use std::path::Path;
use std::time::Duration;

use crate::error::{Context, HarnessError, Result};
use crate::paths;
use crate::procutil;
use crate::protocol::{Request, Response, SessionEvent, SessionStatus};
use crate::transport;

/// Kept strictly larger than `daemon::run`'s own internal
/// `bind_with_retry` budget for `daemon.sock` (20s -- see that call
/// site's doc comment) so this CLI doesn't give up on the supervisor
/// before its own retry loop has had its full chance.
const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// `harness daemon start`: idempotent. If a supervisor is already
/// reachable, reports that and returns; otherwise spawns one detached
/// and waits for its socket to come up.
pub async fn daemon_start(state_root: &Path, exe_path: &Path) -> Result<()> {
    let socket_path = paths::daemon_socket_path(state_root);
    if transport::probe(Context::Daemon, socket_path.clone()).await {
        println!("daemon already running ({})", socket_path.display());
        return Ok(());
    }
    paths::ensure_dir(Context::Daemon, state_root)?;

    use rusty_tokio::process::{Command, Stdio};
    let cwd = std::env::current_dir().map_err(|e| HarnessError::io(Context::Daemon, None, e))?;

    // stderr goes to a log file, not `Stdio::null()`: a detached process
    // has no one to hand a live stderr stream to, but a supervisor that
    // panics or exits before ever binding its socket would otherwise
    // fail completely silently -- the only symptom being `wait_ready`'s
    // own generic timeout below, with zero clue why. `tests/common::
    // daemon_start` reads this back on a failed startup.
    let log_path = paths::daemon_log_path(state_root);
    let log_file = std::fs::File::create(&log_path)
        .map_err(|e| HarnessError::io(Context::Daemon, Some(log_path), e))?;

    let mut cmd = Command::new(exe_path);
    cmd.current_dir(&cwd).arg("__supervisor-main");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file));
    procutil::prepare_detached(&mut cmd);

    let child = cmd
        .spawn()
        .map_err(|e| HarnessError::io(Context::Daemon, Some(exe_path.to_owned()), e))?;
    let pid = child.id();
    drop(child); // detached: the supervisor outlives this CLI invocation.

    transport::wait_ready(Context::Daemon, socket_path.clone(), DAEMON_READY_TIMEOUT).await?;
    println!(
        "daemon started (pid {pid}, socket {})",
        socket_path.display()
    );
    Ok(())
}

async fn connect(state_root: &Path) -> Result<transport::LineStream> {
    let socket_path = paths::daemon_socket_path(state_root);
    transport::connect(Context::Daemon, socket_path)
        .await
        .map_err(|_| {
            HarnessError::conflict(
                Context::Daemon,
                "daemon is not running; run `harness daemon start` first",
            )
        })
}

pub async fn daemon_status(state_root: &Path) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(Context::Daemon, &Request::DaemonStatus)
        .await?;
    match read_response(&mut conn).await? {
        Response::DaemonStatus {
            protocol_version,
            pid,
            generation,
            sessions_active,
        } => {
            println!("daemon: protocol_version={protocol_version} pid={pid} generation={generation} sessions_active={sessions_active}");
            Ok(())
        }
        other => Err(unexpected_response(other)),
    }
}

pub async fn daemon_shutdown(state_root: &Path) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(Context::Daemon, &Request::DaemonShutdown)
        .await?;
    match read_response(&mut conn).await? {
        Response::DaemonShutdownAck => {
            println!("daemon shut down");
            Ok(())
        }
        other => Err(unexpected_response(other)),
    }
}

pub async fn session_new(state_root: &Path, name: Option<String>) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(Context::Daemon, &Request::SessionNew { name })
        .await?;
    match read_response(&mut conn).await? {
        Response::SessionNew { session_id } => {
            println!("{session_id}");
            Ok(())
        }
        other => Err(unexpected_response(other)),
    }
}

pub async fn session_list(state_root: &Path) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(Context::Daemon, &Request::SessionList)
        .await?;
    match read_response(&mut conn).await? {
        Response::SessionList { sessions } => {
            if sessions.is_empty() {
                println!("no sessions");
            }
            for s in sessions {
                let status = match s.status {
                    SessionStatus::Active => "active",
                    SessionStatus::Stopped => "stopped",
                    SessionStatus::Crashed => "crashed",
                };
                let name = s.name.as_deref().unwrap_or("-");
                println!(
                    "{}\t{}\t{}\tturns={}\tupdated_at_ms={}",
                    s.session_id, status, name, s.last_sequence, s.updated_at_ms
                );
            }
            Ok(())
        }
        other => Err(unexpected_response(other)),
    }
}

pub async fn session_attach(state_root: &Path, session_id: String) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(Context::Daemon, &Request::SessionAttach { session_id })
        .await?;
    match read_response(&mut conn).await? {
        Response::SessionAttachStarted { session_id } => {
            println!("attached to {session_id}");
        }
        other => return Err(unexpected_response(other)),
    }
    loop {
        match conn.read_event(Context::Daemon).await? {
            Some(SessionEvent::Snapshot { state, transcript }) => {
                println!(
                    "-- snapshot: generation={} last_sequence={} --",
                    state.generation, state.last_sequence
                );
                for entry in transcript {
                    print_entry(&entry);
                }
            }
            Some(SessionEvent::Turn { entry }) => print_entry(&entry),
            Some(SessionEvent::RecoveryMarker { message, at_ms }) => {
                println!("-- recovered at {at_ms}: {message} --");
            }
            Some(SessionEvent::SessionEnded) => {
                println!("-- session ended --");
                break;
            }
            None => break,
        }
    }
    Ok(())
}

fn print_entry(entry: &crate::protocol::TranscriptEntry) {
    let role = match entry.role {
        crate::protocol::Role::User => "user",
        crate::protocol::Role::Assistant => "assistant",
        crate::protocol::Role::System => "system",
    };
    println!("[{}] {role}: {}", entry.sequence, entry.text);
}

pub async fn session_prompt(state_root: &Path, session_id: String, text: String) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(
        Context::Daemon,
        &Request::SessionPrompt { session_id, text },
    )
    .await?;
    match read_response(&mut conn).await? {
        Response::SessionPromptAck { entry } => {
            print_entry(&entry);
            Ok(())
        }
        other => Err(unexpected_response(other)),
    }
}

/// Response reads get a bounded timeout: a `connect()` that completed
/// but whose peer is gone (e.g. a client racing a supervisor that was
/// just force-killed -- the OS can briefly still complete a connect
/// into a listen queue whose owning process already died, before it
/// finishes tearing the socket down) must not hang this CLI invocation
/// forever waiting for a reply nobody will ever send. Caught by this
/// project's own `tests/supervisor_restart_recovery.rs`.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

async fn read_response(conn: &mut transport::LineStream) -> Result<Response> {
    let response =
        rusty_tokio::time::timeout(RESPONSE_TIMEOUT, conn.read_response(Context::Daemon))
            .await
            .map_err(|_| {
                HarnessError::conflict(Context::Daemon, "daemon did not respond in time")
            })??
            .ok_or_else(|| {
                HarnessError::protocol(
                    Context::Daemon,
                    "daemon closed the connection without responding",
                )
            })?;
    if let Response::Error { message, conflict } = &response {
        return Err(if *conflict {
            HarnessError::conflict(Context::Daemon, message.clone())
        } else {
            HarnessError::protocol(Context::Daemon, message.clone())
        });
    }
    Ok(response)
}

fn unexpected_response(response: Response) -> HarnessError {
    HarnessError::protocol(
        Context::Daemon,
        format!("unexpected response: {response:?}"),
    )
}
