//! Argument parsing and dispatch for the public CLI surface (Required
//! Behavior: `daemon start/status/shutdown`, `session new/attach/list`,
//! plus `session stop`/`session rename`/`session schedule`/`session
//! goal`/`-p`/`--print` -- parity with `prime-agent stop <agent>`/`rename
//! <agent> <name>`/`--goal`/`/goal`/`-p`, see `PARITY.md`) plus the two
//! hidden entrypoints this binary spawns itself as (`__supervisor-main`,
//! `__worker-main`).
//!
//! Hand-rolled, not `clap`: the surface is a fixed, small set of
//! subcommands with at most a few positional/flag arguments each -- a
//! dependency buys nothing here that a few hundred lines of matching
//! doesn't already give directly, and this project's dependency floor
//! (`platform`, `rusty_tokio`, `serde`/`serde_json`, `thiserror`) is
//! deliberately narrow.

use std::path::PathBuf;

use crate::error::{HarnessError, Result};
use crate::protocol::{GoalAction, ScheduleKind};
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
        /// Parity with `prime-agent --model provider/id`; see
        /// `Request::SessionNew::model`'s own doc comment.
        model: Option<String>,
        /// Parity with `prime-agent --goal`; see
        /// `Request::SessionNew::goal`'s own doc comment.
        goal: Option<String>,
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
    /// `harness session schedule add <id> (--at TIME|--every DURATION)
    /// <text...>` -- parity with `prime-agent schedule add`.
    ScheduleAdd {
        session_id: String,
        text: String,
        kind: ScheduleKind,
    },
    /// `harness session schedule list <id>` -- parity with `prime-agent
    /// schedule list`.
    ScheduleList {
        session_id: String,
    },
    /// `harness session schedule cancel <id> <schedule-id>` -- parity
    /// with `prime-agent schedule cancel`.
    ScheduleCancel {
        session_id: String,
        schedule_id: String,
    },
    /// `harness session goal (set <text...>|show|pause|resume|complete|
    /// clear) <id>` -- parity with `prime-agent --goal`/`/goal`.
    GoalUpdate {
        session_id: String,
        action: GoalAction,
    },
    GoalShow {
        session_id: String,
    },
    /// `harness session autonomous <id> --max-turns N [--max-time
    /// DURATION] [--quality-gate CMD]` -- bounded parity with
    /// `prime-agent /autonomous`'s turn/token/time budgets and
    /// user-defined quality gates. No token budget: neither
    /// `EchoProvider` nor `RustyProviderModel`'s `rp-server` round trip
    /// surfaces token counts today, so only turns and wall-clock time are
    /// tracked -- see `PARITY.md`. Requires an existing `Active` goal
    /// (`session goal set`) to drive the continuation prompts.
    SessionAutonomous {
        session_id: String,
        max_turns: u32,
        max_time_ms: Option<u64>,
        quality_gate: Option<String>,
    },
    /// `harness -p [--model PROVIDER/MODEL] <text...>`/`harness --print
    /// ...` -- parity with `prime-agent -p`/`--model`. Unlike every other
    /// subcommand, does not require `daemon start` first: see
    /// `client::print_once`'s doc comment.
    Print {
        text: String,
        model: Option<String>,
    },
    /// `harness __supervisor-main` -- spawned by `daemon start`, never
    /// invoked directly by a user.
    SupervisorMain,
    /// `harness __worker-main --session-id ID --state-root PATH --mode
    /// new|resume|recover [--name NAME] [--model PROVIDER/MODEL]` --
    /// spawned by the supervisor, never invoked directly by a user.
    WorkerMain {
        session_id: String,
        state_root: PathBuf,
        mode: WorkerMode,
        name: Option<String>,
        model: Option<String>,
        goal: Option<String>,
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
    if matches!(
        args.first().map(String::as_str),
        Some("-p") | Some("--print")
    ) {
        let mut rest = &args[1..];
        // `--model` is only recognized as a strict leading flag here
        // (immediately after `-p`/`--print`), not scanned for throughout
        // -- unlike `session new`, everything after this point is free
        // prompt text, and text that happens to contain the substring
        // `--model` must not be misread as the flag.
        let model = if rest.first().map(String::as_str) == Some("--model") {
            let value = rest
                .get(1)
                .cloned()
                .ok_or_else(|| usage("--model requires a value"))?;
            rest = &rest[2..];
            Some(value)
        } else {
            None
        };
        if rest.is_empty() {
            return Err(usage("`-p`/`--print` requires prompt text"));
        }
        return Ok(Command::Print {
            text: rest.join(" "),
            model,
        });
    }
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
                let rest: Vec<&String> = it.collect();
                let name = scan_named_flag(&rest, "--name")?;
                let model = scan_named_flag(&rest, "--model")?;
                let goal = scan_named_flag(&rest, "--goal")?;
                Ok(Command::SessionNew { name, model, goal })
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
            Some("schedule") => parse_schedule(&mut it),
            Some("goal") => parse_goal(&mut it),
            Some("autonomous") => parse_autonomous(&mut it),
            other => Err(usage(format!(
                "expected `session new|attach|list|prompt|stop|rename|schedule|goal|autonomous`, got {other:?}"
            ))),
        },
        Some("__supervisor-main") => Ok(Command::SupervisorMain),
        Some("__worker-main") => parse_worker_main(&mut it),
        other => Err(usage(format!(
            "expected `daemon <start|status|shutdown>`, `session <new|attach|list|prompt|stop|rename|schedule|goal|autonomous>`, or `-p`/`--print <text>`, got {other:?}"
        ))),
    }
}

/// Scans `rest` (the whole remaining arg list, already materialized so
/// this can be called once per flag without exhausting an iterator) for
/// `flag <value>`. A not-found flag legitimately means "not given," not
/// an error -- `session new`'s only caller has nothing else positional
/// to worry about consuming by mistake.
fn scan_named_flag(rest: &[&String], flag: &str) -> Result<Option<String>> {
    for (i, arg) in rest.iter().enumerate() {
        if arg.as_str() == flag {
            return rest
                .get(i + 1)
                .map(|s| Some((*s).to_string()))
                .ok_or_else(|| usage(format!("{flag} requires a value")));
        }
    }
    Ok(None)
}

/// `session schedule add <id> (--at TIME|--every DURATION) <text...>` /
/// `session schedule list <id>` / `session schedule cancel <id>
/// <schedule-id>`.
fn parse_schedule<'a>(it: &mut impl Iterator<Item = &'a String>) -> Result<Command> {
    match it.next().map(String::as_str) {
        Some("add") => {
            let session_id = it
                .next()
                .cloned()
                .ok_or_else(|| usage("`session schedule add` requires a session id"))?;
            let rest: Vec<&String> = it.collect();
            let mut at = None;
            let mut every = None;
            let mut text_parts = Vec::new();
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--at" => {
                        at = Some(
                            rest.get(i + 1)
                                .ok_or_else(|| usage("--at requires a value"))?
                                .to_string(),
                        );
                        i += 2;
                    }
                    "--every" => {
                        every = Some(
                            rest.get(i + 1)
                                .ok_or_else(|| usage("--every requires a value"))?
                                .to_string(),
                        );
                        i += 2;
                    }
                    other => {
                        text_parts.push(other.to_string());
                        i += 1;
                    }
                }
            }
            let kind = match (at, every) {
                (Some(a), None) => ScheduleKind::Once {
                    at_ms: parse_at_ms(&a)?,
                },
                (None, Some(e)) => ScheduleKind::Every {
                    interval_ms: parse_duration_ms(&e)?,
                },
                (Some(_), Some(_)) => {
                    return Err(usage("`--at` and `--every` are mutually exclusive"))
                }
                (None, None) => {
                    return Err(usage("`session schedule add` requires `--at` or `--every`"))
                }
            };
            if text_parts.is_empty() {
                return Err(usage("`session schedule add` requires prompt text"));
            }
            Ok(Command::ScheduleAdd {
                session_id,
                text: text_parts.join(" "),
                kind,
            })
        }
        Some("list") => {
            let session_id = it
                .next()
                .cloned()
                .ok_or_else(|| usage("`session schedule list` requires a session id"))?;
            Ok(Command::ScheduleList { session_id })
        }
        Some("cancel") => {
            let session_id = it
                .next()
                .cloned()
                .ok_or_else(|| usage("`session schedule cancel` requires a session id"))?;
            let schedule_id = it
                .next()
                .cloned()
                .ok_or_else(|| usage("`session schedule cancel` requires a schedule id"))?;
            Ok(Command::ScheduleCancel {
                session_id,
                schedule_id,
            })
        }
        other => Err(usage(format!(
            "expected `session schedule add|list|cancel`, got {other:?}"
        ))),
    }
}

/// `session goal (set <text...>|show|pause|resume|complete|clear) <id>`.
fn parse_goal<'a>(it: &mut impl Iterator<Item = &'a String>) -> Result<Command> {
    match it.next().map(String::as_str) {
        Some("set") => {
            let session_id = it
                .next()
                .cloned()
                .ok_or_else(|| usage("`session goal set` requires a session id"))?;
            let text: Vec<String> = it.cloned().collect();
            if text.is_empty() {
                return Err(usage("`session goal set` requires goal text"));
            }
            Ok(Command::GoalUpdate {
                session_id,
                action: GoalAction::Set {
                    text: text.join(" "),
                },
            })
        }
        Some("show") => {
            let session_id = it
                .next()
                .cloned()
                .ok_or_else(|| usage("`session goal show` requires a session id"))?;
            Ok(Command::GoalShow { session_id })
        }
        Some(verb @ ("pause" | "resume" | "complete" | "clear")) => {
            let session_id = it
                .next()
                .cloned()
                .ok_or_else(|| usage(format!("`session goal {verb}` requires a session id")))?;
            let action = match verb {
                "pause" => GoalAction::Pause,
                "resume" => GoalAction::Resume,
                "complete" => GoalAction::Complete,
                "clear" => GoalAction::Clear,
                _ => unreachable!(),
            };
            Ok(Command::GoalUpdate { session_id, action })
        }
        other => Err(usage(format!(
            "expected `session goal set|show|pause|resume|complete|clear`, got {other:?}"
        ))),
    }
}

/// `session autonomous <id> --max-turns N [--max-time DURATION]
/// [--quality-gate CMD]`.
fn parse_autonomous<'a>(it: &mut impl Iterator<Item = &'a String>) -> Result<Command> {
    let session_id = it
        .next()
        .cloned()
        .ok_or_else(|| usage("`session autonomous` requires a session id"))?;
    let rest: Vec<&String> = it.collect();
    let max_turns: u32 = scan_named_flag(&rest, "--max-turns")?
        .ok_or_else(|| usage("`session autonomous` requires `--max-turns`"))?
        .parse()
        .map_err(|_| usage("--max-turns requires a positive integer"))?;
    let max_time_ms = scan_named_flag(&rest, "--max-time")?
        .map(|s| parse_duration_ms(&s))
        .transpose()?;
    let quality_gate = scan_named_flag(&rest, "--quality-gate")?;
    Ok(Command::SessionAutonomous {
        session_id,
        max_turns,
        max_time_ms,
        quality_gate,
    })
}

/// A duration string like `30s`/`5m`/`2h`/`1d` (a number plus one of
/// `s`/`m`/`h`/`d`) -- deliberately not a full ISO 8601 duration parser;
/// this project doesn't pull in a dependency for `--every`'s narrow
/// needs (parity with `prime-agent`'s own shorthand-friendly CLI
/// conventions, not with ISO 8601 itself).
fn parse_duration_ms(s: &str) -> Result<u64> {
    let (digits, unit) = s.split_at(s.len().saturating_sub(1));
    let n: u64 = digits.parse().map_err(|_| {
        usage(format!(
            "invalid duration {s:?}, expected e.g. `30s`/`5m`/`2h`/`1d`"
        ))
    })?;
    let multiplier = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => {
            return Err(usage(format!(
                "invalid duration {s:?}, expected e.g. `30s`/`5m`/`2h`/`1d`"
            )))
        }
    };
    Ok(n * multiplier)
}

/// `--at`'s value: either a raw Unix-epoch-milliseconds integer (an
/// absolute time), or a duration string per [`parse_duration_ms`]
/// (interpreted as "that far from now"). Not a full RFC 3339 timestamp
/// parser for the same "no dependency for this" reasoning as
/// `parse_duration_ms`.
fn parse_at_ms(s: &str) -> Result<u64> {
    if let Ok(ms) = s.parse::<u64>() {
        return Ok(ms);
    }
    let offset = parse_duration_ms(s)?;
    Ok(crate::paths::now_ms() + offset)
}

fn parse_worker_main<'a>(it: &mut impl Iterator<Item = &'a String>) -> Result<Command> {
    let mut session_id = None;
    let mut state_root = None;
    let mut mode = None;
    let mut name = None;
    let mut model = None;
    let mut goal = None;
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
            "--model" => {
                model = Some(
                    it.next()
                        .ok_or_else(|| usage("--model requires a value"))?
                        .clone(),
                )
            }
            "--goal" => {
                goal = Some(
                    it.next()
                        .ok_or_else(|| usage("--goal requires a value"))?
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
        model,
        goal,
    })
}
