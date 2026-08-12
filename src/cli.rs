//! Argument parsing and dispatch for the public CLI surface (Required
//! Behavior: `daemon start/status/shutdown`, `session new/attach/list`,
//! plus `session stop`/`session rename` -- parity with `prime-agent stop
//! <agent>`/`rename <agent> <name>`, see `PARITY.md`) plus the two hidden
//! entrypoints this binary spawns itself as (`__supervisor-main`,
//! `__worker-main`).
//!
//! Hand-rolled, not `clap`: the surface is ten fixed subcommands with
//! at most two positional/flag arguments each -- a dependency buys
//! nothing here that fifty lines of matching doesn't already give
//! directly, and this project's dependency floor (`platform`,
//! `rusty_tokio`, `serde`/`serde_json`, `thiserror`) is deliberately
//! narrow.

use std::path::PathBuf;

use crate::error::{HarnessError, Result};
use crate::worker::WorkerMode;

/// Parity with `prime-agent --mode json`: a leading, global `--mode
/// json|text` flag (before the subcommand) switches every public
/// subcommand's rendering from this project's own human-readable text to
/// raw `Response`/`SessionEvent` JSON lines -- see `client.rs`'s own doc
/// comment for why this reuses the wire types directly rather than
/// modeling `prime-agent`'s much richer `AgentSessionEvent` vocabulary
/// (`packages/coding-agent/docs/json.md` in that repo), which assumes a
/// streaming model/tool-execution pipeline this project doesn't have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputMode {
    #[default]
    Text,
    Json,
}

impl OutputMode {
    fn parse(s: &str) -> Result<Self> {
        match s {
            "text" => Ok(OutputMode::Text),
            "json" => Ok(OutputMode::Json),
            other => Err(usage(format!(
                "unknown --mode value `{other}`, expected `text` or `json`"
            ))),
        }
    }
}

pub enum Command {
    DaemonStart,
    DaemonStatus,
    DaemonShutdown,
    SessionNew {
        name: Option<String>,
    },
    SessionAttach {
        session_id: String,
    },
    SessionList,
    SessionPrompt {
        session_id: String,
        text: String,
    },
    SessionStop {
        session_id: String,
    },
    SessionRename {
        session_id: String,
        name: Option<String>,
    },
    /// `harness __supervisor-main` -- spawned by `daemon start`, never
    /// invoked directly by a user.
    SupervisorMain,
    /// `harness __worker-main --session-id ID --state-root PATH --mode
    /// new|resume|recover [--name NAME]` -- spawned by the supervisor,
    /// never invoked directly by a user.
    WorkerMain {
        session_id: String,
        state_root: PathBuf,
        mode: WorkerMode,
        name: Option<String>,
    },
}

fn usage(message: impl Into<String>) -> HarnessError {
    HarnessError::Usage {
        message: message.into(),
    }
}

/// Splits off a leading `--mode json|text`, if present, then parses the
/// remaining args as an ordinary subcommand. `--mode` is recognized only
/// in this leading position -- `__worker-main`'s own `--mode
/// new|resume|recover` flag (a different flag, spelled the same,
/// consumed by [`parse_worker_main`] instead) always appears after that
/// hidden subcommand's own name, so the two can never collide.
pub fn parse(args: &[String]) -> Result<(OutputMode, Command)> {
    if args.first().map(String::as_str) == Some("--mode") {
        let value = args
            .get(1)
            .ok_or_else(|| usage("--mode requires a value"))?;
        let output_mode = OutputMode::parse(value)?;
        return Ok((output_mode, parse_command(&args[2..])?));
    }
    Ok((OutputMode::default(), parse_command(args)?))
}

fn parse_command(args: &[String]) -> Result<Command> {
    let mut it = args.iter();
    let first = it.next().map(String::as_str);
    match first {
        Some("daemon") => match it.next().map(String::as_str) {
            Some("start") => Ok(Command::DaemonStart),
            Some("status") => Ok(Command::DaemonStatus),
            Some("shutdown") => Ok(Command::DaemonShutdown),
            other => Err(usage(format!("expected `daemon start|status|shutdown`, got {other:?}"))),
        },
        Some("session") => match it.next().map(String::as_str) {
            Some("new") => {
                let name = parse_named_flag(&mut it, "--name")?;
                Ok(Command::SessionNew { name })
            }
            Some("attach") => {
                let session_id = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session attach` requires a session id"))?;
                Ok(Command::SessionAttach { session_id })
            }
            Some("list") => Ok(Command::SessionList),
            Some("prompt") => {
                let session_id = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session prompt` requires a session id"))?;
                let text: Vec<String> = it.cloned().collect();
                if text.is_empty() {
                    return Err(usage("`session prompt` requires prompt text"));
                }
                Ok(Command::SessionPrompt {
                    session_id,
                    text: text.join(" "),
                })
            }
            Some("stop") => {
                let session_id = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session stop` requires a session id"))?;
                Ok(Command::SessionStop { session_id })
            }
            Some("rename") => {
                let session_id = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session rename` requires a session id"))?;
                let name = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session rename` requires a new name"))?;
                Ok(Command::SessionRename {
                    session_id,
                    name: Some(name),
                })
            }
            other => Err(usage(format!(
                "expected `session new|attach|list|prompt|stop|rename`, got {other:?}"
            ))),
        },
        Some("__supervisor-main") => Ok(Command::SupervisorMain),
        Some("__worker-main") => parse_worker_main(&mut it),
        other => Err(usage(format!(
            "expected `daemon <start|status|shutdown>` or `session <new|attach|list|prompt>`, got {other:?}"
        ))),
    }
}

fn parse_named_flag<'a>(
    it: &mut impl Iterator<Item = &'a String>,
    flag: &str,
) -> Result<Option<String>> {
    // Only ever called with the whole remaining arg list, so a
    // not-found flag legitimately means "not given" -- collect into a
    // vec once to allow a simple scan without consuming positional args
    // this same parse might still need.
    let rest: Vec<&'a String> = it.collect();
    for (i, arg) in rest.iter().enumerate() {
        if arg.as_str() == flag {
            return rest
                .get(i + 1)
                .map(|s| Some((*s).clone()))
                .ok_or_else(|| usage(format!("{flag} requires a value")));
        }
    }
    Ok(None)
}

fn parse_worker_main<'a>(it: &mut impl Iterator<Item = &'a String>) -> Result<Command> {
    let mut session_id = None;
    let mut state_root = None;
    let mut mode = None;
    let mut name = None;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--session-id" => {
                session_id = Some(
                    it.next()
                        .ok_or_else(|| usage("--session-id requires a value"))?
                        .clone(),
                )
            }
            "--state-root" => {
                state_root = Some(PathBuf::from(
                    it.next()
                        .ok_or_else(|| usage("--state-root requires a value"))?,
                ))
            }
            "--mode" => {
                mode = Some(WorkerMode::parse(
                    it.next().ok_or_else(|| usage("--mode requires a value"))?,
                )?)
            }
            "--name" => {
                name = Some(
                    it.next()
                        .ok_or_else(|| usage("--name requires a value"))?
                        .clone(),
                )
            }
            other => return Err(usage(format!("unknown __worker-main flag {other}"))),
        }
    }
    Ok(Command::WorkerMain {
        session_id: session_id.ok_or_else(|| usage("__worker-main requires --session-id"))?,
        state_root: state_root.ok_or_else(|| usage("__worker-main requires --state-root"))?,
        mode: mode.ok_or_else(|| usage("__worker-main requires --mode"))?,
        name,
    })
}
