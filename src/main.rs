mod catalog;
mod cli;
mod client;
mod daemon;
mod error;
mod http_client;
mod ipython_runtime;
mod mcp_client;
mod paths;
mod procutil;
mod prompt_template;
mod protocol;
mod provider;
mod rp_server;
mod schedule;
mod session;
mod sha256;
mod tool_runtime;
mod tools;
mod transport;
mod worker;
mod zmtp;

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
    match run(&args).await {
        // `std::process::exit`, not falling off the end of `main` --
        // same "blunt but honest" reasoning as `WorkerShutdown`'s/
        // `handle_daemon_shutdown`'s own identical calls, but now load-
        // bearing for every CLI invocation: `rp_server::ensure_running`
        // (reachable directly from a one-shot process since `harness
        // model list --detailed` added it, not just from the long-lived
        // daemon) spawns a reaper task that `.await`s the rp-server
        // sidecar's `Child::wait()` -- a deliberately long-lived,
        // detached process that's never expected to exit on its own.
        // `#[rusty_tokio::main]`'s generated `Runtime::drop` waits
        // *unboundedly* for every blocking-pool job (which is where
        // that `wait()` actually runs) to drain before letting the
        // process exit, so falling off the end of `main` after a
        // successful `--detailed` run hung forever waiting for a
        // sidecar this process deliberately leaves running. Every
        // other command already prints via `println!`'s line-buffered
        // writer, which flushes on each trailing newline regardless of
        // whether stdout is a TTY, so skipping the graceful shutdown
        // path here doesn't drop or truncate any already-printed
        // output.
        Ok(()) => std::process::exit(0),
        Err(err) => {
            eprintln!("harness: {err}");
            std::process::exit(exit_code(&err));
        }
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
                windows_sys::Win32::Foundation::SetHandleInformation(
                    handle,
                    HANDLE_FLAG_INHERIT,
                    0,
                );
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
    let (output_mode, command) = cli::parse(args)?;
    let state_root = paths::state_dir()?;
    let exe_path =
        std::env::current_exe().map_err(|e| HarnessError::io(error::Context::Cli, None, e))?;

    match command {
        cli::Command::DaemonStart => {
            client::daemon_start(&state_root, &exe_path, output_mode).await
        }
        cli::Command::DaemonStatus => client::daemon_status(&state_root, output_mode).await,
        cli::Command::DaemonShutdown => client::daemon_shutdown(&state_root, output_mode).await,
        cli::Command::SessionNew {
            name,
            model,
            goal,
            thinking,
            tools,
            runtime,
        } => {
            client::session_new(
                &state_root,
                session::NewSessionMeta {
                    name,
                    model,
                    goal,
                    parent_id: None,
                    thinking,
                    tools,
                    runtime,
                },
                output_mode,
            )
            .await
        }
        cli::Command::SessionAttach { session_id } => {
            client::session_attach(&state_root, session_id, output_mode).await
        }
        cli::Command::SessionList => client::session_list(&state_root, output_mode).await,
        cli::Command::SessionPrompt { session_id, text } => {
            client::session_prompt(&state_root, session_id, text, output_mode).await
        }
        cli::Command::SessionStop { session_id } => {
            client::session_stop(&state_root, session_id, output_mode).await
        }
        cli::Command::SessionRename { session_id, name } => {
            client::session_rename(&state_root, session_id, name, output_mode).await
        }
        cli::Command::ScheduleAdd {
            session_id,
            text,
            kind,
        } => client::schedule_add(&state_root, session_id, text, kind, output_mode).await,
        cli::Command::ScheduleList { session_id } => {
            client::schedule_list(&state_root, session_id, output_mode).await
        }
        cli::Command::ScheduleCancel {
            session_id,
            schedule_id,
        } => client::schedule_cancel(&state_root, session_id, schedule_id, output_mode).await,
        cli::Command::GoalUpdate { session_id, action } => {
            client::goal_update(&state_root, session_id, action, output_mode).await
        }
        cli::Command::GoalShow { session_id } => {
            client::goal_show(&state_root, session_id, output_mode).await
        }
        cli::Command::SessionAutonomous {
            session_id,
            max_turns,
            max_time_ms,
            quality_gate,
        } => {
            client::session_autonomous(
                &state_root,
                session_id,
                max_turns,
                max_time_ms,
                quality_gate,
                output_mode,
            )
            .await
        }
        cli::Command::PromptTemplateList => {
            client::prompt_template_list(&state_root, output_mode).await
        }
        cli::Command::PromptTemplateRender { name, args } => {
            client::prompt_template_render(&state_root, name, args, output_mode).await
        }
        cli::Command::SessionPromptTemplate {
            session_id,
            name,
            args,
        } => {
            client::session_prompt_template(&state_root, session_id, name, args, output_mode).await
        }
        cli::Command::HarnessUpdate { session_id, action } => {
            client::harness_update(&state_root, session_id, action, output_mode).await
        }
        cli::Command::HarnessShow { session_id } => {
            client::harness_show(&state_root, session_id, output_mode).await
        }
        cli::Command::SessionRefine { session_id } => {
            client::session_refine(&state_root, session_id, output_mode).await
        }
        cli::Command::SessionSpawn {
            parent_id,
            task,
            model,
            name,
        } => client::session_spawn(&state_root, parent_id, task, model, name, output_mode).await,
        cli::Command::SessionChildren { parent_id } => {
            client::session_children(&state_root, parent_id, output_mode).await
        }
        cli::Command::SessionMessage {
            from_id,
            to_id,
            text,
        } => client::session_message(&state_root, from_id, to_id, text, output_mode).await,
        cli::Command::SessionRepl { session_id } => {
            client::session_repl(&state_root, session_id, output_mode).await
        }
        cli::Command::ModelList { detailed } => {
            client::model_list(&state_root, detailed, output_mode).await
        }
        cli::Command::Print { text, model } => {
            client::print_once(&state_root, &exe_path, text, model, output_mode).await
        }
        cli::Command::SupervisorMain => daemon::run(state_root, exe_path).await,
        cli::Command::WorkerMain {
            session_id,
            state_root,
            mode,
            name,
            model,
            goal,
            parent_id,
            thinking,
            tools,
            runtime,
        } => {
            worker::run(worker::WorkerArgs {
                session_id,
                state_root,
                mode,
                name,
                model,
                goal,
                parent_id,
                thinking,
                tools,
                runtime,
            })
            .await
        }
    }
}
