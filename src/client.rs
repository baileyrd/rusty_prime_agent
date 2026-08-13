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
use crate::protocol::{
    GoalAction, GoalState, GoalStatus, HarnessAction, HarnessNote, HarnessNoteKind, HarnessState,
    Request, Response, Role, SessionEvent, SessionState, SessionStatus, TranscriptEntry,
};
use crate::termctl;
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
    conn.write_request(
        Context::Daemon,
        &Request::SessionNew {
            name: None,
            model,
            goal: None,
            parent_id: None,
            spawned_from_sequence: None,
            thinking: None,
            tools: None,
            runtime: None,
        },
    )
    .await?;
    let session_id = match read_response(&mut conn).await? {
        Response::SessionNew { session_id } => session_id,
        other => return Err(unexpected_response(other)),
    };

    let mut conn = connect(state_root).await?;
    conn.write_request(
        Context::Daemon,
        &Request::SessionPrompt {
            session_id,
            text,
            images: None,
        },
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

/// Shared by [`session_new`] and [`session_spawn`] -- the latter needs
/// the raw id, not printed text, and sets `parent_id` itself. Takes the
/// same `NewSessionMeta` bundle `AgentSession::create`/`worker::spawn`
/// already use rather than one parameter per field, both to stay under
/// clippy's `too_many_arguments` and because it's the same "this argument
/// list keeps growing every time a new `session new`-seedable field is
/// added" problem `NewSessionMeta`'s own doc comment already names.
async fn create_session(state_root: &Path, meta: crate::session::NewSessionMeta) -> Result<String> {
    let crate::session::NewSessionMeta {
        name,
        model,
        goal,
        parent_id,
        thinking,
        tools,
        runtime,
        // Resolved server-side by `daemon::handle_session_new`, not part
        // of the `Request::SessionNew` wire shape -- see that field's own
        // doc comment on `protocol::SessionState`.
        rlm_depth: _,
        rlm_max_depth: _,
        // No `client.rs` caller ever sets this (only `session::
        // AgentSession::handle_rlm_run`'s own, separate `Request::
        // SessionNew` composition does) -- forwarded rather than
        // hardcoded so this stays correct if that ever changes.
        spawned_from_sequence,
    } = meta;
    let mut conn = connect(state_root).await?;
    conn.write_request(
        Context::Daemon,
        &Request::SessionNew {
            name,
            model,
            goal,
            parent_id,
            spawned_from_sequence,
            thinking,
            tools,
            runtime,
        },
    )
    .await?;
    match read_response(&mut conn).await? {
        Response::SessionNew { session_id } => Ok(session_id),
        other => Err(unexpected_response(other)),
    }
}

pub async fn session_new(
    state_root: &Path,
    meta: crate::session::NewSessionMeta,
    mode: OutputMode,
) -> Result<()> {
    let session_id = create_session(state_root, meta).await?;
    match mode {
        OutputMode::Json => print_json(&Response::SessionNew { session_id }),
        OutputMode::Text => println!("{session_id}"),
    }
    Ok(())
}

/// Shared by [`session_list`] and [`session_spawn`]/[`session_children`]/
/// [`session_message`] -- the latter three need the raw summaries (to
/// read a parent's `model`, filter by `parent_id`, or validate a
/// parent/child relationship), not printed text.
async fn fetch_sessions(state_root: &Path) -> Result<Vec<crate::protocol::SessionSummary>> {
    let mut conn = connect(state_root).await?;
    conn.write_request(Context::Daemon, &Request::SessionList)
        .await?;
    match read_response(&mut conn).await? {
        Response::SessionList { sessions } => Ok(sessions),
        other => Err(unexpected_response(other)),
    }
}

pub async fn session_list(state_root: &Path, mode: OutputMode) -> Result<()> {
    let sessions = fetch_sessions(state_root).await?;
    match mode {
        OutputMode::Json => print_json(&Response::SessionList { sessions }),
        OutputMode::Text => {
            if sessions.is_empty() {
                println!("no sessions");
            }
            for s in sessions {
                print_session_summary_line(&s);
            }
        }
    }
    Ok(())
}

fn print_session_summary_line(s: &crate::protocol::SessionSummary) {
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

/// Bounded, non-Python parity with `prime-agent`'s recursive subagents
/// (`rlm(...)`, `packages/coding-agent/docs/rlm.md`): "The TypeScript
/// host creates a normal child `AgentSession` with an independent
/// context and session directory" -- the actual mechanism this project
/// already has via `session new`, just with `parent_id` recorded and
/// the parent's `model` inherited unless `--model` overrides it (parity
/// with "the child inherits the parent model... unless the call
/// requests another configured model"). Skills/tools/retry-policy
/// inheritance from that same sentence don't apply here -- this
/// project's tool runtime is `NoopToolRuntime` and it has no
/// retry-policy concept to inherit.
///
/// "Returns immediately after task admission with a child handle; it
/// never waits for or returns the child's answer": the task text is
/// enqueued as a near-immediate one-shot schedule (`ScheduleKind::
/// Once`) rather than sent as a blocking `SessionPrompt`, reusing the
/// daemon's existing background schedule-firing loop (`schedule`'s own
/// module doc comment) as the async dispatch mechanism this project
/// already has, rather than inventing a new one. `session message` is
/// the analog of `agent_message.send`.
pub async fn session_spawn(
    state_root: &Path,
    parent_id: String,
    task: String,
    model: Option<String>,
    name: Option<String>,
    mode: OutputMode,
) -> Result<()> {
    let model = match model {
        Some(m) => Some(m),
        None => {
            let sessions = fetch_sessions(state_root).await?;
            sessions
                .into_iter()
                .find(|s| s.session_id == parent_id)
                .ok_or_else(|| {
                    HarnessError::conflict(
                        Context::Daemon,
                        format!("unknown parent session {parent_id}"),
                    )
                })?
                .model
        }
    };
    let child_id = create_session(
        state_root,
        crate::session::NewSessionMeta {
            name,
            model,
            goal: None,
            parent_id: Some(parent_id),
            thinking: None,
            tools: None,
            runtime: None,
            rlm_depth: None,
            rlm_max_depth: None,
            // `session spawn` (this function) is CLI-level admission, not
            // `rlm(...)` -- see `protocol::SessionState::
            // spawned_from_sequence`'s own doc comment for why only the
            // latter ever sets this.
            spawned_from_sequence: None,
        },
    )
    .await?;
    add_schedule(
        state_root,
        child_id.clone(),
        task,
        crate::protocol::ScheduleKind::Once {
            at_ms: paths::now_ms(),
        },
    )
    .await?;
    match mode {
        OutputMode::Json => print_json(&serde_json::json!({
            "type": "session_spawned",
            "session_id": child_id,
        })),
        OutputMode::Text => println!("{child_id}"),
    }
    Ok(())
}

/// `session children <id>` -- direct children only (`parent_id ==
/// id`), read straight off `session list`'s own summaries rather than
/// needing a dedicated request.
pub async fn session_children(
    state_root: &Path,
    parent_id: String,
    mode: OutputMode,
) -> Result<()> {
    let children: Vec<_> = fetch_sessions(state_root)
        .await?
        .into_iter()
        .filter(|s| s.parent_id.as_deref() == Some(parent_id.as_str()))
        .collect();
    match mode {
        OutputMode::Json => print_json(&children),
        OutputMode::Text => {
            if children.is_empty() {
                println!("no children");
            }
            for s in &children {
                print_session_summary_line(s);
            }
        }
    }
    Ok(())
}

/// Parity with `agent_message.send(msg, receiver_role="parent"|
/// "child")`: only a session's own parent or one of its own children is
/// a valid target, validated here against `session list`'s own
/// `parent_id` field -- this project's whole trust model is a single
/// local caller (the same reasoning `session_autonomous`'s
/// `--quality-gate` shell command already leans on), so this doesn't
/// need server-side enforcement of its own. Delivered as an ordinary,
/// visible `SessionPrompt`, prefixed with the sender's id so the
/// recipient's transcript makes clear where it came from -- `agent_
/// message`'s replies "arrive only through explicit... replies," never
/// silently, and this keeps that same visibility.
pub async fn session_message(
    state_root: &Path,
    from_id: String,
    to_id: String,
    text: String,
    mode: OutputMode,
) -> Result<()> {
    let sessions = fetch_sessions(state_root).await?;
    let from = sessions
        .iter()
        .find(|s| s.session_id == from_id)
        .ok_or_else(|| {
            HarnessError::conflict(Context::Daemon, format!("unknown session {from_id}"))
        })?;
    let to = sessions
        .iter()
        .find(|s| s.session_id == to_id)
        .ok_or_else(|| {
            HarnessError::conflict(Context::Daemon, format!("unknown session {to_id}"))
        })?;
    let is_parent = from.parent_id.as_deref() == Some(to_id.as_str());
    let is_child = to.parent_id.as_deref() == Some(from_id.as_str());
    if !is_parent && !is_child {
        return Err(HarnessError::conflict(
            Context::Daemon,
            format!("{to_id} is neither the parent nor a child of {from_id}"),
        ));
    }
    let prefixed = format!("[from {from_id}] {text}");
    let entry = send_prompt(state_root, &to_id, prefixed).await?;
    match mode {
        OutputMode::Json => print_json(&Response::SessionPromptAck { entry }),
        OutputMode::Text => print_entry(&entry),
    }
    Ok(())
}

/// `harness session rpc <id>` -- parity with `prime-agent --mode rpc`:
/// headless, embeddable operation over stdin/stdout, one JSON object per
/// line each direction. Unlike `prime-agent`'s own ~30-command custom
/// protocol (`packages/coding-agent/docs/rpc.md`, by its own words "not
/// JSON-RPC 2.0"), this reuses the wire protocol's own `Request`/
/// `Response`/`SessionEvent` types directly as the RPC vocabulary --
/// the same "don't invent a second JSON schema" choice `cli::
/// OutputMode::Json` already made for `--mode json` (see that type's own
/// doc comment). Two concurrent lanes share one stdout, serialized
/// through `stdout_lock` so a line from one never interleaves with a
/// line from the other:
///
/// - The initial attach (`Response::SessionAttachStarted` plus the
///   snapshot event) happens synchronously, before the stdin loop even
///   starts -- deliberately, so it's always the first line printed
///   regardless of how quickly stdin closes, rather than racing a
///   background task that might not have run yet. The same connection
///   then moves into a background task (`forward_events`) that keeps
///   streaming every subsequent `SessionEvent` as its own JSON line --
///   the exact same event stream `--mode json session attach` produces,
///   just delivered automatically instead of requiring a second CLI
///   invocation. Anything after the initial snapshot is genuinely
///   concurrent with the stdin loop below -- see the grace-window
///   comment at the end of this function for a real race CI caught here
///   and how it's closed for the common case.
/// - The foreground loop reads one line at a time from stdin (each read
///   wrapped in its own `spawn_blocking` call, not one long-lived
///   blocking task, so the loop stays fully `.await`-able between
///   reads), parses it as a `Request`, dispatches it to the daemon over
///   an ordinary one-shot connection (`dispatch_one_shot`), and prints
///   the resulting `Response` as its own JSON line.
///
/// Unlike `prime-agent`'s RPC surface (deliberately session/agent-scoped
/// commands only), any `Request` variant is accepted here -- consistent
/// with this project's blanket single-local-caller trust model (see
/// `PARITY.md`), not a narrower allowlist this project would have to
/// invent and maintain. `Request::SessionAttach` is rejected locally
/// (not forwarded) since this mode already streams that session's
/// events automatically; sending it again would open a second,
/// redundant streaming connection `dispatch_one_shot` isn't built to
/// drain (it reads exactly one response line, matching every other
/// one-shot client call in this file). Ends at stdin EOF, same
/// convention `session_repl` already uses -- the process then exits
/// (`main`'s own `std::process::exit`), which is what actually tears
/// down the background event-forwarding task; no explicit cancellation
/// needed.
pub async fn session_rpc(state_root: &Path, session_id: String) -> Result<()> {
    let stdout_lock = std::sync::Arc::new(rusty_tokio::sync::Mutex::new(()));

    let mut events_conn = connect(state_root).await?;
    events_conn
        .write_request(Context::Daemon, &Request::SessionAttach { session_id })
        .await?;
    match read_response(&mut events_conn).await? {
        response @ Response::SessionAttachStarted { .. } => print_json(&response),
        other => return Err(unexpected_response(other)),
    }
    let events_lock = stdout_lock.clone();
    rusty_tokio::spawn(async move {
        let _ = forward_events(events_conn, events_lock).await;
    });

    loop {
        let line = rusty_tokio::spawn_blocking(|| {
            let mut buf = String::new();
            match std::io::stdin().read_line(&mut buf) {
                Ok(0) | Err(_) => None,
                Ok(_) => Some(buf),
            }
        })
        .await
        .unwrap_or(None);
        let Some(line) = line else { break };
        let text = line.trim();
        if text.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Request>(text) {
            Ok(Request::SessionAttach { .. }) => Response::Error {
                message: "session attach is redundant in rpc mode -- this session's events \
                          are already streamed automatically"
                    .to_string(),
                conflict: false,
            },
            Ok(request) => dispatch_one_shot(state_root, request)
                .await
                .unwrap_or_else(|e| Response::Error {
                    message: e.to_string(),
                    conflict: false,
                }),
            Err(e) => Response::Error {
                message: format!("invalid command JSON: {e}"),
                conflict: false,
            },
        };
        let _guard = stdout_lock.lock().await;
        print_json(&response);
    }

    // A bounded grace window before actually exiting, real race caught
    // in CI: by the time a command's own `Response` was printed above,
    // any `SessionEvent`s it produced are already sitting on the
    // background lane's own socket -- the worker broadcasts them
    // synchronously as part of the very call that produced the response
    // (see `session::AgentSession::append`). So the remaining race isn't
    // provider/network latency, it's purely whether *this process's own*
    // background task has been scheduled yet to read and print them --
    // with a single piped command, stdin can hit EOF and this function
    // can return before that ever happens. This sleep gives it a real
    // chance to run first, closing the common single-command case
    // deterministically; an event from something other than a
    // just-dispatched command (a concurrent schedule firing, another
    // attached client's own prompt) can still race process exit, an
    // honest limitation no fixed grace window fully closes.
    rusty_tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(())
}

/// Sends one [`Request`] to an already-running daemon and returns its
/// typed [`Response`] -- re-exported at the crate root
/// (`rusty_prime_agent::dispatch_one_shot`) as this project's "drive a
/// running daemon" embedding primitive (see `lib.rs`'s own doc comment).
/// Everything else in this module stays crate-internal on purpose: every
/// other `client::session_*`/`client::daemon_*` function renders its
/// result straight to this process's own stdout (`println!`/
/// `print_json`) rather than returning it, which makes sense for a CLI
/// binary and no sense at all for an external caller embedding this
/// crate as a library. Uses the daemon's already-running socket
/// (`connect`) -- an embedder wanting no daemon at all should use
/// [`crate::session::AgentSession`] directly instead.
pub async fn dispatch_one_shot(state_root: &Path, request: Request) -> Result<Response> {
    let mut conn = connect(state_root).await?;
    conn.write_request(Context::Daemon, &request).await?;
    read_response_with_timeout(&mut conn, PROMPT_RESPONSE_TIMEOUT).await
}

/// Continues an already-attached connection (the initial
/// `SessionAttachStarted`/snapshot was already handled synchronously by
/// `session_rpc` before spawning this) -- streams every subsequent
/// `SessionEvent` as its own JSON line until the session ends or the
/// connection closes.
async fn forward_events(
    mut conn: transport::LineStream,
    stdout_lock: std::sync::Arc<rusty_tokio::sync::Mutex<()>>,
) -> Result<()> {
    while let Some(event) = conn.read_event(Context::Daemon).await? {
        let ended = matches!(event, SessionEvent::SessionEnded);
        {
            let _guard = stdout_lock.lock().await;
            print_json(&event);
        }
        if ended {
            break;
        }
    }
    Ok(())
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
        crate::protocol::Role::Tool => "tool",
    };
    // A tool-call-request entry (`Role::Assistant`, `tool_calls: Some`)
    // has no user-visible `text` of its own -- show which tools were
    // requested instead of an empty line.
    if let Some(calls) = &entry.tool_calls {
        let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
        println!(
            "[{}] {role}: <requested tool call(s): {}>",
            entry.sequence,
            names.join(", ")
        );
        return;
    }
    println!("[{}] {role}: {}", entry.sequence, entry.text);
}

/// `harness session repl <id>` -- a minimal, non-Python analog of
/// `prime-agent`'s interactive TUI: reads lines from stdin, sends each
/// as an ordinary `SessionPrompt`, and prints the reply, until stdin
/// hits EOF or a line is exactly `/exit`/`/quit`. None of the TUI's own
/// editor/message-queue features (file reference, image paste, steering
/// vs. follow-up queuing, `/tree`/`/fork`/`/clone`/`/export`/`/share`)
/// -- those stay unimplemented, see `PARITY.md`. `/compact
/// [instructions]` (see below) is implemented, since it's a session-level
/// mutation like `/heartbeat`, not one of the TUI's own editor features.
/// Replays the
/// session's existing transcript first (reusing
/// [`fetch_transcript_snapshot`]), so resuming a session in the REPL
/// shows its prior turns the same way `session attach` would.
///
/// Reads stdin with a blocking `std::io::BufRead` loop rather than an
/// async one -- there is nothing else for this one-shot CLI process to
/// make progress on while waiting for the next line, the same reasoning
/// every other blocking `std::fs` call in this crate already leans on.
pub async fn session_repl(state_root: &Path, session_id: String, mode: OutputMode) -> Result<()> {
    let transcript = fetch_transcript_snapshot(state_root, &session_id).await?;
    match mode {
        OutputMode::Json => print_json(&serde_json::json!({
            "type": "repl_snapshot",
            "transcript": transcript,
        })),
        OutputMode::Text => {
            for entry in &transcript {
                print_entry(entry);
            }
        }
    }

    // Set by `/file <path>`, consumed (and cleared) by the next line that
    // actually sends a prompt -- a REPL-only, bounded slice of
    // `prime-agent`'s TUI-side "file reference" feature: no client-side
    // editor/attachment UI, just reading a local file and folding its
    // content into the next real prompt's text, the same "no new
    // subsystem, reuse what `send_prompt` already does" shape `/compact`
    // above uses. Left queued (not sent immediately) across an
    // intervening `/heartbeat`/`/compact`/`/fork` line, so `/file`
    // followed by one of those doesn't silently drop it.
    let mut pending_file_content: Option<String> = None;
    // Same queuing shape as `pending_file_content`, for `/file <path>`
    // when `<path>` names an image instead of a text file -- parity with
    // a bounded slice of `prime-agent`'s image-paste feature, see
    // `PARITY.md`'s own "Interactive TUI: image paste support" entry.
    let mut pending_images: Vec<String> = Vec::new();

    // Raw mode when connected to a real interactive terminal (parity
    // with `prime-agent`'s interactive TUI's own foundation -- see
    // `termctl`'s own module doc comment and `PARITY.md`'s "Interactive
    // TUI: raw-mode rendering foundation" entry) -- every one of this
    // project's own tests pipes stdin/stdout, so `termctl::is_tty()`
    // reports `false` there and the loop below falls through to the
    // exact same `read_line`-based behavior `BufRead::lines()` gave
    // before this increment, unchanged. A `RawModeGuard::enable()`
    // failure (e.g. some other, unanticipated non-terminal fd shape)
    // degrades the same way: silently fall back to cooked-mode
    // reading rather than treat a terminal-control failure as fatal to
    // an otherwise-working REPL.
    let raw_guard = if termctl::is_tty() {
        termctl::RawModeGuard::enable().ok()
    } else {
        None
    };
    let raw_active = raw_guard.is_some();

    // Parity with a bounded slice of `prime-agent`'s TUI-side "steering
    // vs. follow-up queuing" -- see `PARITY.md`'s own "Interactive TUI:
    // steering vs. follow-up message queue" entry for the full story of
    // what's here and what deliberately isn't. Reading stdin used to be
    // fully synchronous with sending a prompt: read one line, `.await`
    // the daemon's full reply, print it, read the next line -- so there
    // was never a window during which a second line could even be read.
    // Now the line reader lives on its own persistent background task
    // (`rusty_tokio::spawn_blocking`, looping internally rather than one
    // fresh spawn per line -- `session_rpc`'s own stdin-reading loop can
    // afford a fresh `spawn_blocking` per line because it never races
    // that read against anything else; this loop does, so a persistent
    // reader is required -- racing a *fresh* blocking read against an
    // in-flight prompt on every iteration would risk two concurrent
    // blocking reads on the same fd if the prompt happened to finish
    // first and the read was abandoned mid-flight), feeding lines into
    // `line_rx` as they arrive. The main loop below races that channel
    // against whatever prompt is currently in flight (`current`, at most
    // one at a time) using `rusty_tokio::select!`: a line that arrives
    // while nothing is in flight is dispatched immediately, the same as
    // before; a line that arrives while a prompt is still generating is
    // queued (`queue`) and dispatched, in order, once the in-flight
    // prompt's reply lands -- "follow-up queuing." Only ordinary prompt
    // sends run concurrently with reading; slash commands (all of which
    // are typically-fast local operations, or REPL wiring around another
    // `client::session_*` function's own already-serial network round
    // trip) still execute synchronously once dispatched, unchanged --
    // widening every one of them to also run concurrently with an
    // in-flight prompt is a larger surface than this increment covers,
    // see `PARITY.md`'s own "full slash-command surface" entry.
    //
    // Deliberately *not* attempted here: "steering" -- interrupting an
    // already-in-flight prompt instead of queuing a follow-up behind it.
    // Investigated, not assumed: there is no cancellation primitive
    // anywhere in this project's protocol/daemon/worker layers today (no
    // `Request::SessionInterrupt`, nothing a client could send to abort
    // a `session.lock().await`-held `prompt()` call already running
    // server-side) -- see `PARITY.md`'s own "cancel primitive" entry for
    // where that's tracked. Dropping the client-side `JoinHandle` for an
    // in-flight send wouldn't cancel the daemon's own work; the worker
    // would still process it and could append a stray, unrequested reply
    // to the transcript later, incoherent with whatever the "steering"
    // message produced instead. Building a real interrupt primitive is a
    // separate, larger piece of work this increment doesn't take on.
    //
    // `stdout_lock` (an `Arc<std::sync::Mutex<()>>`, not `rusty_tokio::
    // sync::Mutex` -- the reader lives on a `spawn_blocking` thread, a
    // genuinely synchronous context that can't `.await` an async lock)
    // guards every write `read_raw_line` makes against the main task's
    // own output for a prompt that finishes (or gets queued) while the
    // reader is concurrently mid-echo of the next line's keystrokes --
    // the same shared-lock shape `session_rpc`'s own `stdout_lock`/
    // `forward_events` already established for an analogous concurrent-
    // writer problem. Bounded, not exhaustive: it covers this
    // increment's own two new output points (the "queued" notice and a
    // finished reply's print) plus every `read_raw_line` write, but does
    // *not* reach into `session_compact`/`session_fork`/`session_tree`/
    // etc.'s own internal `println!`/`print_json` calls (those are
    // shared with the plain top-level, non-REPL CLI commands, which have
    // no such lock and no reason to need one) -- a command's own output
    // can still, in principle, interleave with live keystroke echo of
    // whatever's typed immediately afterward, now that the reader runs
    // continuously in the background regardless of what the main task is
    // doing. Stated honestly rather than silently left uncovered: closing
    // that residual gap would mean threading a lock through every shared
    // client function's own print calls, a materially larger change than
    // this increment's own scope.
    let stdout_lock = std::sync::Arc::new(std::sync::Mutex::new(()));
    let (line_tx, mut line_rx) =
        rusty_tokio::sync::mpsc::unbounded_channel::<Result<Option<String>>>();
    {
        let stdout_lock = stdout_lock.clone();
        rusty_tokio::spawn_blocking(move || {
            let stdin = std::io::stdin();
            loop {
                let result = next_repl_line(&stdin, raw_active, &stdout_lock);
                let keep_going = matches!(result, Ok(Some(_)));
                if line_tx.send(result).is_err() || !keep_going {
                    break;
                }
            }
        });
    }

    // What woke the loop up: either the in-flight prompt completed, or
    // another line arrived from the reader. Branches inside `rusty_tokio
    // ::select!` below compute one of these and nothing else -- no
    // mutation of any outer `let mut` state from inside a branch body.
    // `rusty_tokio::select!` expands each branch into one shared, `move`
    // `poll_fn` closure (see that macro's own module doc comment); a
    // `move` closure captures every referenced outer variable *by
    // value*, not merely by the reference its usage would otherwise
    // need, so any outer `Vec`/`bool`/`Option` mutated directly inside a
    // branch would just be silently lost once that one-shot closure is
    // dropped at the end of the `.await` -- and referencing the same
    // outer variable again on the *next* loop iteration would fail to
    // compile outright ("borrow of moved value"), since it was already
    // moved into the previous iteration's closure. Computing a plain
    // value here and doing every mutation afterward, in ordinary code
    // outside the macro, sidesteps both problems.
    enum Wake {
        PromptFinished(Result<crate::protocol::TranscriptEntry>),
        Reader(Result<Option<String>>),
    }

    let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut current: Option<rusty_tokio::JoinHandle<Result<crate::protocol::TranscriptEntry>>> =
        None;
    let mut reader_done = false;

    loop {
        if let Some(handle) = current.as_mut() {
            // A fresh reborrow each iteration, referenced by name (not
            // `line_rx` itself) inside the macro below for the same
            // move-capture reason `Wake` exists -- see that enum's own
            // doc comment. `line_rx` is used again on later iterations
            // (both here and in the idle branch further down), so the
            // actual receiver must never itself be moved into a
            // select!-generated closure.
            let line_rx_ref = &mut line_rx;
            let wake = rusty_tokio::select! {
                joined = handle => {
                    Wake::PromptFinished(
                        joined
                            .map_err(|e| {
                                HarnessError::protocol(
                                    Context::Cli,
                                    format!("prompt task did not complete: {e}"),
                                )
                            })
                            .and_then(|inner| inner),
                    )
                },
                received = line_rx_ref.recv() => {
                    Wake::Reader(received.unwrap_or(Ok(None)))
                },
            };
            match wake {
                Wake::PromptFinished(result) => {
                    current = None;
                    let entry = result?;
                    let _guard = stdout_lock.lock().unwrap();
                    match mode {
                        OutputMode::Json => print_json(&Response::SessionPromptAck { entry }),
                        OutputMode::Text => print_entry(&entry),
                    }
                }
                Wake::Reader(Ok(Some(l))) => {
                    if !l.trim().is_empty() {
                        let _guard = stdout_lock.lock().unwrap();
                        println!("(queued -- will run once the current reply finishes)");
                        drop(_guard);
                        queue.push_back(l);
                    }
                }
                Wake::Reader(Ok(None)) => reader_done = true,
                Wake::Reader(Err(e)) => return Err(e),
            }
            continue;
        }

        let line = if let Some(l) = queue.pop_front() {
            l
        } else if reader_done {
            break;
        } else {
            match line_rx.recv().await {
                Some(Ok(Some(l))) => l,
                Some(Ok(None)) | None => {
                    reader_done = true;
                    continue;
                }
                Some(Err(e)) => return Err(e),
            }
        };

        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        if text == "/exit" || text == "/quit" {
            break;
        }
        if text == "/heartbeat" {
            // Parity with `prime-agent`'s `/heartbeat` -- a manual,
            // immediate entry point into the same "continue toward the
            // goal" re-entry `session schedule`/`session autonomous`
            // already cover; see `session::HEARTBEAT_MARKER`'s own doc
            // comment for `rlm_heartbeat()`, the kernel-callable sibling
            // of this. Unlike that sibling (called from inside a
            // still-in-flight `prompt()` call, so it has to go through
            // the daemon's async schedule-firing loop instead), this is
            // a fresh top-level REPL action -- free to just send the
            // continuation prompt immediately, same as any other line
            // typed here, no scheduling indirection needed.
            match fetch_goal(state_root, &session_id).await? {
                Some(goal) if goal.status == GoalStatus::Active => {
                    let continue_text = format!("Continue working toward the goal: {}", goal.text);
                    let entry = send_prompt(state_root, &session_id, continue_text).await?;
                    match mode {
                        OutputMode::Json => print_json(&Response::SessionPromptAck { entry }),
                        OutputMode::Text => print_entry(&entry),
                    }
                }
                _ => println!(
                    "no active goal -- set one with `session goal set {session_id} <text...>` first"
                ),
            }
            continue;
        }
        if let Some(duration_str) = text
            .strip_prefix("/heartbeat every ")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            // Parity with `prime-agent /heartbeat every <duration>` --
            // unlike the plain `/heartbeat` above, a repeating heartbeat
            // is a real, standing re-entry into the session, not a single
            // "send it now" action, so this registers an actual
            // `ScheduleKind::Every` schedule (reusing `schedule_add`'s
            // own plumbing/output shape) rather than sending anything
            // immediately -- the same recurring-fire support `session
            // schedule add --every` already has, not a second mechanism.
            // Listed/canceled the same way any other schedule is
            // (`session schedule list`/`cancel`), no bespoke management
            // surface needed.
            match fetch_goal(state_root, &session_id).await? {
                Some(goal) if goal.status == GoalStatus::Active => {
                    match crate::cli::parse_duration_ms(duration_str) {
                        Ok(interval_ms) => {
                            let continue_text =
                                format!("Continue working toward the goal: {}", goal.text);
                            schedule_add(
                                state_root,
                                session_id.clone(),
                                continue_text,
                                crate::protocol::ScheduleKind::Every { interval_ms },
                                mode,
                            )
                            .await?;
                        }
                        Err(e) => println!("{e}"),
                    }
                }
                _ => println!(
                    "no active goal -- set one with `session goal set {session_id} <text...>` first"
                ),
            }
            continue;
        }
        if text == "/compact" || text.starts_with("/compact ") {
            // Parity with `prime-agent /compact [instructions]`. A fresh
            // top-level REPL action, same as `/heartbeat` above -- no
            // reentrancy hazard here, so this calls `session_compact`
            // directly rather than needing any scheduling indirection.
            let instructions = text
                .strip_prefix("/compact")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            session_compact(state_root, session_id.clone(), instructions, mode).await?;
            continue;
        }
        if let Some(path) = text
            .strip_prefix("/file ")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            // Parity with a bounded slice of `prime-agent`'s TUI-side
            // file-reference feature -- see this loop's own
            // `pending_file_content` doc comment above. An image path
            // (`image_mime_type`'s own extension list) queues into
            // `pending_images` instead -- see `pending_images`'s own doc
            // comment -- since image bytes can't be inlined as text the
            // way a text file's content can.
            if let Some(data_uri) = load_image_as_data_uri(path) {
                println!("queued {path} as an image -- included in your next prompt");
                pending_images.push(data_uri);
            } else {
                match std::fs::read_to_string(path) {
                    Ok(content) => {
                        println!(
                            "queued {path} ({} bytes) -- included in your next prompt",
                            content.len()
                        );
                        pending_file_content = Some(format!("--- {path} ---\n{content}\n---\n\n"));
                    }
                    Err(e) => println!("failed to read {path}: {e}"),
                }
            }
            continue;
        }
        if text == "/fork" || text.starts_with("/fork ") {
            // Parity with a bounded slice of `prime-agent`'s TUI-side
            // `/fork` -- `session fork` itself already exists as a
            // top-level `harness session fork <id>` command (see
            // `protocol::Request::SessionFork`'s own doc comment for the
            // full design and what it deliberately doesn't deliver);
            // this is just wiring the same client-side call into the
            // REPL loop, the identical shape `/compact` above already
            // has for `session compact`.
            let rest = text.strip_prefix("/fork").unwrap_or("").trim();
            match parse_repl_fork_args(rest) {
                Ok((at_sequence, name)) => {
                    session_fork(state_root, session_id.clone(), at_sequence, name, mode).await?;
                }
                Err(e) => println!("{e}"),
            }
            continue;
        }
        if text == "/tree" {
            // Parity with a bounded slice of `prime-agent`'s TUI-side
            // `/tree` visualization -- `session tree` itself already
            // exists as a top-level `harness session tree <id>` command
            // (see `client::session_tree`'s own doc comment); this is
            // just wiring the same client-side call into the REPL loop,
            // the identical shape `/compact`/`/fork` above already have.
            session_tree(state_root, session_id.clone(), mode).await?;
            continue;
        }
        if let Some(sequence_str) = text
            .strip_prefix("/tree ")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            // Parity with `prime-agent`'s `/tree` navigation half:
            // `/tree <sequence>` switches this session's active leaf
            // instead of just displaying the tree, the same "one command
            // name, display with no argument, act with one" shape a
            // bounded REPL slice can afford without a real interactive
            // picker (see `PARITY.md`'s intra-session-branching entry for
            // why that picker itself stays out of scope).
            match sequence_str.parse::<u64>() {
                Ok(sequence) => {
                    session_set_active_leaf(state_root, session_id.clone(), sequence, mode).await?;
                }
                Err(_) => println!("`/tree <sequence>` requires an integer, got {sequence_str:?}"),
            }
            continue;
        }
        if let Some(sequence_str) = text
            .strip_prefix("/branch-summary ")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            // Parity with `session-format.md`'s `BranchSummaryEntry` --
            // `session branch-summary` itself already exists as a
            // top-level `harness session branch-summary <id> <sequence>`
            // command; this is just wiring the same client-side call
            // into the REPL loop, the identical shape `/tree`/`/fork`
            // above already have.
            match sequence_str.parse::<u64>() {
                Ok(branch_leaf_sequence) => {
                    session_branch_summarize(
                        state_root,
                        session_id.clone(),
                        branch_leaf_sequence,
                        mode,
                    )
                    .await?;
                }
                Err(_) => println!(
                    "`/branch-summary <sequence>` requires an integer, got {sequence_str:?}"
                ),
            }
            continue;
        }
        if let Some(path) = text
            .strip_prefix("/export ")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            // Parity with a bounded slice of `prime-agent`'s TUI-side
            // `/export` -- writes this session's current transcript
            // (fetched fresh, so it reflects everything sent so far in
            // this REPL run) to a local file as pretty-printed JSON.
            // `/share` (sending it somewhere hosted) stays out of scope:
            // this project has no hosted destination to send it to, the
            // same "nothing on the other end" shape `/login` has.
            let transcript = fetch_transcript_snapshot(state_root, &session_id).await?;
            match serde_json::to_string_pretty(&transcript) {
                Ok(json) => match std::fs::write(path, json) {
                    Ok(()) => println!("exported {} turn(s) to {path}", transcript.len()),
                    Err(e) => println!("failed to write {path}: {e}"),
                },
                Err(e) => println!("failed to serialize transcript: {e}"),
            }
            continue;
        }
        let text_to_send = match pending_file_content.take() {
            Some(prefix) => format!("{prefix}{text}"),
            None => text.to_string(),
        };
        let (text_to_send, at_images) = expand_at_references(&text_to_send);
        let mut images = std::mem::take(&mut pending_images);
        images.extend(at_images);
        let images = if images.is_empty() {
            None
        } else {
            Some(images)
        };
        // Spawned rather than `.await`ed directly -- this is the one
        // dispatch branch that runs concurrently with reading further
        // lines, per this function's own `stdout_lock` doc comment
        // above. `current` being `Some` is what puts the top of this
        // loop into its "busy" (queue-while-generating) mode next
        // iteration.
        let owned_root = state_root.to_path_buf();
        let owned_session_id = session_id.clone();
        current = Some(rusty_tokio::spawn(async move {
            send_prompt_with_images(&owned_root, &owned_session_id, text_to_send, images).await
        }));
    }
    Ok(())
}

/// One line of `session_repl` input, from whichever source is active --
/// `read_raw_line` when `raw_active` (a real terminal, in raw mode), or
/// the same blocking-read-until-newline behavior `BufRead::lines()` gave
/// before this increment otherwise (every one of this project's own
/// tests pipes stdin, so this is the path they all still exercise,
/// unchanged). `Ok(None)` at EOF either way -- the same "loop just ends"
/// signal the previous `for line in stdin.lock().lines()` gave.
fn next_repl_line(
    stdin: &std::io::Stdin,
    raw_active: bool,
    stdout_lock: &std::sync::Arc<std::sync::Mutex<()>>,
) -> Result<Option<String>> {
    if raw_active {
        return read_raw_line(stdin, stdout_lock);
    }
    use std::io::BufRead;
    let mut buf = String::new();
    let n = stdin
        .lock()
        .read_line(&mut buf)
        .map_err(|e| HarnessError::io(Context::Cli, None, e))?;
    if n == 0 {
        return Ok(None);
    }
    while buf.ends_with('\n') || buf.ends_with('\r') {
        buf.pop();
    }
    Ok(Some(buf))
}

/// Reads one (possibly multi-line) submission from a raw-mode terminal,
/// byte by byte, doing this project's own minimal echo/editing -- raw
/// mode disables the terminal's own line buffering and local echo
/// (`termctl::RawModeGuard`'s own doc comment), so both become this
/// function's job instead. Builds on the foundation the previous
/// increment landed (see `PARITY.md`'s "raw-mode rendering foundation"
/// entry) with the rich-editor pieces that increment explicitly
/// deferred: **multi-line input** (Enter, a raw `\r`, submits; `Ctrl-J`,
/// a raw `\n` -- reachable because raw mode leaves the two bytes
/// distinct, unlike cooked mode's CR-to-NL translation -- inserts a
/// literal newline and keeps composing, so a multi-paragraph prompt can
/// be typed as itself rather than one `session prompt` line at a time)
/// and **Tab completion**, covering both slash-command names and (after
/// an `@`) fuzzy file-path completion -- see [`complete_repl_line`]'s
/// own doc comment for exactly what "fuzzy" means here and why there's
/// no live dropdown. Still no cursor movement *within* a line and no
/// history, and backspacing across a line the user already committed
/// with `Ctrl-J` only rejoins the buffer (no visual un-scroll) -- both
/// stay out of scope, needing real terminal cursor-positioning
/// primitives `termctl` deliberately doesn't have yet (see that
/// module's own doc comment). Accumulates raw bytes and decodes lossily
/// at submission time rather than tracking UTF-8 boundaries as they
/// arrive -- correct for well-formed input, and multi-byte sequences
/// still render correctly since each byte is echoed immediately as
/// typed, the same way a real terminal emulator assembles a UTF-8
/// sequence from bytes arriving one at a time.
fn read_raw_line(
    stdin: &std::io::Stdin,
    stdout_lock: &std::sync::Arc<std::sync::Mutex<()>>,
) -> Result<Option<String>> {
    use std::io::{Read, Write};
    let mut locked = stdin.lock();
    let mut out = std::io::stdout();
    let mut buf: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = locked
            .read(&mut byte)
            .map_err(|e| HarnessError::io(Context::Cli, None, e))?;
        if n == 0 {
            // The underlying fd closed mid-line -- treat like `Ctrl-D`
            // on an empty line: end the REPL.
            return Ok(None);
        }
        // Every write below is taken under `stdout_lock` -- since
        // increment #79 (the follow-up message queue), this function
        // runs continuously on its own background task while a prior
        // prompt may still be generating and printing its own reply
        // concurrently on the main task; without a shared lock the two
        // could interleave mid-write and garble the terminal. See
        // `session_repl`'s own doc comment on `stdout_lock` for the
        // full story -- the same shared-lock shape `session_rpc`'s
        // `forward_events` already established for an analogous
        // concurrent-writer problem.
        match byte[0] {
            b'\r' => {
                let _guard = stdout_lock.lock().unwrap();
                let _ = out.write_all(b"\r\n");
                let _ = out.flush();
                return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
            }
            b'\n' => {
                // `Ctrl-J`: continue composing on a fresh line rather
                // than submitting -- see this function's own doc
                // comment for why `\r`/`\n` can be told apart at all
                // here.
                buf.push(b'\n');
                let _guard = stdout_lock.lock().unwrap();
                let _ = out.write_all(b"\r\n");
                let _ = out.flush();
            }
            0x03 => {
                buf.clear();
                let _guard = stdout_lock.lock().unwrap();
                let _ = out.write_all(b"^C\r\n");
                let _ = out.flush();
            }
            0x04 if buf.is_empty() => {
                let _guard = stdout_lock.lock().unwrap();
                let _ = out.write_all(b"\r\n");
                let _ = out.flush();
                return Ok(None);
            }
            0x7f | 0x08 => match buf.pop() {
                Some(b'\n') => {
                    // Rejoins the buffer, but doesn't try to move
                    // the terminal's own cursor back up a line it
                    // already scrolled past -- see this function's
                    // own doc comment for why that stays out of
                    // scope here.
                }
                Some(_) => {
                    let _guard = stdout_lock.lock().unwrap();
                    let _ = out.write_all(b"\x08 \x08");
                    let _ = out.flush();
                }
                None => {}
            },
            0x09 => {
                let _guard = stdout_lock.lock().unwrap();
                if let Some(completed) = complete_repl_line(&buf) {
                    let erase = buf.len() - completed.common_prefix_len;
                    for _ in 0..erase {
                        let _ = out.write_all(b"\x08 \x08");
                    }
                    buf.truncate(completed.common_prefix_len);
                    buf.extend_from_slice(completed.replacement.as_bytes());
                    let _ = out.write_all(completed.replacement.as_bytes());
                } else {
                    // No completion possible (ambiguous with nothing
                    // more in common, or no candidates at all) -- the
                    // bell is the same portable "can't complete that"
                    // signal every terminal already understands, no
                    // dropdown UI required.
                    let _ = out.write_all(b"\x07");
                }
                let _ = out.flush();
            }
            b => {
                buf.push(b);
                let _guard = stdout_lock.lock().unwrap();
                let _ = out.write_all(&[b]);
                let _ = out.flush();
            }
        }
    }
}

/// Every slash-command `read_raw_line`'s Tab completion knows about --
/// kept as one list precisely so it can't drift from `session_repl`'s
/// own dispatch (a mismatch here would complete to a command that
/// doesn't exist, or fail to complete one that does).
const REPL_SLASH_COMMANDS: &[&str] = &[
    "/exit",
    "/quit",
    "/heartbeat",
    "/compact",
    "/file",
    "/fork",
    "/tree",
    "/branch-summary",
    "/export",
];

/// The result of a successful Tab completion: replace everything in the
/// buffer from `common_prefix_len` onward with `replacement`.
struct Completion {
    common_prefix_len: usize,
    replacement: String,
}

/// Tab completion for `read_raw_line`'s current buffer -- two triggers,
/// sharing one mechanism:
/// - the buffer is exactly a partial slash-command (`/` at the very
///   start, no whitespace yet) -- completes against
///   [`REPL_SLASH_COMMANDS`];
/// - the current word (the text since the last whitespace, or since the
///   start of the buffer) starts with `@` -- completes the path fragment
///   after it against real filesystem entries, the bounded, portable
///   slice of `prime-agent`'s TUI-side "`@` fuzzy search": no live
///   interactive dropdown (that needs terminal cursor-positioning
///   primitives `termctl` doesn't have yet -- see that module's own doc
///   comment), just Tab-driven completion of the reference itself.
///   "Fuzzy" here means subsequence matching (`fuzzy_matches`'s own doc
///   comment), not just a prefix match -- typing `@mn` can complete
///   toward `main.rs`.
///
/// Returns `None` when there's nothing to complete: zero candidates, or
/// more than one candidate with no further common prefix beyond what's
/// already typed (an ambiguous completion this text-only mechanism has
/// no listing UI to disambiguate with -- `read_raw_line`'s own caller
/// rings the terminal bell instead, the same "no dropdown" shape).
fn complete_repl_line(buf: &[u8]) -> Option<Completion> {
    let text = String::from_utf8_lossy(buf);
    let word_start = text.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    let word = &text[word_start..];

    // Slash-command completion only applies to the very first word of
    // the whole buffer -- once there's a preceding word (`word_start >
    // 0`), any `/`/`@` in the current word is ordinary prompt text or a
    // command's own argument, not the command name itself.
    if word_start == 0 {
        if let Some(fragment) = word.strip_prefix('/') {
            let candidates: Vec<&str> = REPL_SLASH_COMMANDS
                .iter()
                .copied()
                .filter(|c| c.strip_prefix('/').unwrap().starts_with(fragment))
                .collect();
            let common = common_prefix(&candidates)?;
            let completed = common.strip_prefix('/').unwrap_or(common);
            if completed.len() <= fragment.len() {
                return None;
            }
            return Some(Completion {
                common_prefix_len: 0,
                replacement: format!("/{completed}"),
            });
        }
    }

    let fragment = word.strip_prefix('@')?;
    let candidates = complete_at_path(fragment);
    let owned: Vec<&str> = candidates.iter().map(String::as_str).collect();
    let common = common_prefix(&owned)?;
    if common.len() <= fragment.len() {
        return None;
    }
    Some(Completion {
        common_prefix_len: word_start,
        replacement: format!("@{common}"),
    })
}

/// Fuzzy (subsequence) match: every character of `pattern`, in order,
/// appears somewhere in `candidate` -- case-insensitive, so `mn`
/// matches `main.rs`, `Main.rs`, and `MAIN.RS` alike. Not a scored/
/// ranked fuzzy match (no "best" ordering beyond what
/// [`complete_at_path`]'s own directory-listing order gives) -- ranking
/// candidates is exactly the kind of thing a live dropdown UI would do,
/// and this mechanism doesn't have one.
fn fuzzy_matches(candidate: &str, pattern: &str) -> bool {
    let mut pattern_chars = pattern.chars().flat_map(char::to_lowercase);
    let mut next = pattern_chars.next();
    for c in candidate.chars().flat_map(char::to_lowercase) {
        match next {
            Some(p) if c == p => next = pattern_chars.next(),
            _ => {}
        }
    }
    next.is_none()
}

/// Lists `fragment`'s directory (everything up to its last `/`, or `.`
/// if there isn't one) and fuzzy-matches each entry's own filename
/// against the part of `fragment` after that -- candidates are returned
/// as full paths (directory prefix reattached), with a trailing `/` on
/// directory entries so a completed directory reference can be Tab-ed
/// straight into its own contents next, the same convention shell path
/// completion already uses. An unreadable directory (a typo, a path
/// that isn't a directory at all) yields no candidates rather than an
/// error -- Tab completion finding nothing is an ordinary, silent
/// outcome, not a failure to surface.
fn complete_at_path(fragment: &str) -> Vec<String> {
    let (dir, name_fragment) = match fragment.rfind('/') {
        Some(i) => (&fragment[..=i], &fragment[i + 1..]),
        None => ("", fragment),
    };
    let scan_dir = if dir.is_empty() { "." } else { dir };
    let Ok(entries) = std::fs::read_dir(scan_dir) else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !fuzzy_matches(name, name_fragment) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        candidates.push(format!("{dir}{name}{}", if is_dir { "/" } else { "" }));
    }
    candidates.sort();
    candidates
}

/// The longest string every one of `candidates` starts with -- `None`
/// for an empty slice (nothing to complete toward at all).
fn common_prefix<'a>(candidates: &[&'a str]) -> Option<&'a str> {
    let first = *candidates.first()?;
    let mut end = first.len();
    for candidate in &candidates[1..] {
        let shared = first
            .bytes()
            .zip(candidate.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        end = end.min(shared);
    }
    // Never split a multi-byte UTF-8 character in half.
    while end > 0 && !first.is_char_boundary(end) {
        end -= 1;
    }
    Some(&first[..end])
}

/// File extensions this project treats as an image to attach out-of-band
/// (`protocol::TranscriptEntry::images`/`provider::ChatTurn::images`)
/// rather than expanded inline as text -- the same small, well-known set
/// every vision-capable provider's own multipart content type accepts.
/// Parity with a bounded slice of `prime-agent`'s image-paste feature --
/// see `PARITY.md`'s own "Interactive TUI: image paste support" entry
/// for why this project's own text-only shapes (not any missing backend
/// support -- `rp-server`'s own `ContentPart::ImageUrl` already exists)
/// were the actual gap "paste" needed closed.
const IMAGE_EXTENSIONS: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("bmp", "image/bmp"),
];

fn image_mime_type(path: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(path)
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    IMAGE_EXTENSIONS
        .iter()
        .find(|(known, _)| *known == ext)
        .map(|(_, mime)| *mime)
}

/// Hand-rolled RFC 4648 base64 encoding (standard alphabet, `=`
/// padding) -- the one encoding this project's image-attachment feature
/// needs, not a reason to add a `base64` crate dependency to a
/// dependency floor kept deliberately narrow (`ARCHITECTURE.md`'s own
/// "Dependency Stack" section), the same "hand-roll a narrowly scoped
/// protocol/encoding concern, don't hand-roll everything" reasoning that
/// chose to hand-roll SHA-256/HMAC/a ZMTP client for RLM while still
/// using `serde_json` rather than a hand-rolled JSON parser.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Reads `path`'s raw bytes and base64-encodes them into a `data:` URI --
/// the exact shape `rp-server`'s own `ContentPart::ImageUrl` (and, through
/// it, every backend it fronts: Anthropic, Gemini, and the
/// OpenAI-compatible path Ollama's own vision models go through) already
/// accepts for an inline image. `None` for a path with no recognized
/// image extension or that can't be read -- the caller falls back to
/// treating it as an ordinary (non-image) reference in that case.
fn load_image_as_data_uri(path: &str) -> Option<String> {
    let mime = image_mime_type(path)?;
    let bytes = std::fs::read(path).ok()?;
    Some(format!("data:{mime};base64,{}", base64_encode(&bytes)))
}

/// Expands every `@<path>` token in `text` (a `@` immediately followed
/// by a non-whitespace path fragment) into that file's content inline,
/// formatted the same way `/file`'s own `pending_file_content` prefix
/// already is -- the other half of the bounded `@`-reference slice
/// [`complete_repl_line`]'s own doc comment describes: Tab completes the
/// reference while composing, this expands it at submission time,
/// wherever in the text it was typed (not just prepended, unlike
/// `/file`) -- a more precise placement than `/file` gives, now that
/// there's a natural point in the text to put it. Applies regardless of
/// whether the line came from raw-mode input or the piped/cooked-mode
/// fallback (every one of this project's own tests uses the latter), so
/// it's testable without a real terminal. A token whose path doesn't
/// resolve to a real, readable file is left untouched -- most likely
/// just a literal `@` mention (an email-style handle, social-media
/// syntax the user typed on purpose), not a botched file reference, so
/// silently leaving it alone beats guessing or erroring.
///
/// Returns `(expanded_text, images)`: an `@<path>` token naming a real,
/// readable *image* file (`image_mime_type`'s own extension list) isn't
/// inlined as text at all -- image bytes obviously can't be, the way a
/// text file's content can -- its literal `@<path>` is left in the text
/// unchanged (a perfectly readable reference on its own) and its content
/// is instead base64-encoded and collected into `images`, the same
/// out-of-band shape `protocol::TranscriptEntry::images`/`provider::
/// ChatTurn::images` already carry.
fn expand_at_references(text: &str) -> (String, Vec<String>) {
    fn expand_word(word: &str, images: &mut Vec<String>) -> String {
        if let Some(path) = word.strip_prefix('@') {
            if !path.is_empty() {
                if let Some(data_uri) = load_image_as_data_uri(path) {
                    images.push(data_uri);
                    return word.to_string();
                }
                if let Ok(content) = std::fs::read_to_string(path) {
                    return format!("--- {path} ---\n{content}\n---\n\n");
                }
            }
        }
        word.to_string()
    }
    let mut images = Vec::new();
    let mut out = String::new();
    let mut word = String::new();
    for c in text.chars() {
        if c.is_whitespace() {
            out.push_str(&expand_word(&word, &mut images));
            word.clear();
            out.push(c);
        } else {
            word.push(c);
        }
    }
    out.push_str(&expand_word(&word, &mut images));
    (out, images)
}

/// Parses `/fork`'s own trailing `[--at N] [--name TEXT]` -- a small,
/// self-contained parser rather than reusing `cli::scan_named_flag`
/// (private to `cli.rs`, and shaped around a full `&[&String]` argv
/// slice, not a single already-stripped REPL line) since this is the
/// only REPL command that needs more than "everything after the
/// keyword is one piece of free text" (`/compact`'s own `instructions`,
/// `/export`'s `path`). Returns a plain `String` error message, printed
/// directly by the caller, the same friendly non-fatal shape
/// `/heartbeat every <duration>`'s own parse-failure handling already
/// uses.
fn parse_repl_fork_args(rest: &str) -> std::result::Result<(Option<u64>, Option<String>), String> {
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let mut at_sequence = None;
    let mut name = None;
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i] {
            "--at" => {
                let value = tokens.get(i + 1).ok_or("--at requires a value")?;
                at_sequence = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("--at requires an integer, got {value:?}"))?,
                );
                i += 2;
            }
            "--name" => {
                let value = tokens.get(i + 1).ok_or("--name requires a value")?;
                name = Some((*value).to_string());
                i += 2;
            }
            other => return Err(format!("unknown /fork argument {other:?}")),
        }
    }
    Ok((at_sequence, name))
}

/// `image_paths` -- parity with a bounded slice of `prime-agent`'s
/// image-paste feature, `harness session prompt <id> [--image <path>]...
/// <text...>`. Unlike `/file`'s own silent-fallback ambiguity (an
/// inline `@` token might not be a file reference at all), an explicit
/// `--image <path>` is an unambiguous user intent, so a path that isn't a
/// readable, recognized image file fails loudly instead of being folded
/// in as literal text.
pub async fn session_prompt(
    state_root: &Path,
    session_id: String,
    text: String,
    image_paths: Vec<String>,
    mode: OutputMode,
) -> Result<()> {
    let entry = if image_paths.is_empty() {
        send_prompt(state_root, &session_id, text).await?
    } else {
        let mut images = Vec::with_capacity(image_paths.len());
        for path in &image_paths {
            let data_uri = load_image_as_data_uri(path).ok_or_else(|| {
                HarnessError::conflict(
                    Context::Cli,
                    format!(
                        "--image {path}: not a readable image file (recognized extensions: \
                         png, jpg, jpeg, gif, webp, bmp)"
                    ),
                )
            })?;
            images.push(data_uri);
        }
        send_prompt_with_images(state_root, &session_id, text, Some(images)).await?
    };
    match mode {
        OutputMode::Json => print_json(&Response::SessionPromptAck { entry }),
        OutputMode::Text => print_entry(&entry),
    }
    Ok(())
}

/// Shared by [`session_prompt`] and [`session_autonomous`]'s own
/// continuation turns -- the latter needs the raw entry, not printed
/// text, and drives many of these in a loop rather than just one.
/// Text-only; [`send_prompt_with_images`] is the same thing plus a
/// bounded slice of `prime-agent`'s image-paste feature (see that
/// function's own doc comment), kept as a separate function rather than
/// adding an `images` parameter here so every one of this function's
/// existing callers stays untouched.
async fn send_prompt(
    state_root: &Path,
    session_id: &str,
    text: String,
) -> Result<crate::protocol::TranscriptEntry> {
    send_prompt_with_images(state_root, session_id, text, None).await
}

/// Parity with a bounded slice of `prime-agent`'s image-paste feature --
/// see `PARITY.md`'s own "Interactive TUI: image paste support" entry.
/// `images` is a list of `data:<mime>;base64,<...>` URIs, the same shape
/// `protocol::TranscriptEntry::images`/`provider::ChatTurn::images`
/// already use.
async fn send_prompt_with_images(
    state_root: &Path,
    session_id: &str,
    text: String,
    images: Option<Vec<String>>,
) -> Result<crate::protocol::TranscriptEntry> {
    let mut conn = connect(state_root).await?;
    conn.write_request(
        Context::Daemon,
        &Request::SessionPrompt {
            session_id: session_id.to_string(),
            text,
            images,
        },
    )
    .await?;
    match read_response_with_timeout(&mut conn, PROMPT_RESPONSE_TIMEOUT).await? {
        Response::SessionPromptAck { entry } => Ok(entry),
        other => Err(unexpected_response(other)),
    }
}

/// `harness model list` -- bounded parity with `prime-agent model
/// list`'s catalog browse: which providers this process's own
/// environment configures, not each one's actual per-model IDs (see
/// `rp_server::known_providers`'s own doc comment for why). A pure
/// environment-variable check, no daemon connection at all -- same
/// reasoning as `prompt_template_list` below.
///
/// `detailed`: additionally starts (or reuses) an `rp-server` sidecar
/// -- the same `rp_server::ensure_running` call `daemon::Supervisor`
/// makes for `session new --model` -- and queries its real `GET /v1/
/// models` catalog (see `rp_server::ModelCatalogEntry`'s own doc
/// comment for the shape). Unlike the plain listing above, this needs
/// `rp-server` actually installed and reachable, so it fails loudly
/// (not silently falling back to the plain listing) when it isn't --
/// same "fail loudly rather than silently degrade" reasoning as
/// `session_new_with_model_fails_loudly_when_rp_server_is_unavailable`.
pub async fn model_list(state_root: &Path, detailed: bool, mode: OutputMode) -> Result<()> {
    if !detailed {
        let providers = crate::rp_server::known_providers(state_root);
        match mode {
            OutputMode::Json => print_json(&providers),
            OutputMode::Text => {
                for p in &providers {
                    let status = if p.configured {
                        "configured"
                    } else {
                        "not configured"
                    };
                    println!("{}\t{status}", p.name);
                }
            }
        }
        return Ok(());
    }

    let port = crate::rp_server::ensure_running(state_root).await?;
    let models = crate::rp_server::fetch_model_catalog(port).await?;
    match mode {
        OutputMode::Json => print_json(&models),
        OutputMode::Text => {
            if models.is_empty() {
                println!("no models");
            }
            for m in &models {
                let context_length = m
                    .context_length
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "-".to_string());
                println!(
                    "{}\towned_by={}\tcontext_length={}",
                    m.id, m.owned_by, context_length
                );
            }
        }
    }
    Ok(())
}

/// `harness prompt-template list` -- a pure local directory scan, no
/// daemon connection at all (unlike almost every other subcommand in
/// this file).
pub async fn prompt_template_list(state_root: &Path, mode: OutputMode) -> Result<()> {
    let cwd = current_dir()?;
    let templates = crate::prompt_template::discover(state_root, &cwd)?;
    match mode {
        OutputMode::Json => print_json(&templates),
        OutputMode::Text => {
            if templates.is_empty() {
                println!("no prompt templates");
            }
            for t in &templates {
                println!("{}\t{}", t.name, t.description.as_deref().unwrap_or(""));
            }
        }
    }
    Ok(())
}

/// `harness skill list` -- lists every skill `skills::discover` finds
/// (`<state-dir>/skills/*/SKILL.md`), with its description. Global-only,
/// so no `cwd` needed -- see `skills.rs`'s own doc comment for why. A
/// pure local directory scan, same as `prompt_template_list`, no daemon
/// connection.
pub async fn skill_list(state_root: &Path, mode: OutputMode) -> Result<()> {
    let skills = crate::skills::discover(state_root)?;
    match mode {
        OutputMode::Json => print_json(&skills),
        OutputMode::Text => {
            if skills.is_empty() {
                println!("no skills");
            }
            for s in &skills {
                println!("{}\t{}", s.name, s.description.as_deref().unwrap_or(""));
            }
        }
    }
    Ok(())
}

/// `harness prompt-template render <name> [args...]` -- expands the
/// named template and prints it, without sending it anywhere. Also a
/// pure local operation, no daemon connection.
pub async fn prompt_template_render(
    state_root: &Path,
    name: String,
    args: Vec<String>,
    mode: OutputMode,
) -> Result<()> {
    let template = find_template(state_root, &name)?;
    let text = template.expand(&args);
    match mode {
        OutputMode::Json => print_json(&serde_json::json!({ "name": name, "text": text })),
        OutputMode::Text => println!("{text}"),
    }
    Ok(())
}

/// `harness session prompt-template <id> <name> [args...]` -- parity
/// with typing `/name args...` in `prime-agent`'s live editor: expands
/// the named template and sends it as an ordinary `SessionPrompt`, same
/// as `session_prompt` would with that already-expanded text.
pub async fn session_prompt_template(
    state_root: &Path,
    session_id: String,
    name: String,
    args: Vec<String>,
    mode: OutputMode,
) -> Result<()> {
    let template = find_template(state_root, &name)?;
    let text = template.expand(&args);
    let entry = send_prompt(state_root, &session_id, text).await?;
    match mode {
        OutputMode::Json => print_json(&Response::SessionPromptAck { entry }),
        OutputMode::Text => print_entry(&entry),
    }
    Ok(())
}

fn current_dir() -> Result<std::path::PathBuf> {
    std::env::current_dir().map_err(|e| HarnessError::io(Context::Cli, None, e))
}

fn find_template(state_root: &Path, name: &str) -> Result<crate::prompt_template::PromptTemplate> {
    let cwd = current_dir()?;
    crate::prompt_template::find(state_root, &cwd, name)?.ok_or_else(|| {
        HarnessError::conflict(Context::Cli, format!("no such prompt template: {name}"))
    })
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

/// Parity with `prime-agent /compact [instructions]`. Also the
/// implementation `session_repl`'s own `/compact` line reuses -- see
/// that branch's own comment for why no separate client-side helper is
/// needed there.
pub async fn session_compact(
    state_root: &Path,
    session_id: String,
    instructions: Option<String>,
    mode: OutputMode,
) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(
        Context::Daemon,
        &Request::SessionCompact {
            session_id,
            instructions,
        },
    )
    .await?;
    match read_response(&mut conn).await? {
        response @ Response::SessionCompactAck { .. } => {
            match (&response, mode) {
                (_, OutputMode::Json) => print_json(&response),
                (
                    Response::SessionCompactAck {
                        compacted: true,
                        summary,
                    },
                    OutputMode::Text,
                ) => {
                    println!(
                        "compacted -- new summary: {}",
                        summary.as_deref().unwrap_or("")
                    )
                }
                (
                    Response::SessionCompactAck {
                        compacted: false, ..
                    },
                    OutputMode::Text,
                ) => {
                    println!("nothing to compact (no model configured, or nothing old enough yet)")
                }
                _ => unreachable!(),
            }
            Ok(())
        }
        other => Err(unexpected_response(other)),
    }
}

/// `session fork <id> [--at N] [--name NAME]` -- see `protocol::
/// Request::SessionFork`'s own doc comment. Same response shape as
/// `session new` (`Response::SessionNew`): a fork *is* a brand-new
/// session, from the client's point of view no different from one
/// created any other way once it exists.
pub async fn session_fork(
    state_root: &Path,
    session_id: String,
    at_sequence: Option<u64>,
    name: Option<String>,
    mode: OutputMode,
) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(
        Context::Daemon,
        &Request::SessionFork {
            session_id,
            at_sequence,
            name,
        },
    )
    .await?;
    match read_response(&mut conn).await? {
        Response::SessionNew { session_id } => {
            match mode {
                OutputMode::Json => print_json(&Response::SessionNew {
                    session_id: session_id.clone(),
                }),
                OutputMode::Text => println!("{session_id}"),
            }
            Ok(())
        }
        other => Err(unexpected_response(other)),
    }
}

/// `session tree <id>` -- parity with `prime-agent`'s `/tree`
/// visualization half: prints every branch of `session_id`'s own
/// transcript (not just the active one -- `session attach`'s own "full,
/// unfiltered audit trail" reasoning applies here too), indented by
/// depth, with the entry `state.active_leaf_sequence` currently points
/// at marked `(active)`. Read-only; see [`session_set_active_leaf`] for
/// the navigation half that actually moves the active leaf.
///
/// Reconstructs the tree client-side from the same two fields the wire
/// protocol already carries (`TranscriptEntry::parent_sequence`,
/// `SessionState::active_leaf_sequence`) rather than adding a new
/// request/response shape just to pre-render it server-side -- this
/// project's own "the client renders, the wire carries data" split every
/// other `--mode text` renderer already follows. `effective_parent`
/// mirrors `session::AgentSession::active_chain`'s own legacy-fallback
/// rule exactly (`parent_sequence: None` with `sequence > 1` implicitly
/// continues from `sequence - 1`), so a pre-branching session's tree
/// still renders as the flat chain it always was.
pub async fn session_tree(state_root: &Path, session_id: String, mode: OutputMode) -> Result<()> {
    let (state, transcript) = fetch_session_snapshot(state_root, &session_id).await?;
    if mode == OutputMode::Json {
        print_json(&serde_json::json!({
            "type": "session_tree",
            "active_leaf_sequence": state.active_leaf_sequence,
            "transcript": transcript,
        }));
        return Ok(());
    }
    if transcript.is_empty() {
        println!("(no turns yet)");
        return Ok(());
    }

    fn effective_parent(transcript: &[TranscriptEntry], entry: &TranscriptEntry) -> Option<u64> {
        match entry.parent_sequence {
            Some(parent) => Some(parent),
            None if entry.sequence > 1 => {
                let previous = entry.sequence - 1;
                transcript
                    .iter()
                    .any(|e| e.sequence == previous)
                    .then_some(previous)
            }
            None => None,
        }
    }

    let mut children: std::collections::BTreeMap<Option<u64>, Vec<&TranscriptEntry>> =
        std::collections::BTreeMap::new();
    for entry in &transcript {
        children
            .entry(effective_parent(&transcript, entry))
            .or_default()
            .push(entry);
    }

    fn print_subtree(
        children: &std::collections::BTreeMap<Option<u64>, Vec<&TranscriptEntry>>,
        parent: Option<u64>,
        depth: usize,
        active_leaf_sequence: Option<u64>,
    ) {
        let Some(siblings) = children.get(&parent) else {
            return;
        };
        for entry in siblings {
            let role = match entry.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System => "system",
                Role::Tool => "tool",
            };
            // `BranchSummary` entries carry an empty `text` (the data
            // lives in the structured field, same as `child_usage_
            // attributed`'s own shape) -- fall back to a short
            // description instead of an empty preview.
            let preview: String = match &entry.branch_summary {
                Some(branch_summary) => format!(
                    "(branch summary of sequence {}, {} turn{})",
                    branch_summary.branch_leaf_sequence,
                    branch_summary.entry_count,
                    if branch_summary.entry_count == 1 {
                        ""
                    } else {
                        "s"
                    }
                ),
                None => entry.text.chars().take(60).collect(),
            };
            let marker = if active_leaf_sequence == Some(entry.sequence) {
                " (active)"
            } else {
                ""
            };
            println!(
                "{}[{}] {role}: {preview}{marker}",
                "  ".repeat(depth),
                entry.sequence
            );
            print_subtree(
                children,
                Some(entry.sequence),
                depth + 1,
                active_leaf_sequence,
            );
        }
    }
    print_subtree(&children, None, 0, state.active_leaf_sequence);
    Ok(())
}

/// `session set-active-leaf <id> <sequence>` -- parity with
/// `prime-agent`'s `/tree` navigation half: redirects
/// `session_id`'s own active leaf to `sequence`, the entry the *next*
/// prompt continues from. See `protocol::Request::SessionSetActiveLeaf`'s
/// own doc comment for the underlying mechanism -- this is the first
/// client surface to reach it; a `sequence` that doesn't name a real
/// transcript entry comes back as `Response::Error { conflict: true,
/// .. }`, surfaced the same way any other conflict already is.
pub async fn session_set_active_leaf(
    state_root: &Path,
    session_id: String,
    sequence: u64,
    mode: OutputMode,
) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(
        Context::Daemon,
        &Request::SessionSetActiveLeaf {
            session_id,
            sequence,
        },
    )
    .await?;
    match read_response(&mut conn).await? {
        response @ Response::SessionSetActiveLeafAck {
            active_leaf_sequence,
        } => {
            match mode {
                OutputMode::Json => print_json(&response),
                OutputMode::Text => println!("active leaf set to sequence {active_leaf_sequence}"),
            }
            Ok(())
        }
        other => Err(unexpected_response(other)),
    }
}

/// `session branch-summary <id> <branch-leaf-sequence>` -- parity with
/// `session-format.md`'s `BranchSummaryEntry`. See
/// `protocol::Request::SessionBranchSummarize`'s own doc comment for the
/// no-op cases (`summarized: false`): no model configured, or
/// `branch_leaf_sequence` already part of the active chain.
pub async fn session_branch_summarize(
    state_root: &Path,
    session_id: String,
    branch_leaf_sequence: u64,
    mode: OutputMode,
) -> Result<()> {
    let mut conn = connect(state_root).await?;
    conn.write_request(
        Context::Daemon,
        &Request::SessionBranchSummarize {
            session_id,
            branch_leaf_sequence,
        },
    )
    .await?;
    match read_response(&mut conn).await? {
        response @ Response::SessionBranchSummarizeAck { .. } => {
            match (&response, mode) {
                (_, OutputMode::Json) => print_json(&response),
                (
                    Response::SessionBranchSummarizeAck {
                        summarized: true,
                        summary,
                    },
                    OutputMode::Text,
                ) => println!("branch summarized: {}", summary.as_deref().unwrap_or("")),
                (
                    Response::SessionBranchSummarizeAck {
                        summarized: false, ..
                    },
                    OutputMode::Text,
                ) => println!(
                    "nothing to summarize (no model configured, or that sequence is already \
                     part of the active branch)"
                ),
                _ => unreachable!(),
            }
            Ok(())
        }
        other => Err(unexpected_response(other)),
    }
}

/// Shared by [`schedule_add`] and [`session_spawn`]'s own near-immediate
/// one-shot enqueue -- the latter needs the raw id, not printed text.
async fn add_schedule(
    state_root: &Path,
    session_id: String,
    text: String,
    kind: crate::protocol::ScheduleKind,
) -> Result<String> {
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
        Response::ScheduleAdded { schedule_id } => Ok(schedule_id),
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
    let schedule_id = add_schedule(state_root, session_id, text, kind).await?;
    match mode {
        OutputMode::Json => print_json(&Response::ScheduleAdded { schedule_id }),
        OutputMode::Text => println!("{schedule_id}"),
    }
    Ok(())
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

pub async fn goal_update(
    state_root: &Path,
    session_id: String,
    action: GoalAction,
    mode: OutputMode,
) -> Result<()> {
    let goal = set_goal(state_root, &session_id, action).await?;
    match mode {
        OutputMode::Json => print_json(&Response::GoalUpdateAck { goal }),
        OutputMode::Text => print_goal_text(&goal),
    }
    Ok(())
}

pub async fn goal_show(state_root: &Path, session_id: String, mode: OutputMode) -> Result<()> {
    let goal = fetch_goal(state_root, &session_id).await?;
    match mode {
        OutputMode::Json => print_json(&Response::GoalShow { goal }),
        OutputMode::Text => print_goal_text(&goal),
    }
    Ok(())
}

/// Shared by [`goal_show`] and [`session_autonomous`]'s per-iteration
/// re-check -- the latter needs the raw value, not printed text.
async fn fetch_goal(state_root: &Path, session_id: &str) -> Result<Option<GoalState>> {
    let mut conn = connect(state_root).await?;
    conn.write_request(
        Context::Daemon,
        &Request::GoalShow {
            session_id: session_id.to_string(),
        },
    )
    .await?;
    match read_response(&mut conn).await? {
        Response::GoalShow { goal } => Ok(goal),
        other => Err(unexpected_response(other)),
    }
}

/// Shared by [`goal_update`] and [`session_autonomous`]'s own
/// `Complete` transition when its quality gate passes -- the latter
/// needs the raw value, not printed text.
async fn set_goal(
    state_root: &Path,
    session_id: &str,
    action: GoalAction,
) -> Result<Option<GoalState>> {
    let mut conn = connect(state_root).await?;
    conn.write_request(
        Context::Daemon,
        &Request::GoalUpdate {
            session_id: session_id.to_string(),
            action,
        },
    )
    .await?;
    match read_response(&mut conn).await? {
        Response::GoalUpdateAck { goal } => Ok(goal),
        other => Err(unexpected_response(other)),
    }
}

fn print_goal_text(goal: &Option<GoalState>) {
    match goal {
        None => println!("no goal"),
        Some(g) => {
            let status = match g.status {
                GoalStatus::Active => "active",
                GoalStatus::Paused => "paused",
                GoalStatus::Completed => "completed",
            };
            println!("{status}\t{}", g.text);
        }
    }
}

pub async fn harness_update(
    state_root: &Path,
    session_id: String,
    action: HarnessAction,
    mode: OutputMode,
) -> Result<()> {
    let state = set_harness(state_root, &session_id, action).await?;
    match mode {
        OutputMode::Json => print_json(&Response::HarnessUpdateAck { state }),
        OutputMode::Text => print_harness_text(&state),
    }
    Ok(())
}

pub async fn harness_show(state_root: &Path, session_id: String, mode: OutputMode) -> Result<()> {
    let state = fetch_harness(state_root, &session_id).await?;
    match mode {
        OutputMode::Json => print_json(&Response::HarnessShow { state }),
        OutputMode::Text => print_harness_text(&state),
    }
    Ok(())
}

/// Shared by [`harness_show`] and [`session_refine`] -- the latter needs
/// the raw value, not printed text.
async fn fetch_harness(state_root: &Path, session_id: &str) -> Result<HarnessState> {
    let mut conn = connect(state_root).await?;
    conn.write_request(
        Context::Daemon,
        &Request::HarnessShow {
            session_id: session_id.to_string(),
        },
    )
    .await?;
    match read_response(&mut conn).await? {
        Response::HarnessShow { state } => Ok(state),
        other => Err(unexpected_response(other)),
    }
}

/// Shared by [`harness_update`] and [`session_refine`]'s own `Add` once
/// its review proposal comes back -- the latter needs the raw value, not
/// printed text.
async fn set_harness(
    state_root: &Path,
    session_id: &str,
    action: HarnessAction,
) -> Result<HarnessState> {
    let mut conn = connect(state_root).await?;
    conn.write_request(
        Context::Daemon,
        &Request::HarnessUpdate {
            session_id: session_id.to_string(),
            action,
        },
    )
    .await?;
    match read_response(&mut conn).await? {
        Response::HarnessUpdateAck { state } => Ok(state),
        other => Err(unexpected_response(other)),
    }
}

fn print_harness_text(state: &HarnessState) {
    if state.notes.is_empty() {
        println!("no harness notes");
    }
    for note in &state.notes {
        let kind = match note.kind {
            HarnessNoteKind::Prompt => "prompt",
            HarnessNoteKind::Memory => "memory",
            HarnessNoteKind::SkillDescription => "skill",
        };
        println!("{}\t{kind}\t{}", note.id, note.text);
    }
    println!("history={}", state.history.len());
}

/// Parity with `prime-agent`'s Continual Harness `/refine`: "reviews the
/// current trajectory and can apply small, evidence-backed updates to
/// supplemental harness state." Fetches the session's transcript (via a
/// `SessionAttach`, read only up to the first `Snapshot` event, then
/// disconnected -- same as any client that stops reading mid-stream) and
/// current harness notes, asks the session's own model to propose one
/// small addition, and records that proposal as a new `Memory` note.
///
/// Unlike a hypothetical hidden "analysis" side channel, this review
/// prompt is sent through the ordinary `SessionPrompt` path, so it (and
/// the model's reply) show up as regular, visible transcript turns --
/// the "evidence" behind a refinement stays auditable inline with the
/// trajectory it reviewed, rather than happening invisibly. That's a
/// deliberate simplification of `prime-agent`'s own hidden-analysis-call
/// design, not an oversight: this project's `ModelProvider` trait has no
/// side channel for a provider call that skips the transcript (see
/// `session::AgentSession::prompt`), and adding one for this alone
/// wasn't worth it.
pub async fn session_refine(state_root: &Path, session_id: String, mode: OutputMode) -> Result<()> {
    let transcript = fetch_transcript_snapshot(state_root, &session_id).await?;
    let harness = fetch_harness(state_root, &session_id).await?;
    let review_prompt = build_refine_prompt(&transcript, &harness.notes);
    let entry = send_prompt(state_root, &session_id, review_prompt).await?;
    let proposal = entry.text.trim().to_string();
    let updated = set_harness(
        state_root,
        &session_id,
        HarnessAction::Add {
            kind: HarnessNoteKind::Memory,
            text: proposal.clone(),
        },
    )
    .await?;
    match mode {
        OutputMode::Json => print_json(&serde_json::json!({
            "type": "refine_applied",
            "note": proposal,
            "harness": updated,
        })),
        OutputMode::Text => println!(
            "refine: added memory note (notes={}, history={})",
            updated.notes.len(),
            updated.history.len()
        ),
    }
    Ok(())
}

/// How many of the most recent transcript entries `session_refine`
/// includes in its review prompt -- bounded so a long-running session
/// doesn't grow the review prompt without limit; recent turns are the
/// ones a small, evidence-backed update is actually about.
const REFINE_TRAJECTORY_WINDOW: usize = 20;

fn build_refine_prompt(transcript: &[TranscriptEntry], notes: &[HarnessNote]) -> String {
    let mut prompt = String::from(
        "Review the following trajectory and the current supplemental harness notes. \
         Propose exactly one small, evidence-backed addition to the harness notes (a \
         lesson, reminder, or reusable fact) that would help on future turns. Reply with \
         just the note text.\n\nTrajectory:\n",
    );
    let start = transcript.len().saturating_sub(REFINE_TRAJECTORY_WINDOW);
    for entry in &transcript[start..] {
        let role = match entry.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
            Role::Tool => "tool",
        };
        prompt.push_str(&format!("[{}] {role}: {}\n", entry.sequence, entry.text));
    }
    prompt.push_str("\nCurrent harness notes:\n");
    if notes.is_empty() {
        prompt.push_str("(none yet)\n");
    } else {
        for note in notes {
            prompt.push_str(&format!("- ({:?}) {}\n", note.kind, note.text));
        }
    }
    prompt
}

/// Fetches just the recovery-baseline `Snapshot` event from a
/// `SessionAttach` stream, then drops the connection without reading
/// further -- an ordinary early client disconnect from the daemon's own
/// point of view (`session_attach`'s own doc comment covers the same
/// stream; this is a one-shot read of its first event only). Shared by
/// every caller that wants the full `state.json`/`transcript.jsonl` pair
/// (`session_tree` needs `SessionState::active_leaf_sequence` alongside
/// the transcript; everyone else only needs the transcript, via
/// [`fetch_transcript_snapshot`]).
async fn fetch_session_snapshot(
    state_root: &Path,
    session_id: &str,
) -> Result<(Box<SessionState>, Vec<TranscriptEntry>)> {
    let mut conn = connect(state_root).await?;
    conn.write_request(
        Context::Daemon,
        &Request::SessionAttach {
            session_id: session_id.to_string(),
        },
    )
    .await?;
    match read_response(&mut conn).await? {
        Response::SessionAttachStarted { .. } => {}
        other => return Err(unexpected_response(other)),
    }
    match conn.read_event(Context::Daemon).await? {
        Some(SessionEvent::Snapshot { state, transcript }) => Ok((state, transcript)),
        Some(other) => Err(HarnessError::protocol(
            Context::Daemon,
            format!("expected a snapshot event first, got {other:?}"),
        )),
        None => Err(HarnessError::protocol(
            Context::Daemon,
            "connection closed before a snapshot event",
        )),
    }
}

async fn fetch_transcript_snapshot(
    state_root: &Path,
    session_id: &str,
) -> Result<Vec<TranscriptEntry>> {
    let (_, transcript) = fetch_session_snapshot(state_root, session_id).await?;
    Ok(transcript)
}

/// Bounded parity with `prime-agent /autonomous`: repeatedly sends a
/// continuation `SessionPrompt` toward the session's existing `Active`
/// goal until `max_turns` turns have gone out, `max_time_ms` (if given)
/// has elapsed, or `quality_gate` (if given) exits zero -- at which
/// point the goal is marked `Complete`. No token budget: neither
/// `EchoProvider` nor `RustyProviderModel`'s `rp-server` round trip
/// surfaces token counts today, so this tracks only turns and
/// wall-clock time (see `PARITY.md`).
///
/// Unlike `prime-agent`'s own `/autonomous`, which runs inside an
/// already-live interactive session, this is a one-shot foreground CLI
/// call -- parity with every other subcommand in this project (`session
/// prompt` included), not a background daemon-side loop. A persistent,
/// always-on autonomous daemon loop is the larger, separate piece this
/// increment doesn't attempt.
///
/// The goal is re-fetched at the top of every iteration (not just once
/// up front): if another client pauses, completes, or clears it out
/// from under a running autonomous loop, that's honored as a normal stop
/// condition on the very next turn, not raced against.
pub async fn session_autonomous(
    state_root: &Path,
    session_id: String,
    max_turns: u32,
    max_time_ms: Option<u64>,
    quality_gate: Option<String>,
    mode: OutputMode,
) -> Result<()> {
    match fetch_goal(state_root, &session_id).await? {
        Some(g) if g.status == GoalStatus::Active => {}
        _ => {
            return Err(HarnessError::conflict(
                Context::Daemon,
                "`session autonomous` requires an active goal -- run `session goal set <id> <text...>` first",
            ));
        }
    }

    let start = std::time::Instant::now();
    let mut turns_used = 0u32;
    let stop_reason: &'static str;

    loop {
        let goal = fetch_goal(state_root, &session_id).await?;
        let goal = match &goal {
            Some(g) if g.status == GoalStatus::Active => g,
            _ => {
                stop_reason = "goal is no longer active";
                break;
            }
        };

        if let Some(gate) = &quality_gate {
            if run_quality_gate(gate).await? {
                set_goal(state_root, &session_id, GoalAction::Complete).await?;
                stop_reason = "quality gate passed";
                break;
            }
        }

        if turns_used >= max_turns {
            stop_reason = "max turns reached";
            break;
        }
        if let Some(budget_ms) = max_time_ms {
            if start.elapsed().as_millis() as u64 >= budget_ms {
                stop_reason = "max time reached";
                break;
            }
        }

        let prompt_text = format!("Continue working toward the goal: {}", goal.text);
        send_prompt(state_root, &session_id, prompt_text).await?;
        turns_used += 1;
    }

    match mode {
        OutputMode::Json => print_json(&serde_json::json!({
            "type": "autonomous_stopped",
            "reason": stop_reason,
            "turns_used": turns_used,
        })),
        OutputMode::Text => {
            println!("autonomous run stopped: {stop_reason} (turns={turns_used})")
        }
    }
    Ok(())
}

/// Cross-platform "run this arbitrary shell command and report whether
/// it exited zero" -- `sh -c` on Unix, `cmd /C` on Windows, matching
/// this project's own CI matrix (`ubuntu`/`macos`/`windows-latest`).
/// Stdout/stderr are left inherited (`rusty_tokio::process::Command`'s
/// own default, same as `std::process::Command`), not `Stdio::null()`,
/// so a failing gate's own output stays visible to whoever is watching
/// this CLI invocation.
async fn run_quality_gate(command: &str) -> Result<bool> {
    use rusty_tokio::process::Command;
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", command]);
        c
    };
    let status = cmd
        .status()
        .await
        .map_err(|e| HarnessError::io(Context::Cli, None, e))?;
    Ok(status.success())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rpa-client-test-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn expand_at_references_folds_in_a_real_files_content() {
        let dir = temp_dir("expand-real-file");
        let path = dir.join("notes.txt");
        std::fs::write(&path, "the file's content").unwrap();
        let path_str = path.to_str().unwrap();

        let (expanded, images) = expand_at_references(&format!("before @{path_str} after"));
        assert_eq!(
            expanded,
            format!("before --- {path_str} ---\nthe file's content\n---\n\n after")
        );
        assert!(images.is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn expand_at_references_leaves_a_nonexistent_path_untouched() {
        let (expanded, images) = expand_at_references("hello @this-path-does-not-exist-xyz world");
        assert_eq!(expanded, "hello @this-path-does-not-exist-xyz world");
        assert!(images.is_empty());
    }

    #[test]
    fn expand_at_references_leaves_plain_text_with_no_at_tokens_untouched() {
        let text = "just an ordinary prompt\nwith a second line, no references";
        let (expanded, images) = expand_at_references(text);
        assert_eq!(expanded, text);
        assert!(images.is_empty());
    }

    #[test]
    fn expand_at_references_preserves_surrounding_multiline_structure() {
        let dir = temp_dir("expand-multiline");
        let path = dir.join("data.txt");
        std::fs::write(&path, "DATA").unwrap();
        let path_str = path.to_str().unwrap();

        let (expanded, images) =
            expand_at_references(&format!("line one\n@{path_str}\nline three"));
        assert!(expanded.starts_with("line one\n"), "got: {expanded:?}");
        assert!(expanded.ends_with("\nline three"), "got: {expanded:?}");
        assert!(expanded.contains("DATA"), "got: {expanded:?}");
        assert!(images.is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn expand_at_references_collects_an_image_reference_out_of_band() {
        let dir = temp_dir("expand-image");
        let path = dir.join("photo.png");
        // A 1x1 transparent PNG's minimal real byte content -- doesn't
        // need to be a valid, decodable image for this test, only real
        // bytes readable off disk and a recognized extension.
        std::fs::write(&path, [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]).unwrap();
        let path_str = path.to_str().unwrap();

        let (expanded, images) = expand_at_references(&format!("look at @{path_str} please"));
        // The literal `@path` mention stays in the text -- image bytes
        // can't be inlined the way a text file's content is.
        assert_eq!(expanded, format!("look at @{path_str} please"));
        assert_eq!(images.len(), 1);
        assert!(images[0].starts_with("data:image/png;base64,"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn expand_at_references_collects_multiple_images_in_order() {
        let dir = temp_dir("expand-multi-image");
        let path_a = dir.join("a.png");
        let path_b = dir.join("b.jpg");
        std::fs::write(&path_a, [1, 2, 3]).unwrap();
        std::fs::write(&path_b, [4, 5, 6]).unwrap();
        let (a_str, b_str) = (path_a.to_str().unwrap(), path_b.to_str().unwrap());

        let (_, images) = expand_at_references(&format!("@{a_str} and @{b_str}"));
        assert_eq!(images.len(), 2);
        assert!(images[0].starts_with("data:image/png;base64,"));
        assert!(images[1].starts_with("data:image/jpeg;base64,"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fuzzy_matches_finds_an_in_order_subsequence_case_insensitively() {
        assert!(fuzzy_matches("main.rs", "mn"));
        assert!(fuzzy_matches("main.rs", "MN"));
        assert!(fuzzy_matches("main.rs", "main.rs"));
        assert!(!fuzzy_matches("main.rs", "nm"));
        assert!(!fuzzy_matches("main.rs", "xyz"));
    }

    #[test]
    fn common_prefix_of_one_candidate_is_the_candidate_itself() {
        assert_eq!(common_prefix(&["/export"]), Some("/export"));
    }

    #[test]
    fn common_prefix_of_diverging_candidates_stops_at_the_divergence() {
        assert_eq!(common_prefix(&["/exit", "/export"]), Some("/ex"));
    }

    #[test]
    fn common_prefix_of_candidates_with_nothing_shared_is_empty() {
        assert_eq!(common_prefix(&["abc", "xyz"]), Some(""));
    }

    #[test]
    fn common_prefix_of_no_candidates_is_none() {
        assert_eq!(common_prefix(&[]), None);
    }

    #[test]
    fn complete_at_path_fuzzy_matches_and_marks_directories() {
        let dir = temp_dir("complete-at-path");
        std::fs::write(dir.join("main.rs"), "").unwrap();
        std::fs::write(dir.join("lib.rs"), "").unwrap();
        std::fs::create_dir(dir.join("target")).unwrap();
        let dir_str = dir.to_str().unwrap();

        let mut candidates = complete_at_path(&format!("{dir_str}/mn"));
        candidates.sort();
        assert_eq!(candidates, vec![format!("{dir_str}/main.rs")]);

        let mut all = complete_at_path(&format!("{dir_str}/"));
        all.sort();
        assert_eq!(
            all,
            vec![
                format!("{dir_str}/lib.rs"),
                format!("{dir_str}/main.rs"),
                format!("{dir_str}/target/"),
            ]
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn complete_at_path_of_an_unreadable_directory_is_empty() {
        assert!(complete_at_path("/this/does/not/exist/at/all/frag").is_empty());
    }

    #[test]
    fn complete_repl_line_completes_an_unambiguous_slash_command() {
        let completion = complete_repl_line(b"/tr").expect("should complete");
        assert_eq!(completion.common_prefix_len, 0);
        assert_eq!(completion.replacement, "/tree");
    }

    #[test]
    fn complete_repl_line_partially_completes_an_ambiguous_slash_command() {
        // `/exit` and `/export` share only `/ex` beyond the typed `/e`.
        let completion = complete_repl_line(b"/e").expect("should partially complete");
        assert_eq!(completion.replacement, "/ex");
    }

    #[test]
    fn complete_repl_line_returns_none_when_no_slash_command_matches() {
        assert!(complete_repl_line(b"/zzz").is_none());
    }

    #[test]
    fn complete_repl_line_does_not_complete_slash_commands_past_the_first_word() {
        // A `/`-looking token that isn't the first word (e.g. a `/fork`
        // argument) isn't a command name to complete.
        assert!(complete_repl_line(b"hello /tr").is_none());
    }

    #[test]
    fn complete_repl_line_completes_an_at_path_reference_mid_line() {
        let dir = temp_dir("complete-repl-line-at");
        std::fs::write(dir.join("main.rs"), "").unwrap();
        let dir_str = dir.to_str().unwrap();

        let buf = format!("please read @{dir_str}/mn").into_bytes();
        let completion = complete_repl_line(&buf).expect("should complete");
        assert_eq!(completion.common_prefix_len, "please read ".len());
        assert_eq!(completion.replacement, format!("@{dir_str}/main.rs"));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
