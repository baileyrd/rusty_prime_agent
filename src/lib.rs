//! `rusty_prime_agent` -- the library crate backing the `harness` CLI
//! binary (`src/main.rs`), and a genuine embeddable SDK surface in its
//! own right, parity with `prime-agent`'s own `createAgentSession()`/
//! `defineTool()` programmatic API (bounded differently -- see below).
//!
//! Two honest embedding layers, matching this project's actual
//! architecture rather than assuming an in-process agent loop the way
//! `prime-agent`'s own SDK does:
//!
//! - **In-process, no daemon at all.** [`session::AgentSession::create`]
//!   is exactly what `session.rs`'s own unit tests already construct
//!   directly (`Box::new(EchoProvider)`/`Box::new(NoopToolRuntime)`),
//!   now `pub` for any external crate to do the same: a real, driveable
//!   session with no daemon/worker/socket machinery in the loop at all.
//!   Not a pure in-memory session, though -- `create` still does real
//!   filesystem I/O ([`paths::ensure_dir`], `state.json`/transcript
//!   persistence) under a caller-supplied `state_root`, the same
//!   durability this project's own daemon-backed sessions get.
//!   Implementing [`provider::ModelProvider`]/[`tool_runtime::
//!   ToolRuntime`] yourself (both plain `pub trait`s, already
//!   object-safe and `Send + Sync`, already how `AgentSession` stores
//!   them internally) is this project's answer to `defineTool()`: no
//!   separate registration API needed, a custom `Box<dyn ToolRuntime>`
//!   passed to `create` is already the whole mechanism.
//! - **Drive a *running* daemon.** [`dispatch_one_shot`] sends one
//!   [`protocol::Request`] over an already-running daemon's socket and
//!   returns a typed [`protocol::Response`] -- the same connect-send-
//!   receive primitive `client.rs`'s own CLI-output functions build on
//!   internally, but returning data instead of printing to this
//!   process's stdout the way every `client::session_*` function does.
//!   Everything else in `client.rs` stays crate-internal on purpose:
//!   those functions are CLI rendering routines, not a library API.
//!
//! Explicitly out of scope for this first increment: any semver/
//! stability guarantee on this public surface (`Cargo.toml` still says
//! `publish = false`, still `0.1.0`), docs.rs-quality rustdoc coverage
//! beyond what's here, and exposing `daemon`/`worker`/`ipython_runtime`/
//! `zmtp` publicly -- nothing in either embedding layer above needs
//! them directly, and they stay implementation detail. See `PARITY.md`'s
//! "Embeddable SDK" entry for the full design story.

pub mod error;
pub mod paths;
pub mod protocol;
pub mod provider;
pub mod session;
pub mod tool_runtime;

mod auth;
mod catalog;
mod cli;
mod client;
mod daemon;
mod doctor;
mod extensions;
mod frontmatter;
mod http_client;
mod ipython_runtime;
mod mcp_client;
mod procutil;
mod prompt_template;
mod providers;
mod rp_server;
mod schedule;
mod self_update;
mod settings;
mod sha256;
mod skills;
mod telemetry;
mod termctl;
mod theme;
mod tools;
mod transport;
mod worker;
mod zmtp;

pub use client::dispatch_one_shot;

use error::{HarnessError, Result};

/// Parses `args` (a CLI invocation's own argv, sans `argv[0]`) and
/// dispatches to the matching subcommand. The entire body of what used
/// to be `main.rs`'s own private `run` function, moved here so
/// `src/main.rs` can shrink to the handful of process-level concerns
/// (stdio hardening, the explicit `std::process::exit`) that are
/// genuinely bin-only and would be actively wrong to impose on a host
/// program embedding this crate as a library instead -- see
/// `main::harden_inherited_stdio`'s own doc comment for why stdio
/// hardening in particular can't move here.
pub async fn run(args: &[String]) -> Result<()> {
    let (output_mode, command) = cli::parse(args)?;
    let state_root = paths::state_dir()?;
    let exe_path =
        std::env::current_exe().map_err(|e| HarnessError::io(error::Context::Cli, None, e))?;

    match command {
        cli::Command::DaemonStart => {
            client::daemon_start(&state_root, &exe_path, output_mode).await
        }
        cli::Command::DaemonStatus => client::daemon_status(&state_root, output_mode).await,
        cli::Command::DaemonShutdown { force } => {
            client::daemon_shutdown(&state_root, force, output_mode).await
        }
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
                    // Resolved server-side by `daemon::handle_session_new`
                    // -- see that field's own doc comment on `protocol::
                    // SessionState`.
                    rlm_depth: None,
                    rlm_max_depth: None,
                    // `session new` (this command) is a plain top-level
                    // session, never `rlm(...)`-admitted.
                    spawned_from_sequence: None,
                },
                output_mode,
            )
            .await
        }
        cli::Command::SessionAttach { session_id } => {
            client::session_attach(&state_root, session_id, output_mode).await
        }
        cli::Command::SessionList => client::session_list(&state_root, output_mode).await,
        cli::Command::SessionPrompt {
            session_id,
            text,
            image_paths,
        } => client::session_prompt(&state_root, session_id, text, image_paths, output_mode).await,
        cli::Command::SessionStop { session_id } => {
            client::session_stop(&state_root, session_id, output_mode).await
        }
        cli::Command::SessionRename { session_id, name } => {
            client::session_rename(&state_root, session_id, name, output_mode).await
        }
        cli::Command::SessionCompact {
            session_id,
            instructions,
        } => client::session_compact(&state_root, session_id, instructions, output_mode).await,
        cli::Command::SessionHeartbeat { session_id, every } => {
            client::session_heartbeat(&state_root, session_id, every, output_mode).await
        }
        cli::Command::SessionInterrupt { session_id } => {
            client::session_interrupt(&state_root, session_id, output_mode).await
        }
        cli::Command::SessionFork {
            session_id,
            at_sequence,
            name,
        } => client::session_fork(&state_root, session_id, at_sequence, name, output_mode).await,
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
        cli::Command::SkillList => client::skill_list(&state_root, output_mode).await,
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
        cli::Command::SessionRpc { session_id } => {
            client::session_rpc(&state_root, session_id).await
        }
        cli::Command::SessionTree { session_id } => {
            client::session_tree(&state_root, session_id, output_mode).await
        }
        cli::Command::SessionSetActiveLeaf {
            session_id,
            sequence,
        } => client::session_set_active_leaf(&state_root, session_id, sequence, output_mode).await,
        cli::Command::SessionBranchSummarize {
            session_id,
            branch_leaf_sequence,
        } => {
            client::session_branch_summarize(
                &state_root,
                session_id,
                branch_leaf_sequence,
                output_mode,
            )
            .await
        }
        cli::Command::ModelList { detailed } => {
            client::model_list(&state_root, detailed, output_mode).await
        }
        cli::Command::Update { force } => client::self_update(force, output_mode).await,
        cli::Command::Doctor { fix } => {
            client::doctor(&state_root, &exe_path, fix, output_mode).await
        }
        cli::Command::Print {
            text,
            model,
            no_session,
        } => {
            let text = merge_piped_stdin(text);
            if no_session {
                client::print_ephemeral(text, model, output_mode).await
            } else {
                client::print_once(&state_root, &exe_path, text, model, output_mode).await
            }
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
            rlm_depth,
            rlm_max_depth,
            spawned_from_sequence,
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
                rlm_depth,
                rlm_max_depth,
                spawned_from_sequence,
            })
            .await
        }
    }
}

/// Parity with a bounded slice of `prime-agent -p`'s piped-stdin
/// handling: when this process's stdin is not a terminal (i.e. it was
/// redirected from a file or a pipe, `harness -p "..." < context.txt` or
/// `cat context.txt | harness -p "..."`), whatever it contains is read
/// in full and appended to the prompt text, separated by a blank line.
/// A real terminal (interactive use, the ordinary case) is left alone --
/// this must never block waiting for a human to type and hit Ctrl-D.
/// Reading fails open: an I/O error reading an already-non-terminal
/// stdin (rare -- a closed fd, a already-exhausted pipe) is treated as
/// "nothing piped" rather than failing the whole command, since the
/// prompt text the caller already gave on the command line is still a
/// perfectly valid prompt on its own.
fn merge_piped_stdin(text: String) -> String {
    use std::io::{IsTerminal, Read};
    if std::io::stdin().is_terminal() {
        return text;
    }
    let mut piped = String::new();
    if std::io::stdin().read_to_string(&mut piped).is_err() {
        return text;
    }
    let piped = piped.trim_end_matches('\n');
    if piped.is_empty() {
        return text;
    }
    format!("{text}\n\n{piped}")
}
