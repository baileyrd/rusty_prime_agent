//! Harness-wide error type.
//!
//! Every fallible operation in this crate returns [`HarnessError`]. OS
//! failures (process spawn, socket I/O) surface as `std::io::Error` --
//! `rusty_tokio::process`/`rusty_tokio::io` already normalize both their
//! own and `rustils`' underlying errors to that type, so this crate
//! wraps `std::io::Error` with its own session/worker/transport context
//! (which subsystem, which operation, which path) rather than a
//! `rustils`-specific error type.

use std::fmt;
use std::path::PathBuf;

/// What this process/operation was doing when a [`HarnessError`] occurred.
///
/// Deliberately a closed, small set: every fallible entry point in this
/// crate can name exactly one of these, and the set is what
/// `ARCHITECTURE.md` and log output key off. Not `#[non_exhaustive]`:
/// this enum spans only this crate's own module boundaries and is meant
/// to be matched exhaustively at the CLI's error -> exit-code mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    /// Supervisor startup, shutdown, or public-socket handling.
    Daemon,
    /// Worker process spawn, liveness check, or kill.
    Worker,
    /// Session transcript or state-file persistence.
    Session,
    /// Session catalog scan.
    Catalog,
    /// CLI argument parsing / dispatch.
    Cli,
    /// The `rp-server` sidecar (spawn, health check) or a `ModelProvider`
    /// backend's HTTP call to it -- see `rp_server`/`provider::OllamaProvider`.
    Provider,
}

impl fmt::Display for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Context::Daemon => "daemon",
            Context::Worker => "worker",
            Context::Session => "session",
            Context::Catalog => "catalog",
            Context::Cli => "cli",
            Context::Provider => "provider",
        };
        f.write_str(s)
    }
}

/// The one error type returned across this crate's module boundaries.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    /// Malformed or unexpected wire data (bad JSON, unknown command,
    /// protocol-version mismatch, truncated frame).
    #[error("{context}: protocol error: {message}")]
    Protocol { context: Context, message: String },

    /// A session-lease or session-lifecycle conflict that is expected to
    /// happen (concurrent open, unknown id) and is reported back to the
    /// client as a structured condition rather than a bug.
    #[error("{context}: {message}")]
    Conflict { context: Context, message: String },

    /// (De)serialization of a persisted or wire JSON value failed.
    #[error("{context}: json error at {path:?}: {source}")]
    Json {
        context: Context,
        path: Option<PathBuf>,
        #[source]
        source: serde_json::Error,
    },

    /// Any OS-level failure: process spawn/liveness, socket I/O, or a
    /// plain `std::fs` call for the session directory tree.
    #[error("{context}: io error at {path:?}: {source}")]
    Io {
        context: Context,
        path: Option<PathBuf>,
        #[source]
        source: std::io::Error,
    },

    /// CLI usage error (bad arguments, unknown subcommand).
    #[error("usage: {message}")]
    Usage { message: String },
}

impl HarnessError {
    pub fn protocol(context: Context, message: impl Into<String>) -> Self {
        HarnessError::Protocol {
            context,
            message: message.into(),
        }
    }

    pub fn conflict(context: Context, message: impl Into<String>) -> Self {
        HarnessError::Conflict {
            context,
            message: message.into(),
        }
    }

    pub fn json(context: Context, path: Option<PathBuf>, source: serde_json::Error) -> Self {
        HarnessError::Json {
            context,
            path,
            source,
        }
    }

    pub fn io(context: Context, path: Option<PathBuf>, source: std::io::Error) -> Self {
        HarnessError::Io {
            context,
            path,
            source,
        }
    }

    /// True when this error represents an expected, recoverable condition
    /// (as opposed to an unexpected bug) -- used by the CLI to decide
    /// whether to print a plain message or a full debug dump.
    pub fn is_conflict(&self) -> bool {
        matches!(self, HarnessError::Conflict { .. })
    }
}

pub type Result<T, E = HarnessError> = std::result::Result<T, E>;

/// Extension trait for attaching harness context to a plain
/// `std::io::Result` -- the common shape `procutil`/`rusty_tokio` calls
/// return.
pub trait IoResultExt<T> {
    fn ctx(self, context: Context) -> Result<T>;
}

impl<T> IoResultExt<T> for std::io::Result<T> {
    fn ctx(self, context: Context) -> Result<T> {
        self.map_err(|e| HarnessError::io(context, None, e))
    }
}
