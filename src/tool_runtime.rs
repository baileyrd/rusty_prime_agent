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

/// What one `execute` call produced. Deliberately small: Phase 1 never
/// inspects this beyond appending it to the transcript, and Phase 2's
/// real kernel connection is where "typed host requests return
/// authoritative operations to the TypeScript session" (the reference
/// architecture's own phrase) would grow this into something richer.
#[derive(Debug, Clone, Default)]
// `execute`'s return type: legitimately unconstructed in Phase 1 (see
// `execute`'s own `#[allow]` below) -- not oversight.
#[allow(dead_code)]
pub struct ExecutionOutcome {
    pub stdout: String,
    pub result: Option<String>,
}

/// The host-side handle to a model-facing code execution environment.
/// Phase 2 will back this with a real IPython kernel subprocess; Phase 1
/// backs it only with [`NoopToolRuntime`].
pub trait ToolRuntime: Send + Sync {
    /// Bring the runtime up (Phase 2: spawn and handshake with the
    /// kernel subprocess via `rustils::Command`/`rusty_tokio`). Called
    /// once per `AgentSession` lifetime, before the first `execute`.
    fn start(&mut self) -> BoxFuture<'_, Result<()>>;

    /// Run one turn's worth of code and return what it produced. Never
    /// called in Phase 1 -- tool execution is an explicit non-goal, and
    /// `AgentSession::prompt` never reaches for it. Kept (and kept
    /// compiling, via `NoopToolRuntime`) because the trait's whole
    /// purpose is to be the exact shape Phase 2's turn loop calls into;
    /// a method Phase 1 doesn't exercise is the intended state, not an
    /// unfinished one.
    #[allow(dead_code)]
    fn execute(&mut self, code: &str) -> BoxFuture<'_, Result<ExecutionOutcome>>;

    /// Tear the runtime down (Phase 2: terminate the kernel subprocess).
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
            })
        })
    }

    fn shutdown(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}
