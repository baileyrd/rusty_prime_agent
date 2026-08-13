//! The one deliberate ports-and-adapters seam this project leaves open
//! (see the project brief's "Architecture Constraints" and
//! `ARCHITECTURE.md`'s "ToolRuntime Trait Boundary"): the connection to
//! the model-facing IPython kernel, which stays an external process this
//! host drives (Phase 1 non-goal: "The IPython kernel / its transport --
//! design the trait boundary ... but implement only a no-op/mock
//! runtime").
//!
//! Every other subsystem in this crate (process lifecycle, local IPC)
//! calls `rustils`/`rusty_tokio` directly rather than wrapping them
//! behind a speculative trait -- see `crate::process`/`crate::net`. This
//! is the one boundary that genuinely has a second backend coming
//! (Phase 2's real kernel subprocess), so it alone gets the trait.
//!
//! Object-safe (`Box<dyn ToolRuntime>` is what `AgentSession` holds) with
//! boxed-future async methods rather than a real `async fn` in the
//! trait: Phase 2's kernel connection will genuinely need to `.await` on
//! subprocess I/O (`rusty_tokio` pipes/stdio), and a sync-only trait
//! today would just have to be redesigned then. No `async-trait`
//! dependency pulled in for this -- it is one trait, three methods, and
//! the manual `Pin<Box<dyn Future<...>>>` desugaring is the same one
//! that macro expands to.

use std::future::Future;
use std::pin::Pin;

use crate::error::Result;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What one `execute`/`resume_execute` call produced. Grown, as this
/// module's own doc comment anticipated, to carry "typed host requests"
/// once `ipython_runtime::IpythonKernelRuntime` actually implemented
/// them: `pending_host_request` is `Some` exactly when the kernel is
/// blocked mid-cell awaiting a reply (`await host_request(...)`) rather
/// than finished -- the caller must compute a reply and call
/// [`ToolRuntime::resume_execute`], not treat this `ExecutionOutcome` as
/// final. `stdout`/`result` still only ever reflect what was produced
/// *up to* that pause; a caller looping through possibly-several pauses
/// within one cell is responsible for concatenating `stdout` across
/// calls itself (see `session::execute_python_tool_call`).
#[derive(Debug, Clone, Default)]
pub struct ExecutionOutcome {
    pub stdout: String,
    pub result: Option<String>,
    pub pending_host_request: Option<HostRequest>,
}

/// One typed host request the kernel is blocked awaiting a reply to --
/// parity with `rlm-runtime.md`'s "comm target: host.request" (see
/// `ipython_runtime`'s own doc comment for the full mechanism: a Jupyter
/// comm opened by kernel-side code, replied to over the `control`
/// channel to avoid deadlocking the `shell` channel it was opened on).
/// `kind`/`payload` are exactly what the kernel-side `host_request(kind,
/// payload)` call passed, opaque to `ToolRuntime` itself -- interpreting
/// `kind` and producing a reply is the caller's job (`AgentSession`),
/// not this trait's.
#[derive(Debug, Clone)]
pub struct HostRequest {
    /// The Jupyter comm id the reply must be addressed to (see
    /// `ToolRuntime::resume_execute`).
    pub comm_id: String,
    pub kind: String,
    pub payload: serde_json::Value,
}

/// The host-side handle to a model-facing code execution environment.
/// Backed for real by `ipython_runtime::IpythonKernelRuntime` when a
/// session opts in (`session new --runtime ipython`); every other
/// session gets [`NoopToolRuntime`].
pub trait ToolRuntime: Send + Sync {
    /// Bring the runtime up (spawn and handshake with the kernel
    /// subprocess). Called once per `AgentSession` lifetime, before the
    /// first `execute`.
    fn start(&mut self) -> BoxFuture<'_, Result<()>>;

    /// Run one turn's worth of code and return what it produced -- or,
    /// if the kernel blocks mid-cell on `await host_request(...)`, what
    /// it produced *so far* plus `pending_host_request`. A caller that
    /// sees `pending_host_request` set must compute a reply and call
    /// [`resume_execute`](Self::resume_execute), not treat the call as
    /// finished.
    fn execute(&mut self, code: &str) -> BoxFuture<'_, Result<ExecutionOutcome>>;

    /// Replies to a `pending_host_request` from a prior `execute`/
    /// `resume_execute` call (addressed by `comm_id`) and continues
    /// draining the kernel's response until it either completes or
    /// blocks on another host request -- a cell may `await
    /// host_request(...)` more than once, so a caller must loop calling
    /// this until the returned `ExecutionOutcome.pending_host_request`
    /// is `None`. [`NoopToolRuntime`] never produces a pending host
    /// request in the first place, so this is never reachable on it in
    /// practice; its own implementation errors loudly rather than
    /// pretending to be capable of it.
    fn resume_execute(
        &mut self,
        comm_id: &str,
        reply: serde_json::Value,
    ) -> BoxFuture<'_, Result<ExecutionOutcome>>;

    /// Tear the runtime down (terminate the kernel subprocess).
    fn shutdown(&mut self) -> BoxFuture<'_, Result<()>>;
}

/// The Phase 1 stand-in: never spawns a process, never executes
/// anything, just echoes back that it was asked to. Exists so
/// `AgentSession` and the worker's lifecycle plumbing can be built and
/// tested end to end against the real trait shape before Phase 2's
/// kernel backend exists.
#[derive(Debug, Default)]
pub struct NoopToolRuntime;

impl ToolRuntime for NoopToolRuntime {
    fn start(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn execute(&mut self, code: &str) -> BoxFuture<'_, Result<ExecutionOutcome>> {
        let code = code.to_string();
        Box::pin(async move {
            Ok(ExecutionOutcome {
                stdout: String::new(),
                result: Some(format!("<no-op tool runtime: would execute `{code}`>")),
                pending_host_request: None,
            })
        })
    }

    fn resume_execute(
        &mut self,
        _comm_id: &str,
        _reply: serde_json::Value,
    ) -> BoxFuture<'_, Result<ExecutionOutcome>> {
        Box::pin(async {
            Err(crate::error::HarnessError::conflict(
                crate::error::Context::Runtime,
                "NoopToolRuntime never produces a pending host request, so resume_execute should never be called on it",
            ))
        })
    }

    fn shutdown(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}
