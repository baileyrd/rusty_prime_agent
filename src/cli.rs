//! Argument parsing and dispatch for the public CLI surface (Required
//! Behavior: `daemon start/status/shutdown`, `session new/attach/list`)
//! plus the two hidden entrypoints this binary spawns itself as
//! (`__supervisor-main`, `__worker-main`).
//!
//! Hand-rolled, not `clap`: the surface is eight fixed subcommands with
//! at most two positional/flag arguments each -- a dependency buys
//! nothing here that fifty lines of matching doesn't already give
//! directly, and this project's dependency floor (`platform`,
//! `rusty_tokio`, `serde`/`serde_json`, `thiserror`) is deliberately
//! narrow.

use std::path::PathBuf;

use crate::error::{HarnessError, Result};
use crate::worker::WorkerMode;

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

pub fn parse(args: &[String]) -> Result<Command> {
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
            other => Err(usage(format!("expected `session new|attach|list|prompt`, got {other:?}"))),
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
