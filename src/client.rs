//! Client-side helpers: connect to the daemon's public socket (starting
//! it first if needed), send one [`Request`], and render the
//! [`Response`]/event stream to stdout. This is "a bare one-shot CLI
//! over the same IPC boundary the future TUI will use" (Phase 1
//! non-goal on TUI polish) -- no interactive rendering, just plain text
//! by default, or raw JSON lines under [`OutputMode::Json`] (parity with
//! `prime-agent --mode json`, see `cli::OutputMode`'s own doc comment for
//! why this reuses `Response`/`SessionEvent` as the JSON vocabulary
//! rather than `prime-agent`'s own richer event schema).

use std::path::Path;
use std::time::Duration;

use crate::cli::OutputMode;
use crate::error::{Context, HarnessError, Result};
use crate::paths;
use crate::procutil;
use crate::protocol::{Request, Response, SessionEvent, SessionStatus};
use crate::transport;

/// `Response`/`SessionEvent` are plain derived-`Serialize` data (strings,
/// numbers, enums, nested structs) with no types that can fail to
/// serialize (no maps with non-string keys, no `f32`/`f64` NaN/Infinity)
/// -- a serialization error here would mean this project's own wire
/// types changed shape incompatibly, a programmer error to surface loudly
/// rather than a runtime condition to route through `Result`.
fn print_json(value: &impl serde::Serialize) {
    println!(
        "{}",
        serde_json::to_string(value).expect("Response/SessionEvent are always serializable")
    );
}

/// Kept strictly larger than `daemon::run`'s own internal
/// `bind_with_retry` budget for `daemon.sock` (20s -- see that call
/// site's doc comment) so this CLI doesn't give up on the supervisor
/// before its own retry loop has had its full chance.
const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Shared by `daemon_start` (which reports what it did) and `print_once`
/// (which stays silent about it -- parity with `prime-agent -p`, whose
/// whole point is "just the answer, nothing else"): idempotent, spawns a
/// detached supervisor and waits for its socket only if one isn't
/// already reachable. `Some(pid)` if a fresh supervisor was spawned,
/// `None` if one was already running.
async fn ensure_daemon_started(state_root: &Path, exe_path: &Path) -> Result<Option<u32>> {
    let socket_path = paths::daemon_socket_path(state_root);
    if transport::probe(Context::Daemon, socket_path.clone()).await {
        return Ok(None);
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

    transport::wait_ready(Context::Daemon, socket_path, DAEMON_READY_TIMEOUT).await?;
    Ok(Some(pid))
}

/// `harness daemon start`: idempotent. If a supervisor is already
/// reachable, reports that and returns; otherwise spawns one detached
/// and waits for its socket to come up.
pub async fn daemon_start(state_root: &Path, exe_path: &Path, mode: OutputMode) -> Result<()> {
    let socket_path = paths::daemon_socket_path(state_root);
    match ensure_daemon_started(state_root, exe_path).await? {
        None => match mode {
            OutputMode::Json => print_json(&serde_json::json!({
                "type": "daemon_already_running",
                "socket": socket_path,
            })),
            OutputMode::Text => println!("daemon already running ({})", socket_path.display()),
        },
        Some(pid) => match mode {
            OutputMode::Json => print_json(&serde_json::json!({
                "type": "daemon_started",
                "pid": pid,
                "socket": socket_path,
            })),
            OutputMode::Text => println!(
                "daemon started (pid {pid}, socket {})",
                socket_path.display()
            ),
        },
    }
    Ok(())
}

/// `harness -p`/`--print`: parity with `prime-agent -p`. Transparently
/// ensures a daemon is running (unlike every other subcommand, which
/// assumes `daemon start` already happened), creates a new, unnamed
/// session, prompts it once, and prints just the reply -- no session id,
/// no daemon-startup noise, no `[seq] role:` prefix -- matching
/// `prime-agent -p`'s own "print response and exit" contract. The
/// session itself is not torn down afterward: it stays reachable via its
/// id the same as any `session new`-created one, for parity with
/// `prime-agent`'s own sessions-are-always-persisted default.
pub async fn print_once(
    state_root: &Path,
    exe_path: &Path,
    text: String,
    model: Option<String>,
    mode: OutputMode,
) -> Result<()> {
    ensure_daemon_started(state_root, exe_path).await?;

    let mut conn = connect(state_root).await?;
    conn.write_request(Context::Daemon, &Request::SessionNew { name: None, model })
        .await?;
    let session_id = match read_response(&mut conn).await? {
        Response::SessionNew { session_id } => session_id,
        other => return Err(unexpected_response(other)),
    };

    let mut conn = connect(state_root).await?;
    conn.write_request(
        Context::Daemon,
        &Request::SessionPrompt { session_id, text },
    )
    .await?;
    match read_response_with_timeout(&mut conn, PROMPT_RESPONSE_TIMEOUT).await? {
        response @ Response::SessionPromptAck { .. } => {
            match (&response, mode) {
                (_, OutputMode::Json) => print_json(&response),
                (Response::SessionPromptAck { entry }, OutputMode::Text) => {
                    println!("{}", entry.text)
                }
                _ => unreachable!(),
            }
            Ok(())
        }
        other => Err(unexpected_response(other)),
    }
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

pub async fn daemon_status(state_root: &Path, mode: OutputMode) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(Context::Daemon, &Request::DaemonStatus)
        .await?;
    let response = read_response(&mut conn).await?;
    match (&response, mode) {
        (Response::DaemonStatus { .. }, OutputMode::Json) => {
            print_json(&response);
            Ok(())
        }
        (
            Response::DaemonStatus {
                protocol_version,
                pid,
                generation,
                sessions_active,
            },
            OutputMode::Text,
        ) => {
            println!("daemon: protocol_version={protocol_version} pid={pid} generation={generation} sessions_active={sessions_active}");
            Ok(())
        }
        _ => Err(unexpected_response(response)),
    }
}

pub async fn daemon_shutdown(state_root: &Path, mode: OutputMode) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(Context::Daemon, &Request::DaemonShutdown)
        .await?;
    match read_response(&mut conn).await? {
        response @ Response::DaemonShutdownAck => {
            match mode {
                OutputMode::Json => print_json(&response),
                OutputMode::Text => println!("daemon shut down"),
            }
            Ok(())
        }
        other => Err(unexpected_response(other)),
    }
}

pub async fn session_new(
    state_root: &Path,
    name: Option<String>,
    model: Option<String>,
    mode: OutputMode,
) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(Context::Daemon, &Request::SessionNew { name, model })
        .await?;
    match read_response(&mut conn).await? {
        response @ Response::SessionNew { .. } => {
            match (&response, mode) {
                (_, OutputMode::Json) => print_json(&response),
                (Response::SessionNew { session_id }, OutputMode::Text) => println!("{session_id}"),
                _ => unreachable!(),
            }
            Ok(())
        }
        other => Err(unexpected_response(other)),
    }
}

pub async fn session_list(state_root: &Path, mode: OutputMode) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(Context::Daemon, &Request::SessionList)
        .await?;
    match read_response(&mut conn).await? {
        response @ Response::SessionList { .. } => {
            if mode == OutputMode::Json {
                print_json(&response);
                return Ok(());
            }
            let Response::SessionList { sessions } = response else {
                unreachable!()
            };
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
                let worker_pid = s
                    .worker_pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "-".to_string());
                let model = s.model.as_deref().unwrap_or("echo");
                println!(
                    "{}\t{}\t{}\tturns={}\tgeneration={}\tworker_pid={}\tmodel={}\tupdated_at_ms={}",
                    s.session_id,
                    status,
                    name,
                    s.last_sequence,
                    s.generation,
                    worker_pid,
                    model,
                    s.updated_at_ms
                );
            }
            Ok(())
        }
        other => Err(unexpected_response(other)),
    }
}

pub async fn session_attach(state_root: &Path, session_id: String, mode: OutputMode) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(Context::Daemon, &Request::SessionAttach { session_id })
        .await?;
    match read_response(&mut conn).await? {
        response @ Response::SessionAttachStarted { .. } => match mode {
            OutputMode::Json => print_json(&response),
            OutputMode::Text => {
                let Response::SessionAttachStarted { session_id } = response else {
                    unreachable!()
                };
                println!("attached to {session_id}");
            }
        },
        other => return Err(unexpected_response(other)),
    }
    loop {
        match conn.read_event(Context::Daemon).await? {
            Some(event) if mode == OutputMode::Json => {
                let ended = matches!(event, SessionEvent::SessionEnded);
                print_json(&event);
                if ended {
                    break;
                }
            }
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

pub async fn session_prompt(
    state_root: &Path,
    session_id: String,
    text: String,
    mode: OutputMode,
) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(
        Context::Daemon,
        &Request::SessionPrompt { session_id, text },
    )
    .await?;
    match read_response_with_timeout(&mut conn, PROMPT_RESPONSE_TIMEOUT).await? {
        response @ Response::SessionPromptAck { .. } => {
            match (&response, mode) {
                (_, OutputMode::Json) => print_json(&response),
                (Response::SessionPromptAck { entry }, OutputMode::Text) => print_entry(entry),
                _ => unreachable!(),
            }
            Ok(())
        }
        other => Err(unexpected_response(other)),
    }
}

pub async fn session_stop(state_root: &Path, session_id: String, mode: OutputMode) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(Context::Daemon, &Request::SessionStop { session_id })
        .await?;
    match read_response(&mut conn).await? {
        response @ Response::SessionStopAck { .. } => {
            match (&response, mode) {
                (_, OutputMode::Json) => print_json(&response),
                (
                    Response::SessionStopAck {
                        already_stopped: true,
                    },
                    OutputMode::Text,
                ) => {
                    println!("session already stopped")
                }
                (
                    Response::SessionStopAck {
                        already_stopped: false,
                    },
                    OutputMode::Text,
                ) => {
                    println!("session stopped")
                }
                _ => unreachable!(),
            }
            Ok(())
        }
        other => Err(unexpected_response(other)),
    }
}

pub async fn session_rename(
    state_root: &Path,
    session_id: String,
    name: Option<String>,
    mode: OutputMode,
) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(
        Context::Daemon,
        &Request::SessionRename { session_id, name },
    )
    .await?;
    match read_response(&mut conn).await? {
        response @ Response::SessionRenameAck { .. } => {
            match (&response, mode) {
                (_, OutputMode::Json) => print_json(&response),
                (Response::SessionRenameAck { name }, OutputMode::Text) => {
                    println!("renamed to {}", name.as_deref().unwrap_or("-"))
                }
                _ => unreachable!(),
            }
            Ok(())
        }
        other => Err(unexpected_response(other)),
    }
}

pub async fn schedule_add(
    state_root: &Path,
    session_id: String,
    text: String,
    kind: crate::protocol::ScheduleKind,
    mode: OutputMode,
) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(
        Context::Daemon,
        &Request::ScheduleAdd {
            session_id,
            text,
            kind,
        },
    )
    .await?;
    match read_response(&mut conn).await? {
        response @ Response::ScheduleAdded { .. } => {
            match (&response, mode) {
                (_, OutputMode::Json) => print_json(&response),
                (Response::ScheduleAdded { schedule_id }, OutputMode::Text) => {
                    println!("{schedule_id}")
                }
                _ => unreachable!(),
            }
            Ok(())
        }
        other => Err(unexpected_response(other)),
    }
}

pub async fn schedule_list(state_root: &Path, session_id: String, mode: OutputMode) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(Context::Daemon, &Request::ScheduleList { session_id })
        .await?;
    match read_response(&mut conn).await? {
        response @ Response::ScheduleList { .. } => {
            if mode == OutputMode::Json {
                print_json(&response);
                return Ok(());
            }
            let Response::ScheduleList { entries } = response else {
                unreachable!()
            };
            if entries.is_empty() {
                println!("no schedules");
            }
            for e in entries {
                let kind = match e.kind {
                    crate::protocol::ScheduleKind::Once { at_ms } => format!("once@{at_ms}"),
                    crate::protocol::ScheduleKind::Every { interval_ms } => {
                        format!("every={interval_ms}ms")
                    }
                };
                println!(
                    "{}\t{}\tnext_fire_ms={}\t{}",
                    e.schedule_id, kind, e.next_fire_ms, e.text
                );
            }
            Ok(())
        }
        other => Err(unexpected_response(other)),
    }
}

pub async fn schedule_cancel(
    state_root: &Path,
    session_id: String,
    schedule_id: String,
    mode: OutputMode,
) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(
        Context::Daemon,
        &Request::ScheduleCancel {
            session_id,
            schedule_id,
        },
    )
    .await?;
    match read_response(&mut conn).await? {
        response @ Response::ScheduleCancelAck { .. } => {
            match (&response, mode) {
                (_, OutputMode::Json) => print_json(&response),
                (Response::ScheduleCancelAck { found: true }, OutputMode::Text) => {
                    println!("schedule canceled")
                }
                (Response::ScheduleCancelAck { found: false }, OutputMode::Text) => {
                    println!("no such schedule")
                }
                _ => unreachable!(),
            }
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

/// `SessionPrompt`'s own response wait, specifically -- every other
/// request this client sends only waits on local IPC (bind a socket,
/// read a state file), but a `SessionPrompt` response isn't sent until
/// `AgentSession::prompt` has a full reply from whatever `ModelProvider`
/// the worker is using. `EchoProvider` answers instantly, but a real
/// backend doesn't: measured against `OllamaProvider` (a tiny model,
/// CPU-only, via a local `rp-server` sidecar -- see `rp_server`'s own
/// doc comment) a single completion took ~29s. `RESPONSE_TIMEOUT` stays
/// tight for everything else so a genuinely dead daemon still fails
/// fast.
const PROMPT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);

async fn read_response(conn: &mut transport::LineStream) -> Result<Response> {
    read_response_with_timeout(conn, RESPONSE_TIMEOUT).await
}

async fn read_response_with_timeout(
    conn: &mut transport::LineStream,
    timeout: Duration,
) -> Result<Response> {
    let response = rusty_tokio::time::timeout(timeout, conn.read_response(Context::Daemon))
        .await
        .map_err(|_| HarnessError::conflict(Context::Daemon, "daemon did not respond in time"))??
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
