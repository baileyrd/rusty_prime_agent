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
use crate::protocol::{GoalAction, HarnessAction, HarnessNoteKind, ScheduleKind};
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
    DaemonShutdown {
        force: bool,
    },
    SessionNew {
        name: Option<String>,
        /// Parity with `prime-agent --model provider/id`; see
        /// `Request::SessionNew::model`'s own doc comment.
        model: Option<String>,
        /// Parity with `prime-agent --goal`; see
        /// `Request::SessionNew::goal`'s own doc comment.
        goal: Option<String>,
        /// `--thinking low|medium|high`; see
        /// `Request::SessionNew::thinking`'s own doc comment.
        thinking: Option<String>,
        /// `--tools read`; see `Request::SessionNew::tools`'s own doc
        /// comment.
        tools: Option<String>,
        /// `--runtime ipython`; see `Request::SessionNew::runtime`'s own
        /// doc comment.
        runtime: Option<String>,
    },
    SessionAttach {
        session_id: String,
    },
    SessionList,
    SessionPrompt {
        session_id: String,
        text: String,
        /// `--image <path>` (repeatable) -- parity with a bounded slice
        /// of `prime-agent`'s image-paste feature. See
        /// `protocol::TranscriptEntry::images`'s own doc comment for the
        /// shape these get loaded into.
        image_paths: Vec<String>,
        /// `--request-id <id>` -- opts this one prompt into idempotent
        /// replay protection. See `protocol::Request::SessionPrompt::
        /// request_id`'s own doc comment.
        request_id: Option<String>,
    },
    SessionStop {
        session_id: String,
    },
    SessionRename {
        session_id: String,
        name: Option<String>,
    },
    /// `harness session compact <id> [instructions...]` -- parity with
    /// `prime-agent /compact [instructions]`. See
    /// `protocol::Request::SessionCompact`'s own doc comment.
    SessionCompact {
        session_id: String,
        instructions: Option<String>,
    },
    /// `harness session fork <id> [--at N] [--name NAME]` -- bounded
    /// parity with a slice of `prime-agent`'s `/fork`. See
    /// `protocol::Request::SessionFork`'s own doc comment.
    SessionFork {
        session_id: String,
        at_sequence: Option<u64>,
        name: Option<String>,
    },
    /// `harness session rpc <id>` -- parity with `prime-agent --mode
    /// rpc`. See `client::session_rpc`'s own doc comment.
    SessionRpc {
        session_id: String,
    },
    /// `harness session tree <id>` -- parity with `prime-agent`'s
    /// `/tree` visualization half. See `client::session_tree`'s own doc
    /// comment.
    SessionTree {
        session_id: String,
    },
    /// `harness session set-active-leaf <id> <sequence>` -- parity with
    /// `prime-agent`'s `/tree` navigation half. See
    /// `protocol::Request::SessionSetActiveLeaf`'s own doc comment.
    SessionSetActiveLeaf {
        session_id: String,
        sequence: u64,
    },
    /// `harness session branch-summary <id> <branch-leaf-sequence>` --
    /// parity with `session-format.md`'s `BranchSummaryEntry`. See
    /// `protocol::Request::SessionBranchSummarize`'s own doc comment.
    SessionBranchSummarize {
        session_id: String,
        branch_leaf_sequence: u64,
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
    /// `harness prompt-template list` -- parity with `prime-agent`'s `/`
    /// autocomplete listing every discovered template with its
    /// description. No daemon needed: this is a local directory scan
    /// (`prompt_template::discover`), not a session-scoped operation.
    PromptTemplateList,
    /// `harness prompt-template render <name> [args...]` -- prints the
    /// expanded prompt text to stdout without sending it anywhere. No
    /// daemon needed, same reasoning as `PromptTemplateList`.
    PromptTemplateRender {
        name: String,
        args: Vec<String>,
    },
    /// `harness skill list` -- lists every skill `skills::discover` finds
    /// under `<state-dir>/skills/`, with its description. No daemon
    /// needed, same reasoning as `PromptTemplateList`.
    SkillList,
    /// `harness session prompt-template <id> <name> [args...]` -- parity
    /// with typing `/name args...` in `prime-agent`'s live editor:
    /// expands the named template (`prompt_template::discover`) against
    /// `args` and sends the result as an ordinary `SessionPrompt`, same
    /// as `session prompt` would with that expanded text.
    SessionPromptTemplate {
        session_id: String,
        name: String,
        args: Vec<String>,
    },
    /// `harness session harness (add <id> <prompt|memory|skill>
    /// <text...>|list <id>|rollback <id> <index>)` -- parity with
    /// `prime-agent`'s Continual Harness durable state
    /// (`HarnessState`); `session refine <id>` (below) is the other,
    /// model-driven way to reach `Add`.
    HarnessUpdate {
        session_id: String,
        action: HarnessAction,
    },
    HarnessShow {
        session_id: String,
    },
    /// `harness session refine <id>` -- parity with `prime-agent`'s
    /// `/refine`: reviews the session's current trajectory and applies
    /// one small, evidence-backed update to its harness state. See
    /// `client::session_refine`'s own doc comment for exactly how.
    SessionRefine {
        session_id: String,
    },
    /// `harness session spawn <parent-id> [--model PROVIDER/MODEL]
    /// [--name NAME] <task text...>` -- bounded, non-Python parity with
    /// `prime-agent`'s recursive subagents (`rlm(...)`). See
    /// `client::session_spawn`'s own doc comment for exactly how.
    SessionSpawn {
        parent_id: String,
        task: String,
        model: Option<String>,
        name: Option<String>,
    },
    /// `harness session children <id>` -- direct children only.
    SessionChildren {
        parent_id: String,
    },
    /// `harness session message <from-id> <to-id> <text...>` -- parity
    /// with `agent_message.send(msg, receiver_role="parent"|"child")`;
    /// `to-id` must be `from-id`'s own parent or one of its own
    /// children.
    SessionMessage {
        from_id: String,
        to_id: String,
        text: String,
    },
    /// `harness session repl <id>` -- minimal, non-Python parity with
    /// `prime-agent`'s interactive TUI. See `client::session_repl`'s own
    /// doc comment for exactly what it does and doesn't cover.
    SessionRepl {
        session_id: String,
    },
    /// `harness model list [--detailed]` -- bounded parity with
    /// `prime-agent model list`'s catalog browse. See `client::
    /// model_list`'s own doc comment for exactly what it does and
    /// doesn't cover. Plain `model list` needs no daemon: a pure
    /// environment-variable check. `--detailed` additionally starts (or
    /// reuses) an `rp-server` sidecar and queries its real per-model
    /// catalog.
    ModelList {
        detailed: bool,
    },
    /// `harness update [--force]` -- bounded, honest parity with
    /// `prime-agent update [--force]`. See `self_update`'s own module
    /// doc comment for exactly what it does and doesn't cover: this
    /// project has no release channel to check against (`publish =
    /// false`, no GitHub Releases), so this pulls and rebuilds the git
    /// checkout this binary was itself built from instead. No daemon
    /// needed, same reasoning as `ModelList`.
    Update {
        force: bool,
    },
    /// `harness doctor [--fix]` -- bounded, honest parity with
    /// `prime-agent doctor [--fix]`. See `doctor`'s own module doc
    /// comment for exactly what's checked and what `--fix` does and
    /// doesn't do. Does not require a daemon first -- reachability is
    /// one of the things it checks.
    Doctor {
        fix: bool,
    },
    /// `harness session heartbeat <id> [--every DURATION]` -- a
    /// top-level CLI entry point into the same re-entry mechanism
    /// `session_repl`'s own `/heartbeat`/`/heartbeat every <duration>`
    /// already cover, for a caller who wants it without an interactive
    /// REPL session (parity with `session compact`/`/compact` already
    /// existing as both). See `client::session_heartbeat`'s own doc
    /// comment for exactly how.
    SessionHeartbeat {
        session_id: String,
        /// Already parsed to milliseconds at CLI-parse time (a bad
        /// `--every` value is a usage error, exactly like `session
        /// schedule add --every`'s own `parse_duration_ms(&e)?` --
        /// unlike `session_repl`'s own tolerant `/heartbeat every
        /// <duration>` line, which prints and keeps the REPL running on
        /// a bad value instead of exiting).
        every: Option<u64>,
    },
    /// `harness session interrupt <id>` -- bounded parity with a slice
    /// of `prime-agent`'s "steering": requests that an in-flight
    /// `session prompt`/`/heartbeat`-triggered multi-round tool-calling
    /// loop stop *before its next round* rather than continuing to a
    /// natural finish or the `MAX_TOOL_ROUNDS` cap. See
    /// `protocol::Request::SessionInterrupt`'s own doc comment for
    /// exactly what this can and can't cancel.
    SessionInterrupt {
        session_id: String,
    },
    /// `harness -p [--model PROVIDER/MODEL] [--no-session] <text...>`/
    /// `harness --print ...` -- parity with `prime-agent -p`/`--model`/
    /// `--no-session`. Unlike every other subcommand, does not require
    /// `daemon start` first: see `client::print_once`'s doc comment.
    /// `no_session: true` skips the daemon entirely -- see `client::
    /// print_ephemeral`'s own doc comment for what that does and doesn't
    /// mean.
    Print {
        text: String,
        model: Option<String>,
        no_session: bool,
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
        parent_id: Option<String>,
        thinking: Option<String>,
        tools: Option<String>,
        runtime: Option<String>,
        /// Always supplied by the daemon at spawn time (see
        /// `worker::WorkerArgs::rlm_depth`'s own doc comment for why this
        /// can't wait until `AgentSession::create`/`recover` reads it back
        /// out of persisted state instead).
        rlm_depth: Option<u32>,
        rlm_max_depth: Option<u32>,
        /// Only meaningful for `--mode new` (see `worker::WorkerArgs::
        /// spawned_from_sequence`'s own doc comment).
        spawned_from_sequence: Option<u64>,
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
        // `--model`/`--no-session` are only recognized as strict leading
        // flags here (immediately after `-p`/`--print`, in either
        // order), not scanned for throughout -- unlike `session new`,
        // everything after the last recognized leading flag is free
        // prompt text, and text that happens to contain either substring
        // must not be misread as a flag.
        let mut model = None;
        let mut no_session = false;
        loop {
            match rest.first().map(String::as_str) {
                Some("--model") => {
                    let value = rest
                        .get(1)
                        .cloned()
                        .ok_or_else(|| usage("--model requires a value"))?;
                    rest = &rest[2..];
                    model = Some(value);
                }
                Some("--no-session") => {
                    rest = &rest[1..];
                    no_session = true;
                }
                _ => break,
            }
        }
        if rest.is_empty() {
            return Err(usage("`-p`/`--print` requires prompt text"));
        }
        return Ok(Command::Print {
            text: rest.join(" "),
            model,
            no_session,
        });
    }
    let mut it = args.iter();
    let first = it.next().map(String::as_str);
    match first {
        Some("daemon") => match it.next().map(String::as_str) {
            Some("start") => Ok(Command::DaemonStart),
            Some("status") => Ok(Command::DaemonStatus),
            Some("shutdown") => {
                let rest: Vec<&String> = it.collect();
                let force = rest.iter().any(|a| a.as_str() == "--force");
                Ok(Command::DaemonShutdown { force })
            }
            other => Err(usage(format!(
                "expected `daemon start|status|shutdown [--force]`, got {other:?}"
            ))),
        },
        Some("session") => match it.next().map(String::as_str) {
            Some("new") => {
                let rest: Vec<&String> = it.collect();
                let name = scan_named_flag(&rest, "--name")?;
                let model = scan_named_flag(&rest, "--model")?;
                let goal = scan_named_flag(&rest, "--goal")?;
                let thinking = scan_named_flag(&rest, "--thinking")?
                    .map(|v| parse_thinking_level(&v))
                    .transpose()?;
                let tools = scan_named_flag(&rest, "--tools")?
                    .map(|v| parse_tools_value(&v))
                    .transpose()?;
                let runtime = scan_named_flag(&rest, "--runtime")?
                    .map(|v| parse_runtime_value(&v))
                    .transpose()?;
                Ok(Command::SessionNew {
                    name,
                    model,
                    goal,
                    thinking,
                    tools,
                    runtime,
                })
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
                // `--image <path>` is repeatable and can appear anywhere
                // among the free-text words, unlike `scan_named_flag`'s
                // own single-occurrence assumption -- so this is parsed
                // by hand rather than reusing it, the same "only write a
                // bespoke parser when the shared one's shape doesn't fit"
                // reasoning `/fork`'s own REPL argument parser uses.
                let rest: Vec<String> = it.cloned().collect();
                let mut image_paths = Vec::new();
                let mut text_words = Vec::new();
                let mut request_id = None;
                let mut i = 0;
                while i < rest.len() {
                    if rest[i] == "--image" {
                        let path = rest
                            .get(i + 1)
                            .cloned()
                            .ok_or_else(|| usage("--image requires a value"))?;
                        image_paths.push(path);
                        i += 2;
                    } else if rest[i] == "--request-id" {
                        request_id = Some(
                            rest.get(i + 1)
                                .cloned()
                                .ok_or_else(|| usage("--request-id requires a value"))?,
                        );
                        i += 2;
                    } else {
                        text_words.push(rest[i].clone());
                        i += 1;
                    }
                }
                if text_words.is_empty() && image_paths.is_empty() {
                    return Err(usage("`session prompt` requires prompt text or --image"));
                }
                Ok(Command::SessionPrompt {
                    session_id,
                    text: text_words.join(" "),
                    image_paths,
                    request_id,
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
            Some("compact") => {
                let session_id = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session compact` requires a session id"))?;
                let instructions: Vec<String> = it.cloned().collect();
                Ok(Command::SessionCompact {
                    session_id,
                    instructions: if instructions.is_empty() {
                        None
                    } else {
                        Some(instructions.join(" "))
                    },
                })
            }
            Some("heartbeat") => {
                let session_id = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session heartbeat` requires a session id"))?;
                let rest: Vec<&String> = it.collect();
                let every = scan_named_flag(&rest, "--every")?
                    .map(|v| parse_duration_ms(&v))
                    .transpose()?;
                Ok(Command::SessionHeartbeat { session_id, every })
            }
            Some("interrupt") => {
                let session_id = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session interrupt` requires a session id"))?;
                Ok(Command::SessionInterrupt { session_id })
            }
            Some("fork") => {
                let session_id = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session fork` requires a session id"))?;
                let rest: Vec<&String> = it.collect();
                let at_sequence = scan_named_flag(&rest, "--at")?
                    .map(|v| {
                        v.parse::<u64>()
                            .map_err(|_| usage(format!("--at requires an integer, got {v:?}")))
                    })
                    .transpose()?;
                let name = scan_named_flag(&rest, "--name")?;
                Ok(Command::SessionFork {
                    session_id,
                    at_sequence,
                    name,
                })
            }
            Some("tree") => {
                let session_id = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session tree` requires a session id"))?;
                Ok(Command::SessionTree { session_id })
            }
            Some("set-active-leaf") => {
                let session_id = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session set-active-leaf` requires a session id"))?;
                let sequence = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session set-active-leaf` requires a sequence"))?
                    .parse::<u64>()
                    .map_err(|_| usage("`session set-active-leaf` requires an integer sequence"))?;
                Ok(Command::SessionSetActiveLeaf {
                    session_id,
                    sequence,
                })
            }
            Some("branch-summary") => {
                let session_id = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session branch-summary` requires a session id"))?;
                let branch_leaf_sequence = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session branch-summary` requires a sequence"))?
                    .parse::<u64>()
                    .map_err(|_| usage("`session branch-summary` requires an integer sequence"))?;
                Ok(Command::SessionBranchSummarize {
                    session_id,
                    branch_leaf_sequence,
                })
            }
            Some("schedule") => parse_schedule(&mut it),
            Some("goal") => parse_goal(&mut it),
            Some("autonomous") => parse_autonomous(&mut it),
            Some("prompt-template") => {
                let session_id = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session prompt-template` requires a session id"))?;
                let name = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session prompt-template` requires a template name"))?;
                let args: Vec<String> = it.cloned().collect();
                Ok(Command::SessionPromptTemplate {
                    session_id,
                    name,
                    args,
                })
            }
            Some("harness") => parse_harness(&mut it),
            Some("refine") => {
                let session_id = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session refine` requires a session id"))?;
                Ok(Command::SessionRefine { session_id })
            }
            Some("spawn") => {
                let parent_id = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session spawn` requires a parent session id"))?;
                let rest: Vec<&String> = it.collect();
                let model = scan_named_flag(&rest, "--model")?;
                let name = scan_named_flag(&rest, "--name")?;
                // Everything left after pulling `--model`/`--name` (each
                // flag plus its value) out is the task text, joined back
                // together the same way `session prompt`'s free-text
                // tail is.
                let mut task_parts: Vec<String> = Vec::new();
                let mut i = 0;
                while i < rest.len() {
                    match rest[i].as_str() {
                        "--model" | "--name" => i += 2,
                        other => {
                            task_parts.push(other.to_string());
                            i += 1;
                        }
                    }
                }
                if task_parts.is_empty() {
                    return Err(usage("`session spawn` requires task text"));
                }
                Ok(Command::SessionSpawn {
                    parent_id,
                    task: task_parts.join(" "),
                    model,
                    name,
                })
            }
            Some("children") => {
                let parent_id = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session children` requires a session id"))?;
                Ok(Command::SessionChildren { parent_id })
            }
            Some("message") => {
                let from_id = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session message` requires a sender session id"))?;
                let to_id = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session message` requires a recipient session id"))?;
                let text: Vec<String> = it.cloned().collect();
                if text.is_empty() {
                    return Err(usage("`session message` requires message text"));
                }
                Ok(Command::SessionMessage {
                    from_id,
                    to_id,
                    text: text.join(" "),
                })
            }
            Some("repl") => {
                let session_id = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session repl` requires a session id"))?;
                Ok(Command::SessionRepl { session_id })
            }
            Some("rpc") => {
                let session_id = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`session rpc` requires a session id"))?;
                Ok(Command::SessionRpc { session_id })
            }
            other => Err(usage(format!(
                "expected `session new|attach|list|prompt|stop|rename|compact|heartbeat|interrupt|schedule|goal|autonomous|prompt-template|harness|refine|spawn|children|message|repl|rpc`, got {other:?}"
            ))),
        },
        Some("prompt-template") => match it.next().map(String::as_str) {
            Some("list") => Ok(Command::PromptTemplateList),
            Some("render") => {
                let name = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("`prompt-template render` requires a template name"))?;
                let args: Vec<String> = it.cloned().collect();
                Ok(Command::PromptTemplateRender { name, args })
            }
            other => Err(usage(format!(
                "expected `prompt-template list|render`, got {other:?}"
            ))),
        },
        Some("skill") => match it.next().map(String::as_str) {
            Some("list") => Ok(Command::SkillList),
            other => Err(usage(format!("expected `skill list`, got {other:?}"))),
        },
        Some("model") => match it.next().map(String::as_str) {
            Some("list") => {
                let rest: Vec<&String> = it.collect();
                let detailed = rest.iter().any(|a| a.as_str() == "--detailed");
                Ok(Command::ModelList { detailed })
            }
            other => Err(usage(format!("expected `model list`, got {other:?}"))),
        },
        Some("update") => {
            let rest: Vec<&String> = it.collect();
            let force = rest.iter().any(|a| a.as_str() == "--force");
            Ok(Command::Update { force })
        }
        Some("doctor") => {
            let rest: Vec<&String> = it.collect();
            let fix = rest.iter().any(|a| a.as_str() == "--fix");
            Ok(Command::Doctor { fix })
        }
        Some("__supervisor-main") => Ok(Command::SupervisorMain),
        Some("__worker-main") => parse_worker_main(&mut it),
        other => Err(usage(format!(
            "expected `daemon <start|status|shutdown [--force]>`, `session <new|attach|list|prompt|stop|rename|compact|heartbeat|interrupt|schedule|goal|autonomous|prompt-template|harness|refine|spawn|children|message|repl|rpc>`, `prompt-template <list|render>`, `skill list`, `model list`, `update [--force]`, `doctor [--fix]`, or `-p`/`--print <text>`, got {other:?}"
        ))),
    }
}

/// Scans `rest` (the whole remaining arg list, already materialized so
/// this can be called once per flag without exhausting an iterator) for
/// `flag <value>`. A not-found flag legitimately means "not given," not
/// an error -- `session new`'s only caller has nothing else positional
/// to worry about consuming by mistake.
/// `--thinking low|medium|high`: validated against `rp-server`'s own
/// `ReasoningConfig.effort` vocabulary (OpenAI's `effort` convention) so a
/// typo fails loudly at parse time instead of silently reaching
/// `rp-server` as an unrecognized value.
fn parse_thinking_level(value: &str) -> Result<String> {
    match value {
        "low" | "medium" | "high" => Ok(value.to_string()),
        other => Err(usage(format!(
            "unknown --thinking value `{other}`, expected `low`, `medium`, or `high`"
        ))),
    }
}

/// `--tools read|mcp`: `read` offers `tools::read_only_tool_defs`'s
/// built-in tools; `mcp` offers whatever `rp-server`'s own MCP gateway
/// currently exposes (its native tools, plus any configured
/// `[[mcp.upstreams]]`) instead -- see `PARITY.md`'s MCP entry for why
/// these two are mutually exclusive in this pass rather than merged
/// (`--tools shell`, and combining tool sources, are natural v2/v3
/// extensions of this same flag, not built now).
fn parse_tools_value(value: &str) -> Result<String> {
    match value {
        "read" | "mcp" => Ok(value.to_string()),
        other => Err(usage(format!(
            "unknown --tools value `{other}`, expected `read` or `mcp`"
        ))),
    }
}

/// `--runtime ipython`: selects `tool_runtime::ToolRuntime`'s real
/// backend (`ipython_runtime::IpythonKernelRuntime`) for this session --
/// see `Request::SessionNew::runtime`'s own doc comment for why this is a
/// separate flag from `--tools` rather than folded into it. `"ipython"`
/// is the only value accepted today; no other runtime backend exists.
fn parse_runtime_value(value: &str) -> Result<String> {
    match value {
        "ipython" => Ok(value.to_string()),
        other => Err(usage(format!(
            "unknown --runtime value `{other}`, expected `ipython`"
        ))),
    }
}

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

/// `session harness (add <id> <prompt|memory|skill> <text...>|list
/// <id>|rollback <id> <index>)`.
fn parse_harness<'a>(it: &mut impl Iterator<Item = &'a String>) -> Result<Command> {
    match it.next().map(String::as_str) {
        Some("add") => {
            let session_id = it
                .next()
                .cloned()
                .ok_or_else(|| usage("`session harness add` requires a session id"))?;
            let kind = match it.next().map(String::as_str) {
                Some("prompt") => HarnessNoteKind::Prompt,
                Some("memory") => HarnessNoteKind::Memory,
                Some("skill") => HarnessNoteKind::SkillDescription,
                other => {
                    return Err(usage(format!(
                        "expected `session harness add <id> prompt|memory|skill <text...>`, got {other:?}"
                    )))
                }
            };
            let text: Vec<String> = it.cloned().collect();
            if text.is_empty() {
                return Err(usage("`session harness add` requires note text"));
            }
            Ok(Command::HarnessUpdate {
                session_id,
                action: HarnessAction::Add {
                    kind,
                    text: text.join(" "),
                },
            })
        }
        Some("list") => {
            let session_id = it
                .next()
                .cloned()
                .ok_or_else(|| usage("`session harness list` requires a session id"))?;
            Ok(Command::HarnessShow { session_id })
        }
        Some("rollback") => {
            let session_id = it
                .next()
                .cloned()
                .ok_or_else(|| usage("`session harness rollback` requires a session id"))?;
            let index: usize = it
                .next()
                .ok_or_else(|| usage("`session harness rollback` requires a history index"))?
                .parse()
                .map_err(|_| usage("history index must be a non-negative integer"))?;
            Ok(Command::HarnessUpdate {
                session_id,
                action: HarnessAction::Rollback { index },
            })
        }
        other => Err(usage(format!(
            "expected `session harness add|list|rollback`, got {other:?}"
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
/// conventions, not with ISO 8601 itself). `pub(crate)` since
/// `session::trigger_heartbeat`/`client::session_repl` reuse it for
/// `rlm_heartbeat(every=...)`/`/heartbeat every <duration>` -- the same
/// shorthand, not a second parser to keep in sync.
pub(crate) fn parse_duration_ms(s: &str) -> Result<u64> {
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
    let mut parent_id = None;
    let mut thinking = None;
    let mut tools = None;
    let mut runtime = None;
    let mut rlm_depth = None;
    let mut rlm_max_depth = None;
    let mut spawned_from_sequence = None;
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
            "--parent-id" => {
                parent_id = Some(
                    it.next()
                        .ok_or_else(|| usage("--parent-id requires a value"))?
                        .clone(),
                )
            }
            "--thinking" => {
                thinking = Some(
                    it.next()
                        .ok_or_else(|| usage("--thinking requires a value"))?
                        .clone(),
                )
            }
            "--tools" => {
                tools = Some(
                    it.next()
                        .ok_or_else(|| usage("--tools requires a value"))?
                        .clone(),
                )
            }
            "--runtime" => {
                runtime = Some(
                    it.next()
                        .ok_or_else(|| usage("--runtime requires a value"))?
                        .clone(),
                )
            }
            "--rlm-depth" => {
                let value = it
                    .next()
                    .ok_or_else(|| usage("--rlm-depth requires a value"))?;
                rlm_depth = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| usage(format!("invalid --rlm-depth value {value:?}")))?,
                )
            }
            "--rlm-max-depth" => {
                let value = it
                    .next()
                    .ok_or_else(|| usage("--rlm-max-depth requires a value"))?;
                rlm_max_depth = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| usage(format!("invalid --rlm-max-depth value {value:?}")))?,
                )
            }
            "--spawned-from-sequence" => {
                let value = it
                    .next()
                    .ok_or_else(|| usage("--spawned-from-sequence requires a value"))?;
                spawned_from_sequence = Some(value.parse::<u64>().map_err(|_| {
                    usage(format!("invalid --spawned-from-sequence value {value:?}"))
                })?)
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
        parent_id,
        thinking,
        tools,
        runtime,
        rlm_depth,
        rlm_max_depth,
        spawned_from_sequence,
    })
}
