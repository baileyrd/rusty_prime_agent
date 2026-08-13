# Marketing claims audit

`prime-agent`'s own descriptive copy makes specific capability claims
about its two core abstractions (the Recursive Language Model / RLM, and
the Continual Harness) and the surrounding runtime. This document
fact-checks each claim against what `rusty_prime_agent` actually
implements today, and tracks the follow-up work each finding implies.
Companion to `PARITY.md` (which tracks the full feature-parity surface);
this document is scoped to just the claims below, one level more
detailed than `PARITY.md`'s entries for the same features.

Verdict legend: **True**, **False**, **Partial** (real but narrower or
conditioned differently than the claim implies).

## RLM (Recursive Language Model)

- **"Treats context as variables (prompt-as-a-variable) ... inside a
  persistent REPL."** -- **False.** A real persistent kernel exists
  (`--runtime ipython`, `src/ipython_runtime.rs`, hand-rolled ZMTP 3.0
  client verified byte-exact against a real `ipykernel`), so code
  execution itself is real. The defining RLM abstraction -- the prompt/
  context itself exposed as a Python variable the model can slice,
  summarize, or recurse into -- was never implemented. This project only
  has the execution half.
- **"Subagents are built in: `rlm(...)` spawns real child agents ... as
  function calls."** -- **False as described, true underneath.**
  Subagents are real (`session spawn`/`session children`/`session
  message`, `src/cli.rs`, `src/client.rs`), but `rlm(...)` itself --
  a Python function callable from inside kernel code -- does not exist.
  `session spawn` is a CLI/daemon-level command, not something the model
  invokes from Python. The code's own doc comments already say this:
  "bounded, non-Python parity with `rlm(...)`" (`src/client.rs`,
  `src/cli.rs`).
- **"Everything is programmatic: file operations, shell commands, tool
  use, subagents, and context management happen through code."** --
  **False.** `--tools read|mcp` is a separate, independent model-facing
  tool-calling loop (`enabled_tool_defs`, `src/session.rs:476`) that has
  nothing to do with the kernel. Subagent spawning/messaging, goals, and
  compaction are all daemon/CLI-level, not code the model writes and
  executes. Only `execute_python` itself is code-based.

## Continual Harness

- **"Stores supplemental prompts, memories, skill descriptions ... as
  durable state."** -- **True.** `HarnessState { notes, history }`
  (`src/protocol.rs`), mutated via `session harness add/list <id>`.
- **"Recorded snapshots support rollback."** -- **True.** Every `Add`/
  `Rollback` appends a `HarnessSnapshot` to `history`
  (`src/session.rs:1012-1052`); `session harness rollback <id> <index>`
  restores an earlier one, and the rollback itself becomes a new history
  entry rather than erasing anything.
- **"`/refine` reviews the trajectory and applies a small,
  evidence-backed update ... never rewrites the immutable base system
  prompt."** -- **Partial.** `session refine <id>` does review the last
  20 transcript entries and append one evidence-backed `Memory` note --
  that much is real (`src/client.rs:1566` `session_refine`). But:
  - This project has no "immutable base system prompt" object at all (no
    `SYSTEM.md`/`APPEND_SYSTEM.md` support), so the "never rewrites it"
    guarantee is vacuous rather than an enforced safeguard -- there's
    nothing for `/refine` to be tempted to touch in the first place.
  - **Harness notes are never fed back into future prompts.**
    `build_turns` (`src/session.rs:732`, what's actually sent to the
    model each turn) only injects `AGENTS.md`/`CLAUDE.md` and the
    compaction summary. Harness notes are stored, rollback-able, and
    used as input to `/refine`'s own one-shot review prompt -- but not
    automatically re-injected as context on ordinary turns. The harness
    records history faithfully; it does not yet close the loop back into
    the agent's behavior.

## Skills

- **"Skills are executable: skills are importable Python packages."** --
  **True.** Confirmed real and importable inside the kernel via
  `sys.path` injection (`worker::bootstrap_kernel`, `src/skills.rs`).
- **"The built-in skill creator can turn recurring workflows into
  project or personal skills."** -- **False.** No such tool/command
  exists. Skills must be hand-authored on disk (`SKILL.md` + package
  directory); nothing in this codebase generates one.

## Background sessions

- **"Daemon-backed agents keep running when the terminal disconnects and
  can be reattached later."** -- **True.** Core to the daemon/worker
  architecture; `session attach`/`session repl` reattach to a running
  worker via its private `worker.sock`.

## Agent-to-agent communication

- **"Agents communicate directly ... without routing everything through
  the user."** -- **Partial.** `session message <from-id> <to-id>
  <text...>` exists and delivers directly without user relay, but it's
  restricted client-side to a session's own parent or its own children
  (`src/client.rs:435`) -- not "any agents can message any agents."

## Long-running tasks

- Automatic compaction, persistent goals, heartbeats (including
  interval-repeating), schedules, and bounded autonomous mode -- **all
  true**, all implemented (see `PARITY.md`'s corresponding entries for
  each).
- **"Retained subagents."** -- **False as a distinct feature.** No such
  named concept exists here; subagents are ordinary sessions with a
  recorded `parent_id`, nothing separately "retained."

## Architecture overview (system diagram + prompt execution flow)

`prime-agent`'s architecture doc describes a client/supervisor/worker
split with a flowchart and a prompt-execution sequence diagram. Checked
each labeled component and edge against this project's actual daemon/
worker/session code.

- **`AgentConnection` (client-side execution boundary).** -- **True in
  substance, no matching name.** `src/client.rs`'s functions
  (`session_prompt`, `session_attach`, `session_repl`, ...) play exactly
  this role -- they own rendering/stdin, never execute a prompt
  themselves, and talk to the daemon over `transport::connect`. There's
  no single `AgentConnection` type; it's a set of free functions sharing
  the same daemon-socket pattern.
- **Daemon supervisor -- routing, attachments, recovery.** -- **True.**
  `src/daemon/mod.rs`'s `handle_session_attach`/`handle_session_prompt`
  open a fresh connection to the target session's private `worker.sock`
  per request and proxy responses/events back to the client connection
  -- this matches the diagram's `S->>W` routing step directly, not just
  in spirit.
- **Catalog process -- saved-session scans.** -- **False as "process."**
  `src/catalog.rs`'s own module doc says so explicitly: the reference
  architecture runs this as a separate subprocess, but this project runs
  it in-process inside the supervisor, citing the project's Phase 1
  "modular monolith" constraint -- a deliberate, already-documented
  divergence, not an oversight.
- **Session worker: "one root session tree"** (one `AgentSessionRuntime`
  hosting a root `AgentSession`, a `Scheduler`, a root IPython kernel,
  and RLM child runtimes as descendants, all inside one process). --
  **False.** `worker::run` (`src/worker/mod.rs:190`) takes exactly one
  `session_id` and builds exactly one `AgentSession` -- there is no
  runtime object that owns a tree of child sessions in-process.
  `session spawn` (`src/client.rs:351`, the subagent mechanism) creates
  a fully independent session through the ordinary `create_session` path
  -- its own daemon-spawned worker process, its own `worker.sock` -- and
  links it to the parent only via the `parent_id` field and later
  daemon-routed `session message` calls. A parent's worker process has
  no in-process handle to any child's runtime at all.
- **"IPython is the model-facing control environment. Typed host
  requests return authoritative operations to the TypeScript session."**
  -- **Partial.** The kernel is real and model-facing for `--runtime
  ipython` sessions (see the RLM section above). But there is no general
  typed request/response channel from kernel code back to the Rust host.
  The only kernel-to-host signal that exists is the single hardcoded
  stdout marker (`HEARTBEAT_MARKER`, `src/session.rs:56`) used
  exclusively to trigger `trigger_heartbeat` -- a one-off convention, not
  a general "typed host request" protocol a kernel call could use for
  other authoritative operations.
- **"Workers and kernels are separate processes for lifecycle and
  failure containment, not security sandboxes. They normally run with
  the same operating-system permissions as the client."** -- **True.**
  Workers are spawned as `harness __worker-main` subprocesses
  (`src/worker/mod.rs`), kernels as their own subprocess under
  `IpythonKernelRuntime`; neither uses any sandboxing/privilege-dropping
  mechanism, same as the client's own OS user.
- **Prompt execution sequence diagram** (`U -> C -> S -> W -> A -> P`,
  streamed back through the same chain, transcript appended, "opt
  IPython tool call" branching into a "typed host request" vs. "ordinary
  execution" alternative). -- **True for the outer flow, false for the
  inner branch.** The client/daemon/worker/`AgentSession`/provider chain,
  transcript append, and event streaming back to the client all match
  what `handle_session_prompt`/`AgentSession::prompt` actually do,
  including the "generation-aware events" detail (`SessionState.
  generation`, `src/protocol.rs:531`, bumped on every respawn precisely
  so attach-stream cursors can detect it). The "typed host request" arm
  of the IPython branch does not exist as a general mechanism, per the
  point above -- only the heartbeat marker special case does.
- **"From the session queue onward, the same execution and persistence
  path is used when a prompt comes from a heartbeat, cron schedule, goal
  continuation, autonomous mode, or another agent instead of an attached
  user."** -- **True.** Heartbeats and schedules fire through the
  daemon's background schedule loop into an ordinary `Request::
  SessionPrompt`; `session_autonomous`'s goal continuation and `session
  message` both send ordinary `SessionPrompt`s too -- none of these
  sources bypass `AgentSession::prompt`.

## Candidate follow-ups

Findings above that describe a real, bounded gap rather than an
out-of-scope Python-first redesign -- listed as candidates for future
work, not yet decided or scheduled:

- [ ] **Feed harness notes back into `build_turns`.** The only concrete,
      bounded gap found in the Continual Harness's actual behavior:
      notes are durable and rollback-able but invisible to the model
      except via `/refine`'s own review prompt. A bounded slice would
      inject `state.harness.notes` as an additional system turn in
      `build_turns` (`src/session.rs:732`), the same place `AGENTS.md`/
      compaction already hook in -- needs a design decision on ordering/
      formatting relative to those two, and on whether all notes or only
      certain `HarnessNoteKind`s belong in the live prompt.
- [ ] **A `session skill create` command** (skill scaffolding: writes a
      `SKILL.md` + package skeleton from a name/description, does not
      need to synthesize skill *logic*) -- would close the "built-in
      skill creator" gap without requiring the model to author working
      Python on its own.
- [ ] **Generalize the heartbeat marker into a real typed host-request
      channel.** Currently one hardcoded stdout marker for one specific
      trigger. If a second kernel-to-host authoritative operation is
      ever needed, a small tagged-JSON-on-stdout convention (marker +
      one JSON payload, parsed the same way `extract_heartbeat_marker`
      already parses `marker + every`) would generalize this without
      much new machinery -- not worth building ahead of a second actual
      caller, so left as a candidate rather than started.

Not candidates -- structurally out of scope, same reasoning as
`PARITY.md`'s "Needs a new subsystem" section: prompt-as-a-variable,
`rlm(...)` as an in-kernel Python callable, and routing all tool use/
subagent orchestration through kernel code. Each would require the
Python-first control environment this project has deliberately not
built (see `PARITY.md`'s "RLM programming model" and "Recursive
subagents" entries for why). Also not a candidate: restructuring the
worker model so a parent process hosts child runtimes in-process
("one root session tree") instead of today's one-process-per-session
design -- that's a foundational rewrite of the daemon/worker
architecture (crash isolation, `parent_id`-based routing, the whole
socket-per-session model), not a bounded slice, and today's design
already gets the properties that matter (independent lifecycle,
independent crash recovery, daemon-mediated messaging) through a
different, already-working mechanism.
