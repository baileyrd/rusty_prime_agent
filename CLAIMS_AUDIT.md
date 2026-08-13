# Marketing claims audit

`prime-agent`'s own descriptive copy makes specific capability claims
about its two core abstractions (the Recursive Language Model / RLM, and
the Continual Harness) and the surrounding runtime. This document
fact-checks each claim against what `rusty_prime_agent` actually
implements today, and tracks the follow-up work each finding implies.
Companion to `PARITY.md` (which tracks the full feature-parity surface);
this document is scoped to just the claims below, one level more
detailed than `PARITY.md`'s entries for the same features.

The first pass below covers `prime-agent`'s top-level marketing copy and
`architecture.md`. A second pass extends the same treatment to the four
detailed docs `architecture.md` links out to: `daemon.md`,
`agent-connection.md`, `rlm-runtime.md`, and `long-running-agents.md`.
Claims that only describe TypeScript-specific implementation details
(exact class/file names, benchmark scripts) with no architecturally
meaningful analog to check are marked **N/A** rather than forced into a
verdict.

Verdict legend: **True**, **False**, **Partial** (real but narrower or
conditioned differently than the claim implies), **N/A**.

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

## Daemon Architecture (`daemon.md`)

- **"Each active root session tree in its own process."** -- **Partial.**
  True that each session gets its own process; false that a "tree" (root
  + RLM descendants) shares one -- confirmed above, every session
  including subagents is a fully separate worker process.
- **Catalog subprocess.** -- **False**, same finding as the system
  diagram's `catalog` node: `src/catalog.rs` runs in-process inside the
  supervisor, a deliberate, already-documented Phase-1 decision.
- **"Closing the TUI detaches the client; it does not stop the
  worker."** -- **True.** Core to the daemon/worker split; `session
  stop` is the only thing that actually shuts a worker down.
- **Client-owned workers: "Direct SDK calls to print and RPC modes
  remain in-process."** -- **False.** Checked directly: `print_once`
  (`src/client.rs:123`) calls `ensure_daemon_started` and creates a real
  session through the daemon before prompting -- `-p`/`--print` and
  `--mode rpc` both go through the same daemon-backed worker path as an
  interactive session, not an in-process shortcut. There's no
  "client-owned worker with a cleanup grace period" concept at all; a
  print-mode session is an ordinary session left behind on disk.
- **Session leases keyed by canonical path, `session_already_active`
  conflicts.** -- **Partial.** A real conflict-signaling mechanism
  exists (`daemon::Supervisor`'s `spawn_lock` plus `Response::Error {
  conflict: true }`, one call site's own doc comment explicitly calling
  it "session_already_active-shaped"), but it's an in-process `tokio`
  mutex inside one supervisor, not a cross-process file lease keyed by
  the session's on-disk path -- there's only ever one supervisor per
  `state_root` in this project's model, so the stronger cross-process
  primitive was never needed.
- **Scheduling: per-session persisted jobs, not a global cron file.** --
  **True.** `schedule::read_all`/`write_all` operate on
  `paths::schedules_path(session_dir)` -- one file per session, matching
  this claim exactly.
- **Public daemon protocol v4: versioned envelopes, capability
  negotiation, chunked snapshot streaming (512 KiB target), file-backed
  transcript caches above 4 MiB.** -- **False.** `protocol.rs` has no
  request-level version/capability negotiation (only a bare
  `protocol_version: u32` field inside `Response::DaemonStatus`, a
  status readout, not a negotiated envelope), no chunked
  begin/chunk/end snapshot streaming, and no size-based transcript
  caching -- every attach sends one `SessionEvent::Snapshot` with the
  full transcript in memory.
- **Private worker transport: fixed binary frame (4-byte header + 4-byte
  payload length + routing header), zero-copy payload forwarding.** --
  **False.** `src/transport.rs` uses plain JSONL over the Unix socket
  for both the public and private sockets -- one shared, simpler framing
  scheme, not a distinct binary protocol with routing-header-only
  inspection.
- **Idempotency: mutating commands keyed by `clientId + commandId`,
  recorded in an append-only journal before dispatch, uncertain results
  reported rather than blindly replayed.** -- **False.** No client
  ID/command ID concept exists anywhere in this project's protocol or
  client code -- confirmed by grep, nothing produces or consumes one. A
  request that times out or whose connection drops before a response
  arrives has no idempotent-replay protection; a retried `SessionPrompt`
  after a timeout could in principle double-send.
- **Coordinated two-phase self-update (checkpoint, validate, commit).**
  -- **False/N/A.** No self-update mechanism exists in this project at
  all.
- **Backpressure is attachment-local; no unbounded per-client queue.**
  -- **Not directly verified**, but plausible by construction: each
  attach is proxied through one dedicated `LineStream` per connection
  with no shared broadcast buffer in `handle_session_attach`, so a slow
  reader would block only its own proxy loop rather than a shared queue.
  Not exercised by any load test in this project the way `daemon.md`'s
  own benchmark scripts (`daemon-multiclient-bench.ts`, N/A -- no
  equivalent exists here) exercise it upstream.
- **Benchmarks (`daemon-multiclient-bench.ts`, stress test with 50
  workers).** -- **N/A.** TypeScript-specific tooling; this project has
  no equivalent bench harness.

## Agent Connection Architecture (`agent-connection.md`)

- **`AgentConnection`/`DaemonAgentConnection`/`InProcessAgentConnection`
  as a named type hierarchy.** -- **Partial.** The boundary these types
  enforce is real here too -- `client.rs`'s functions never touch
  session state directly, only through daemon requests -- but it's
  expressed as a set of free functions sharing a connection pattern, not
  a formalized adapter interface with two concrete implementations.
  `InProcessAgentConnection`'s closest analog is the embeddable SDK
  ([lib] target, task #60): constructing an `AgentSession` directly with
  no daemon, the same idea in a different shape.
- **Cursor-based reconnect: attach accepts a resume cursor, replays only
  the missing interval, and reports complete/partial/unavailable.** --
  **False.** `session.rs`'s own module doc says so directly: "Full JSONL
  replay, not periodic snapshot + tail." Every `session attach` (and
  every `dispatch_one_shot`) always replays the entire transcript from
  scratch; there is no `{generation, sequence}`-cursor-based partial
  resume path, even though `SessionState.generation` and per-event
  sequence numbers both exist and could in principle support one later.
- **Command lifecycle idempotency (`clientId + commandId` journal).** --
  **False**, same finding as `daemon.md` above.
- **Extension UI boundary (select/confirm/input/editor/notification
  dialogs, executable callbacks staying host-side).** -- **False/N/A.**
  No extension system exists in this project (see `PARITY.md`'s
  Extensions entry) -- there is nothing for this boundary to protect.
- **"If an action changes agent execution or persisted session state, it
  goes through `AgentConnection`. If it changes only terminal
  presentation or local preference UI, it stays client-side."** --
  **True in spirit.** `client.rs` never mutates `SessionState`/
  `transcript.jsonl` directly; every state change is a `Request` sent to
  the daemon. Local-only concerns (REPL prompt rendering, `--mode`
  output formatting) stay entirely in `client.rs` with no daemon round
  trip.

## RLM Runtime Architecture (`rlm-runtime.md`)

Mostly **False/N/A**, consistent with the RLM findings above -- `rlm(...)`
as a kernel-callable function does not exist, so most of this document's
claims (comm target `host.request`, `RLMSpawnHandle`, `RLM_DEPTH`/
`RLM_MAX_DEPTH`, `rlm.list_subagents()`/`rlm.delete_subagent()`, model
search via `rlm.find_models()`) have no analog to check. The specific
points worth calling out individually:

- **Persistent kernel, Jupyter protocol over ZeroMQ, HMAC-SHA256 signed
  frames.** -- **True.** `zmtp.rs`/`sha256.rs`, hand-rolled and verified
  byte-exact against a real `ipykernel`.
- **Three Jupyter channels: shell, iopub, control -- control used
  specifically so a host-request reply doesn't deadlock a running
  cell's `execute_request`.** -- **False, and this is the direct root
  cause of the earlier "typed host request" finding.** `ipython_runtime.rs`
  connects only `shell` and `iopub` sockets -- there is no control-channel
  connection at all. This is exactly why `HEARTBEAT_MARKER` is a stdout
  hack instead of a real host-request protocol: the mechanism
  `rlm-runtime.md` describes for avoiding a shell-channel deadlock
  (replying on `control` instead) isn't available here because the
  control channel was never wired up.
- **Usage/cost attribution: child assistant usage folded into the parent
  turn via a `child_usage_attributed` transcript entry.** -- **False.**
  Confirmed absent by grep -- `session spawn` creates a fully independent
  session with its own, separately-tracked usage; nothing links a
  child's token usage back to the parent's turn.
- **Continual Harness storage: `harness/harness_state.json` per session
  artifact directory, plus an explicitly global scope under
  `~/.prime/agent/harness/`.** -- **False.** `HarnessState` lives inline
  inside each session's own `state.json` (`SessionState.harness`), not a
  standalone file -- and there is no global, cross-session harness scope
  at all; every harness note is scoped to exactly one session.
- **`/refine`'s base system prompt stays immutable; rollback uses
  recorded snapshots.** -- **Partial**, same finding as the marketing-copy
  section above (rollback is real; "immutable base system prompt" is
  vacuous since no such object exists here to protect).
- **Trust boundary: kernel runs model-generated code with the worker's
  own OS permissions; not a sandbox; use an external sandbox for
  untrusted workspaces.** -- **True**, matches this project's own stated
  design almost verbatim (see the system-diagram section above).

## Long-Running and Background Agents (`long-running-agents.md`)

- **Daemon-backed sessions survive detach; `list`/`attach`/`rename`/
  `stop`/`status` lifecycle commands.** -- **True** for all but one:
  `session list`/`session attach`/`session rename`/`session stop`/
  `daemon status` all exist and match. **`doctor [--fix]`** --
  **False**, confirmed absent: no diagnostic/repair command exists in
  this project.
- **Agent-to-agent messaging via a kernel-callable `agent_message`
  Python skill, with `auto`/`steer`/`follow_up` delivery modes, a
  `deliveryStatus` receipt, and `agent_message.send("all", ...)`
  broadcast.** -- **False.** `session message` is CLI/daemon-level only
  -- not callable from inside the kernel -- delivers as an ordinary,
  unconditional `SessionPrompt` with no steering-vs-follow-up delivery
  mode (this project's REPL has no concept of "steering" at all, see
  `PARITY.md`'s TUI entry) and no broadcast target.
- **Three heartbeat surfaces (`/heartbeat` for the user,
  `rlm_heartbeat` for the agent, `schedule` for general automation).**
  -- **Partial.** All three surfaces genuinely exist here -- `/heartbeat`
  (with an `every <duration>` interval form), `rlm_heartbeat(every=...)`
  callable from the kernel, and `session schedule add/list/cancel` --
  but each is a narrower, single-instance version of `prime-agent`'s
  richer design: no `/heartbeat status|pause|resume|clear`, no `
  --follow-up` delivery-mode flag, and no multi-heartbeat
  create/list/update-with-labels surface (`rlm_heartbeat()` here is one
  implicit per-session trigger, not `rlm_heartbeat.create(...)` returning
  an addressable, independently pausable entry).
- **Persistent goals: `/goal [--budget N]`, `status|pause|resume|
  clear`, kernel-callable `goal.get()`/`goal.complete()`.** --
  **Partial.** `session goal set|show|pause|resume|complete|clear`
  all exist and match. **No `--budget` token-budget flag** (confirmed
  absent by grep) and **no kernel-callable `goal` Python skill** --
  goal state is CLI/daemon-level only, not something `execute_python`
  code can query or complete.
- **Autonomous mode: turn/time/token limits, a quality-gate command,
  and avoiding a rerun of the same failed gate when the workspace hasn't
  changed.** -- **Partial.** `session autonomous --max-turns
  --max-time --quality-gate` all exist and run for real (checked
  `run_quality_gate`, a real subprocess invocation). No token-budget
  limit (this project's own doc comment says so explicitly: "No token
  budget"), and no rerun-avoidance -- `run_quality_gate` always
  re-executes the gate command unconditionally, with no tracking of
  whether the workspace changed since the last run.
- **Compaction: automatic on overflow, kernel persists through it,
  kernel-callable `compact.status()`/`compact.run(...)`.** --
  **Partial.** Automatic compaction is real (task #53) and the IPython
  kernel is a separate long-lived process genuinely unaffected by
  transcript compaction. **No kernel-callable `compact` Python skill**
  exists -- compaction is triggered automatically inside `maybe_compact`
  or not at all; the model cannot request it from code.

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
- [ ] **`daemon doctor [--fix]`.** No diagnostic/repair command exists.
      A bounded first slice: check daemon-socket reachability, scan for
      orphaned `worker.sock` files with no live process behind them
      (`is_worker_alive` already does the liveness check `session
      list` uses), and report findings; `--fix` could remove confirmed-
      orphaned socket files. No self-repair beyond that without a much
      larger scope.
- [ ] **`/heartbeat status|pause|resume|clear` and a `--follow-up`
      flag.** Today's `/heartbeat`/`rlm_heartbeat(every=...)` are each a
      single implicit per-session trigger with no way to inspect,
      pause, or cancel a running interval short of overwriting it with
      another `/heartbeat every ...` call. Bounded slice: track the
      active heartbeat's schedule-entry id on `SessionState` so these
      subcommands have something to target; `--follow-up` would need
      the same "ordinary prompt vs. explicit follow-up" distinction
      `session message`'s delivery-mode gap below would also need, so
      worth doing together if either is picked up.
- [ ] **Idempotent replay protection for in-flight requests.** A
      `SessionPrompt` whose response times out or whose connection drops
      has no way to know whether the prompt was actually enqueued before
      retrying -- unlike `daemon.md`'s `clientId + commandId` journal.
      Bounded first slice: a small in-memory (not necessarily durable)
      per-worker dedup keyed by a client-supplied request id, rejecting
      an exact duplicate rather than double-enqueuing -- durability
      across a worker crash is a separably larger step.

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
