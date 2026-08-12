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
    Request, Response, Role, SessionEvent, SessionStatus, TranscriptEntry,
};
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
    } = meta;
    let mut conn = connect(state_root).await?;
    conn.write_request(
        Context::Daemon,
        &Request::SessionNew {
            name,
            model,
            goal,
            parent_id,
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
/// vs. follow-up queuing, `/tree`/`/fork`/`/clone`/`/compact`/`/export`/
/// `/share`) -- those stay out of scope, see `PARITY.md`. Replays the
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

    use std::io::BufRead;
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| HarnessError::io(Context::Cli, None, e))?;
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        if text == "/exit" || text == "/quit" {
            break;
        }
        let entry = send_prompt(state_root, &session_id, text.to_string()).await?;
        match mode {
            OutputMode::Json => print_json(&Response::SessionPromptAck { entry }),
            OutputMode::Text => print_entry(&entry),
        }
    }
    Ok(())
}

pub async fn session_prompt(
    state_root: &Path,
    session_id: String,
    text: String,
    mode: OutputMode,
) -> Result<()> {
    let entry = send_prompt(state_root, &session_id, text).await?;
    match mode {
        OutputMode::Json => print_json(&Response::SessionPromptAck { entry }),
        OutputMode::Text => print_entry(&entry),
    }
    Ok(())
}

/// Shared by [`session_prompt`] and [`session_autonomous`]'s own
/// continuation turns -- the latter needs the raw entry, not printed
/// text, and drives many of these in a loop rather than just one.
async fn send_prompt(
    state_root: &Path,
    session_id: &str,
    text: String,
) -> Result<crate::protocol::TranscriptEntry> {
    let mut conn = connect(state_root).await?;
    conn.write_request(
        Context::Daemon,
        &Request::SessionPrompt {
            session_id: session_id.to_string(),
            text,
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
        let providers = crate::rp_server::known_providers();
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
/// stream; this is a one-shot read of its first event only).
async fn fetch_transcript_snapshot(
    state_root: &Path,
    session_id: &str,
) -> Result<Vec<TranscriptEntry>> {
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
        Some(SessionEvent::Snapshot { transcript, .. }) => Ok(transcript),
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
