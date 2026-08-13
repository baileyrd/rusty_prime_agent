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
    /// See `Request::SessionNew::parent_id`'s own doc comment. Same
    /// "only meaningful for `WorkerMode::New`" reasoning as `goal`.
    pub parent_id: Option<String>,
    /// See `Request::SessionNew::thinking`'s own doc comment. Same
    /// "always supplied by the daemon at spawn time" treatment as
    /// `model` (not `goal`/`parent_id`'s "New-only" treatment), since
    /// it's consumed by `build_provider` on every worker spawn --
    /// `RustyProviderModel` needs it for every subsequent request, not
    /// just at session-creation time.
    pub thinking: Option<String>,
    /// See `Request::SessionNew::tools`'s own doc comment. Same
    /// "only meaningful for `WorkerMode::New`" reasoning as `goal`/
    /// `parent_id`: unlike `thinking`, nothing at spawn time other than
    /// `AgentSession::create` reads this -- `AgentSession::prompt`'s
    /// tool-calling loop reads the offered tool set back off
    /// `state.tools` (already persisted by then), not off `WorkerArgs`.
    pub tools: Option<String>,
    /// See `protocol::Request::SessionNew::runtime`'s own doc comment.
    /// Same "always supplied by the daemon at spawn time" treatment as
    /// `model`/`thinking` (not `tools`'s "New-only" treatment): unlike
    /// `state.tools`, which `AgentSession::prompt` re-reads off persisted
    /// state on every call, [`run`] must pick a `ToolRuntime`
    /// implementation *before* `AgentSession::create`/`recover` even run
    /// -- there is no persisted state yet to read it back from at that
    /// point on a fresh spawn, and re-deriving it from `state.json` on a
    /// resume/recover respawn is exactly what `daemon::Supervisor::
    /// ensure_worker_running` does, mirroring `thinking`.
    pub runtime: Option<String>,
    /// See `protocol::SessionState::rlm_depth`'s own doc comment. Same
    /// "always supplied by the daemon at spawn time" treatment as
    /// `runtime` -- [`bootstrap_kernel`] needs it *before* `AgentSession::
    /// create`/`recover` even run (to inject `RLM_DEPTH` into the kernel
    /// at startup), so it can't be deferred to a persisted-state read
    /// the way `tools` is.
    pub rlm_depth: Option<u32>,
    /// See `protocol::SessionState::rlm_max_depth`'s own doc comment.
    /// Same treatment as `rlm_depth`.
    pub rlm_max_depth: Option<u32>,
    /// See `protocol::SessionState::spawned_from_sequence`'s own doc
    /// comment. Same "only meaningful for `WorkerMode::New`" treatment as
    /// `goal`/`parent_id` (not `rlm_depth`/`rlm_max_depth`'s "needed
    /// before `bootstrap_kernel` runs" reasoning) -- nothing at spawn
    /// time other than `AgentSession::create` reads this.
    pub spawned_from_sequence: Option<u64>,
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
    thinking: Option<String>,
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
        port, model, thinking,
    )))
}

/// Builds this worker's `ToolRuntime`. `runtime.as_deref() != Some("ipython")`
/// (the ordinary case) is `NoopToolRuntime`, no kernel subprocess spawned
/// at all. Selected here, before `AgentSession::create`/`recover` even
/// run -- see `WorkerArgs::runtime`'s own doc comment for why this can't
/// wait until `state.tools`-style persisted-state lookup the way the
/// tool-calling loop's own tool set does.
fn build_tool_runtime(session_dir: &Path, runtime: Option<&str>) -> Box<dyn ToolRuntime> {
    match runtime {
        Some("ipython") => Box::new(crate::ipython_runtime::IpythonKernelRuntime::new(
            session_dir.to_path_buf(),
        )),
        _ => Box::new(NoopToolRuntime),
    }
}

/// One-time kernel setup once `tool_runtime` is up. Defines
/// `rlm_heartbeat()` in the kernel's own globals -- parity with
/// `prime-agent`'s kernel-side manual re-entry trigger (`session::
/// execute_python_tool_call` watches for the marker it prints, see that
/// module's own doc comment) -- and a `host_request(kind, payload=None)`
/// coroutine (parity with `rlm-runtime.md`'s `rlm.host_request(...)`),
/// and, only when `skills::discover` finds any, also puts `paths::
/// global_skills_dir` on the kernel's `sys.path` so `import <skill-name>`
/// resolves. Always exactly one `execute_request` when `--runtime
/// ipython` (previously zero when no skills were installed); propagates
/// a genuine execution failure via `?`, same "fail loudly on a broken
/// startup precondition" convention as `build_provider`'s own callers.
///
/// `rlm_heartbeat(every=None)` -- the optional `every` argument (a
/// duration string like `"10m"`, parity with `prime-agent`'s own
/// `rlm_heartbeat.create(interval=...)`) rides along after the marker on
/// the same printed line (`marker + (every or "")`), since a plain
/// stdout `print()` is the only channel from kernel code back to this
/// process -- `session::execute_python_tool_call`'s marker parsing
/// splits it back out. `rlm_heartbeat` itself is *not* migrated onto
/// `host_request` here: it works today, and the marker hack costs
/// nothing extra to keep running alongside the new channel.
///
/// `host_request(kind, payload=None)` opens a Jupyter comm targeting
/// `"host.request"` and returns an `asyncio.Future` that resolves with
/// whatever `ipython_runtime::IpythonKernelRuntime::resume_execute`'s
/// caller replies with -- confirmed against a real `ipykernel` before
/// writing any Rust (see `ipython_runtime`'s own module doc comment) that
/// this requires monkeypatching `kernel.control_handlers['comm_msg']`
/// (stock `ipykernel` only ever routes `comm_msg` through `shell`, never
/// `control`, despite `rlm-runtime.md` describing `control` as where
/// host-request replies travel) and resolving the future via
/// `loop.call_soon_threadsafe` (`rlm-runtime.md`'s own stated reason:
/// "the control handler may run on another thread").
///
/// `rlm(task, name=None, model=None)` -- parity with `prime-agent`'s
/// kernel-callable `rlm(...)`, `packages/coding-agent/docs/rlm.md` --
/// is the first (and so far only) real `host_request` kind,
/// `session::AgentSession::handle_rlm_run` on the other end. Deliberately
/// a thin wrapper, not a distinct comm target of its own: `rlm("task")`
/// is exactly `await host_request("rlm.run", {"task": "task"})`, matching
/// the "one comm target, typed request kinds" shape `rlm-runtime.md`
/// itself describes ("Bundled Python skills such as `goal` call
/// `rlm.host_request("goal.get", ...)`").
///
/// `RLM_DEPTH`/`RLM_MAX_DEPTH` are also injected as plain kernel globals
/// (ints, not functions) -- parity with `rlm-runtime.md`'s "Children
/// receive incremented `RLM_DEPTH`, the inherited maximum depth." The
/// actual depth-limit *check* happens server-side in `AgentSession::
/// handle_rlm_run` (which has `self.state.rlm_depth`/`rlm_max_depth`
/// already loaded, no round trip needed); these globals exist so kernel
/// code can also read/display them directly, matching what a real
/// `prime-agent` session exposes.
/// The `pi` object every discovered extension's own `register(pi)`
/// receives -- bounded parity with `prime-agent`'s `ExtensionAPI`
/// (`extensions.md`), scoped to the two pieces `PARITY.md`'s own
/// "Extensions" entry calls out as the tractable first slice:
/// `pi.on("pre_tool_call", handler)` and `pi.register_command(name,
/// handler, description="")`. Deliberately *synchronous* Python methods
/// (no `await host_request(...)`, unlike `goal`/`agent_message`/
/// `compact`/`rlm(...)` above) -- registration itself never needs a
/// round trip back into Rust; it only ever appends to `pi`'s own
/// in-kernel lists/dict. This matters for a real, load-bearing reason:
/// an extension's own top-level `register(pi)` call happens from inside
/// a plain `import <name>` statement below, and plain module-level code
/// executed via `import` does **not** get IPython's own top-level-
/// `await` support the way a real `execute_request` cell (this whole
/// bootstrap included) does -- an `async def register(pi): await
/// pi.on(...)` pattern would need the *caller* to `await
/// extension.register(pi)` from bootstrap's own top level for the
/// `await` inside `register` to work at all, which would in turn need
/// this function's single `tool_runtime.execute(&code)` call below to
/// handle a `pending_host_request` pause -- machinery `bootstrap_kernel`
/// doesn't have (unlike `execute_python_tool_call`'s own pause/resume
/// loop) and doesn't need, since nothing here has to notify Rust
/// mid-registration. What Rust actually needs (which commands got
/// registered, whether any hook exists) is read back in one shot at the
/// very end of this same code string instead -- see `EXTENSION_REGISTRY_MARKER`.
const EXTENSION_REGISTRY_MARKER: &str = "___RPA_EXTENSION_REGISTRY___";

async fn bootstrap_kernel(
    state_root: &Path,
    tool_runtime: &mut dyn ToolRuntime,
    rlm_depth: u32,
    rlm_max_depth: u32,
) -> Result<crate::extensions::ExtensionRegistry> {
    let mut code = format!(
        "def rlm_heartbeat(every=None):\n    print({marker:?} + (every or \"\"))\n    return \"heartbeat requested\"\n\n\
         RLM_DEPTH = {rlm_depth}\n\
         RLM_MAX_DEPTH = {rlm_max_depth}\n\n\
         import asyncio\n\
         from ipykernel.comm import Comm\n\
         _host_request_kernel = get_ipython().kernel\n\
         _host_request_kernel.control_handlers['comm_msg'] = _host_request_kernel.comm_manager.comm_msg\n\
         _host_request_kernel.control_handlers['comm_close'] = _host_request_kernel.comm_manager.comm_close\n\
         _host_request_loop = asyncio.get_event_loop()\n\
         def host_request(kind, payload=None):\n\
         \x20   comm = Comm(target_name='host.request', data={{'kind': kind, **(payload or {{}})}})\n\
         \x20   fut = _host_request_loop.create_future()\n\
         \x20   def _on_msg(msg):\n\
         \x20       def _resolve():\n\
         \x20           if not fut.done():\n\
         \x20               fut.set_result(msg['content']['data'])\n\
         \x20       _host_request_loop.call_soon_threadsafe(_resolve)\n\
         \x20   comm.on_msg(_on_msg)\n\
         \x20   return fut\n\n\
         async def rlm(task, name=None, model=None):\n\
         \x20   payload = {{'task': task}}\n\
         \x20   if name is not None:\n\
         \x20       payload['name'] = name\n\
         \x20   if model is not None:\n\
         \x20       payload['model'] = model\n\
         \x20   return await host_request('rlm.run', payload)\n\n\
         async def rlm_list_subagents():\n\
         \x20   return await host_request('rlm.list_subagents')\n\n\
         async def rlm_delete_subagent(id):\n\
         \x20   return await host_request('rlm.delete_subagent', {{'id': id}})\n\n\
         class _Goal:\n\
         \x20   async def get(self):\n\
         \x20       return await host_request('goal.get')\n\
         \x20   async def create(self, task, token_budget=None):\n\
         \x20       payload = {{'task': task}}\n\
         \x20       if token_budget is not None:\n\
         \x20           payload['token_budget'] = token_budget\n\
         \x20       return await host_request('goal.create', payload)\n\
         \x20   async def complete(self):\n\
         \x20       return await host_request('goal.complete')\n\
         goal = _Goal()\n\n\
         class _AgentMessage:\n\
         \x20   async def send(self, message, receiver_role='parent', receiver_name=None):\n\
         \x20       payload = {{'message': message, 'receiver_role': receiver_role}}\n\
         \x20       if receiver_name is not None:\n\
         \x20           payload['receiver_name'] = receiver_name\n\
         \x20       return await host_request('agent_message.send', payload)\n\
         agent_message = _AgentMessage()\n\n\
         class _Compact:\n\
         \x20   async def now(self, instructions=None):\n\
         \x20       payload = {{}}\n\
         \x20       if instructions is not None:\n\
         \x20           payload['instructions'] = instructions\n\
         \x20       return await host_request('compact.now', payload)\n\
         compact = _Compact()\n\n\
         class _Pi:\n\
         \x20   def __init__(self):\n\
         \x20       self._pre_tool_call_hooks = []\n\
         \x20       self._commands = {{}}\n\
         \x20   def on(self, event, handler):\n\
         \x20       if event != 'pre_tool_call':\n\
         \x20           raise ValueError(\n\
         \x20               'unknown event %r -- only \\'pre_tool_call\\' is supported'\n\
         \x20               % (event,)\n\
         \x20           )\n\
         \x20       self._pre_tool_call_hooks.append(handler)\n\
         \x20   def register_command(self, name, handler, description=''):\n\
         \x20       self._commands[name] = (handler, description)\n\
         \x20   def _run_pre_tool_call_hooks(self, tool_name, arguments):\n\
         \x20       for hook in self._pre_tool_call_hooks:\n\
         \x20           result = hook(tool_name, arguments)\n\
         \x20           if result:\n\
         \x20               return result\n\
         \x20       return None\n\
         \x20   def _run_command(self, name, args):\n\
         \x20       entry = self._commands.get(name)\n\
         \x20       if entry is None:\n\
         \x20           return None\n\
         \x20       return entry[0](args)\n\
         pi = _Pi()\n",
        marker = crate::session::HEARTBEAT_MARKER
    );

    let skills = crate::skills::discover(state_root)?;
    if !skills.is_empty() {
        let skills_dir = paths::global_skills_dir(state_root);
        let skills_dir_json = serde_json::to_string(&skills_dir.display().to_string())
            .map_err(|e| HarnessError::json(Context::Runtime, None, e))?;
        code.push_str(&format!(
            "import sys; sys.path.insert(0, {skills_dir_json})\n"
        ));
    }

    // Parity with a bounded slice of `prime-agent`'s extension system --
    // see `extensions.rs`'s own module doc comment for the full design.
    // Each discovered extension is `import`ed (a plain, synchronous
    // statement -- see `_Pi`'s own doc comment above for why this has
    // to stay synchronous) and, if it defines a top-level `register`
    // function, called with the shared `pi` object so it can call
    // `pi.on(...)`/`pi.register_command(...)`.
    let extensions = crate::extensions::discover(state_root)?;
    if !extensions.is_empty() {
        let extensions_dir = paths::global_extensions_dir(state_root);
        let extensions_dir_json = serde_json::to_string(&extensions_dir.display().to_string())
            .map_err(|e| HarnessError::json(Context::Runtime, None, e))?;
        code.push_str(&format!(
            "import sys; sys.path.insert(0, {extensions_dir_json})\n"
        ));
        for extension in &extensions {
            let name_ident = &extension.name;
            code.push_str(&format!(
                "import {name_ident}\n\
                 if hasattr({name_ident}, 'register'):\n\
                 \x20   {name_ident}.register(pi)\n"
            ));
        }
    }

    // One marker-prefixed `print` at the very end reads back everything
    // registered above in this same call -- the same technique
    // `session::HEARTBEAT_MARKER` already established, chosen for the
    // same reason: a plain `print()` is the only channel from kernel
    // code back to this process that doesn't need the pause/resume
    // machinery a real `host_request` round trip would.
    code.push_str(&format!(
        "import json as __rpa_json\n\
         print({EXTENSION_REGISTRY_MARKER:?} + __rpa_json.dumps({{\n\
         \x20   'commands': {{k: v[1] for k, v in pi._commands.items()}},\n\
         \x20   'has_pre_tool_call_hook': len(pi._pre_tool_call_hooks) > 0,\n\
         }}))\n"
    ));

    let outcome = tool_runtime.execute(&code).await?;
    parse_extension_registry(&outcome.stdout)
}

/// Extracts the JSON payload `EXTENSION_REGISTRY_MARKER` prefixes in
/// `bootstrap_kernel`'s own final `print` line -- the exact same
/// "find the marker, parse what follows to the end of that line"
/// technique `session::extract_heartbeat_marker` already uses, just
/// parsing structured JSON instead of a bare duration string. A missing
/// marker (shouldn't happen -- this function's own `code` string always
/// ends with it -- but a real kernel could in principle fail partway
/// through, e.g. an extension's own `register(pi)` raising) reads as an
/// empty registry rather than a hard error: bootstrap still succeeded
/// enough to be usable, just without whatever extension state didn't
/// make it into the final print.
fn parse_extension_registry(stdout: &str) -> Result<crate::extensions::ExtensionRegistry> {
    let Some(start) = stdout.find(EXTENSION_REGISTRY_MARKER) else {
        return Ok(crate::extensions::ExtensionRegistry::default());
    };
    let after = &stdout[start + EXTENSION_REGISTRY_MARKER.len()..];
    let line_len = after.find('\n').unwrap_or(after.len());
    let json = &after[..line_len];
    serde_json::from_str(json).map_err(|e| HarnessError::json(Context::Runtime, None, e))
}

/// The worker process entrypoint (`harness __worker-main`).
pub async fn run(args: WorkerArgs) -> Result<()> {
    let session_dir = paths::session_dir(&args.state_root, &args.session_id);
    let mut tool_runtime = build_tool_runtime(&session_dir, args.runtime.as_deref());
    tool_runtime.start().await?;
    let extension_registry = if args.runtime.as_deref() == Some("ipython") {
        bootstrap_kernel(
            &args.state_root,
            tool_runtime.as_mut(),
            args.rlm_depth.unwrap_or(0),
            args.rlm_max_depth
                .unwrap_or(crate::session::DEFAULT_RLM_MAX_DEPTH),
        )
        .await?
    } else {
        crate::extensions::ExtensionRegistry::default()
    };
    let provider = build_provider(&args.state_root, args.model.clone(), args.thinking.clone())?;

    let mut session = match args.mode {
        WorkerMode::New => {
            AgentSession::create(
                &args.state_root,
                args.session_id.clone(),
                crate::session::NewSessionMeta {
                    name: args.name.clone(),
                    model: args.model.clone(),
                    goal: args.goal.clone(),
                    parent_id: args.parent_id.clone(),
                    thinking: args.thinking.clone(),
                    tools: args.tools.clone(),
                    runtime: args.runtime.clone(),
                    rlm_depth: args.rlm_depth,
                    rlm_max_depth: args.rlm_max_depth,
                    spawned_from_sequence: args.spawned_from_sequence,
                },
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
    session.install_extension_registry(extension_registry);
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
        Request::SessionPrompt { text, images, .. } => {
            let entry = session
                .lock()
                .await
                .prompt_with_images(text, images)
                .await?;
            conn.write_response(Context::Worker, &Response::SessionPromptAck { entry })
                .await
        }
        Request::SessionRename { name, .. } => {
            session.lock().await.rename(name.clone()).await?;
            conn.write_response(Context::Worker, &Response::SessionRenameAck { name })
                .await
        }
        Request::SessionCompact { instructions, .. } => {
            let (compacted, summary) = session.lock().await.compact_now(instructions).await?;
            conn.write_response(
                Context::Worker,
                &Response::SessionCompactAck { compacted, summary },
            )
            .await
        }
        Request::SessionExtensionCommand { command, args, .. } => {
            let output = session
                .lock()
                .await
                .invoke_extension_command(&command, &args)
                .await?;
            conn.write_response(
                Context::Worker,
                &Response::SessionExtensionCommandResult { output },
            )
            .await
        }
        Request::SessionSetActiveLeaf { sequence, .. } => {
            // Unlike `SessionRename`/`SessionCompact` (never fail --
            // renaming always succeeds, compaction's own no-op cases are
            // still `Ok`), an unknown `sequence` is a real, expected
            // business error (`session::AgentSession::set_active_leaf`'s
            // own doc comment) -- matched explicitly and turned into a
            // `Response::Error` sent back over *this* connection instead
            // of propagated via `?`, which would just drop the
            // connection (`run`'s own accept loop logs a bare `Err` to
            // the worker's stderr and moves on, no response at all) and
            // leave the daemon's own relay -- and the client past it --
            // seeing an opaque "closed before responding" instead of the
            // real conflict message. The same explicit-match-not-`?`
            // shape `daemon::Supervisor::handle_session_fork` already
            // uses for its own genuinely-failable step.
            match session.lock().await.set_active_leaf(sequence).await {
                Ok(active_leaf_sequence) => {
                    conn.write_response(
                        Context::Worker,
                        &Response::SessionSetActiveLeafAck {
                            active_leaf_sequence,
                        },
                    )
                    .await
                }
                Err(err) => {
                    conn.write_response(
                        Context::Worker,
                        &Response::Error {
                            message: err.to_string(),
                            conflict: true,
                        },
                    )
                    .await
                }
            }
        }
        Request::SessionBranchSummarize {
            branch_leaf_sequence,
            ..
        } => {
            // Same explicit-match-not-`?` shape as `SessionSetActiveLeaf`
            // just above -- an unknown `branch_leaf_sequence` is a real
            // conflict, not a bug to drop the connection over.
            match session
                .lock()
                .await
                .branch_summarize(branch_leaf_sequence)
                .await
            {
                Ok((summarized, summary)) => {
                    conn.write_response(
                        Context::Worker,
                        &Response::SessionBranchSummarizeAck {
                            summarized,
                            summary,
                        },
                    )
                    .await
                }
                Err(err) => {
                    conn.write_response(
                        Context::Worker,
                        &Response::Error {
                            message: err.to_string(),
                            conflict: true,
                        },
                    )
                    .await
                }
            }
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
        Request::AttributeChildUsage { child_id } => {
            let attributed = session
                .lock()
                .await
                .attribute_child_usage(&child_id)
                .await?;
            conn.write_response(
                Context::Worker,
                &Response::AttributeChildUsageAck { attributed },
            )
            .await
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
    meta: crate::session::NewSessionMeta,
) -> Result<u32> {
    use rusty_tokio::process::{Command, Stdio};

    let crate::session::NewSessionMeta {
        name,
        model,
        goal,
        parent_id,
        thinking,
        tools,
        runtime,
        rlm_depth,
        rlm_max_depth,
        spawned_from_sequence,
    } = meta;

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
    if let Some(parent_id) = &parent_id {
        cmd.arg("--parent-id").arg(parent_id);
    }
    if let Some(thinking) = &thinking {
        cmd.arg("--thinking").arg(thinking);
    }
    if let Some(tools) = &tools {
        cmd.arg("--tools").arg(tools);
    }
    if let Some(runtime) = &runtime {
        cmd.arg("--runtime").arg(runtime);
    }
    if let Some(rlm_depth) = rlm_depth {
        cmd.arg("--rlm-depth").arg(rlm_depth.to_string());
    }
    if let Some(rlm_max_depth) = rlm_max_depth {
        cmd.arg("--rlm-max-depth").arg(rlm_max_depth.to_string());
    }
    if let Some(spawned_from_sequence) = spawned_from_sequence {
        cmd.arg("--spawned-from-sequence")
            .arg(spawned_from_sequence.to_string());
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
