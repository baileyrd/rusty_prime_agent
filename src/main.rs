mod catalog;
mod cli;
mod client;
mod daemon;
mod error;
mod paths;
mod procutil;
mod protocol;
mod provider;
mod session;
mod tool_runtime;
mod transport;
mod worker;

use error::{HarnessError, Result};

#[rusty_tokio::main]
async fn main() {
    // Must run before this process spawns *anything* -- see the
    // function's own doc comment for the hang this prevents. Every CLI
    // invocation (including the hidden `__supervisor-main`/
    // `__worker-main` entrypoints, which go on to spawn further
    // children of their own) shares this one `main`, so doing it here,
    // first, covers every spawn call in the process.
    harden_inherited_stdio();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(err) = run(&args).await {
        eprintln!("harness: {err}");
        std::process::exit(exit_code(&err));
    }
}

/// Strips this process's own inherited stdin/stdout/stderr of their
/// "inheritable by a child process" property.
///
/// Without this, a `Stdio::Null`-everywhere, `detach()`ed spawn (every
/// spawn this project ever does -- `client::daemon_start`,
/// `worker::spawn`) does not actually stop this process's *own*
/// inherited stdio from leaking into that child:
///
/// - Windows: `platform-windows` uses `STARTF_USESTDHANDLES` with
///   explicit `NUL`-device handles for the *child's* three stdio slots,
///   but still passes `bInheritHandles = TRUE` to `CreateProcessW`
///   (needed for those explicit handles to cross at all) -- and
///   `bInheritHandles = TRUE` duplicates *every* currently-inheritable
///   handle in this process into the child's handle table, not just the
///   three named in `STARTUPINFO`. If this process's own stdout is
///   itself an inherited, inheritable pipe (exactly what happens when a
///   test harness or another tool captures this process's output via a
///   pipe), that pipe handle rides along uninvited.
/// - Unix: `posix_spawn`/`fork`+`exec` inherit every open fd without
///   `FD_CLOEXEC` by default, independent of whatever `Stdio` variant a
///   caller picked for the child's *own* fd 0/1/2 slots.
///
/// Either way, a detached child that outlives this process then holds
/// that handle/fd open forever, so a parent reading this process's
/// output until EOF (`std::process::Command::output`, exactly what this
/// project's own integration tests under `tests/` do) blocks
/// indefinitely -- not because this process is still running, but
/// because *something it spawned* still is. Caught by this project's own
/// `tests/session_lifecycle.rs` hanging on `daemon start` before this
/// function existed.
fn harden_inherited_stdio() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: `GetStdHandle` with one of the three documented
            // constants always returns a valid (possibly
            // `INVALID_HANDLE_VALUE`/null, both handled by
            // `SetHandleInformation` returning a checkable failure
            // rather than being unsound to call with) handle value; no
            // ownership is taken (this does not close or duplicate
            // anything), only its inherit flag is cleared.
            unsafe {
                let handle = GetStdHandle(which);
                // `SetHandleInformation` is a no-op-and-fail on a null
                // or `INVALID_HANDLE_VALUE` handle (no such standard
                // stream, or not currently redirected) -- calling it
                // unconditionally and ignoring the result is simpler
                // than special-casing those values, and no less safe.
                windows_sys::Win32::Foundation::SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
    #[cfg(unix)]
    {
        for fd in [0, 1, 2] {
            // SAFETY: `fcntl(fd, F_SETFD, FD_CLOEXEC)` on a standard,
            // always-valid-in-a-running-process fd number is a plain,
            // well-defined POSIX call; a failure (e.g. `fd` already
            // closed) is ignored rather than propagated -- this is
            // best-effort hygiene, not a correctness requirement this
            // process's own behavior depends on.
            unsafe {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
    }
}

fn exit_code(err: &HarnessError) -> i32 {
    if matches!(err, HarnessError::Usage { .. }) {
        2
    } else if err.is_conflict() {
        3
    } else {
        1
    }
}

async fn run(args: &[String]) -> Result<()> {
    let command = cli::parse(args)?;
    let state_root = paths::state_dir()?;
    let exe_path = std::env::current_exe().map_err(|e| HarnessError::io(error::Context::Cli, None, e))?;

    match command {
        cli::Command::DaemonStart => client::daemon_start(&state_root, &exe_path).await,
        cli::Command::DaemonStatus => client::daemon_status(&state_root).await,
        cli::Command::DaemonShutdown => client::daemon_shutdown(&state_root).await,
        cli::Command::SessionNew { name } => client::session_new(&state_root, name).await,
        cli::Command::SessionAttach { session_id } => client::session_attach(&state_root, session_id).await,
        cli::Command::SessionList => client::session_list(&state_root).await,
        cli::Command::SessionPrompt { session_id, text } => client::session_prompt(&state_root, session_id, text).await,
        cli::Command::SupervisorMain => daemon::run(state_root, exe_path).await,
        cli::Command::WorkerMain { session_id, state_root, mode, name } => {
            worker::run(worker::WorkerArgs { session_id, state_root, mode, name }).await
        }
    }
}
