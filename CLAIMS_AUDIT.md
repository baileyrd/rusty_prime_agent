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
`agent-connection.md`, `rlm-runtime.md`, and `long-running-agents.md`. A
third pass recursively follows every doc link reachable from `README.md`
-- `README.md`'s own "Documentation" links plus `docs/index.md`'s full
listing -- covering every remaining doc in `packages/coding-agent/docs/`:
`quickstart.md`, `usage.md`, `sessions.md`, `session-format.md`, `rlm.md`,
`skills.md`, `compaction.md`, `prompt-templates.md`, `json.md`, `rpc.md`,
`acp.md`, `mcp-integrations.md`, `sdk.md`, `providers.md`,
`custom-provider.md`, `models.md`, `settings.md`, `extensions.md`,
`themes.md`, and `tui.md`. Docs that are pure OS/tooling setup
instructions with no runtime-behavior claims to check (`keybindings.md`,
`packages.md`, `shell-aliases.md`, `terminal-setup.md`, `termux.md`,
`tmux.md`, `windows.md`, `development.md`) were read but produced no
checkable claims and are omitted below.

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
  function calls."** -- **Now True (was False as described, true only
  underneath).** Originally: subagents were real (`session spawn`/
  `session children`/`session message`) but `rlm(...)` itself -- a
  Python function callable from inside kernel code -- did not exist;
  `session spawn` was CLI/daemon-level only. **Closed**: `rlm(task,
  name=None, model=None)` is now a real kernel-callable coroutine
  (`worker::bootstrap_kernel`), admitting a child session through the
  same `SessionNew`/`ScheduleAdd` daemon round trip `session spawn`
  already used, just callable as `await rlm("task")` from kernel code
  instead of the CLI. See the RLM Runtime Architecture section below for
  the mechanism. Recursion depth limits (`RLM_DEPTH`/`RLM_MAX_DEPTH`), a
  parent-scoped child registry (`rlm_list_subagents()`/
  `rlm_delete_subagent()`), and child usage/cost attribution to the
  parent turn are now all real too -- see below.
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
  -- **Now Partial-leaning-True (was Partial-leaning-False).** The
  kernel is real and model-facing for `--runtime ipython` sessions (see
  the RLM section above). Originally found without any general typed
  request/response channel -- the only kernel-to-host signal was the
  hardcoded `HEARTBEAT_MARKER` stdout convention. **Closed at the
  mechanism level**: a real `host.request` Jupyter comm protocol now
  exists (`tool_runtime::HostRequest`, `ToolRuntime::resume_execute`,
  kernel-side `host_request(kind, payload)` defined in `worker::
  bootstrap_kernel`) -- `IpythonKernelRuntime::execute` pauses with
  `pending_host_request` set when the kernel opens a `host.request` comm
  and blocks awaiting a reply, and `resume_execute` delivers one over
  `control` and lets the cell finish. **Fully closed now**:
  `execute_python_tool_call` loops on `pending_host_request`, dispatching
  to `AgentSession::handle_host_request`; `"rlm.run"` is the first real
  request `kind`, backing a kernel-callable `rlm(task, ...)` -- see
  the marketing-copy section above and RLM Runtime Architecture below.
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
  execution" alternative). -- **Now True for both the outer flow and the
  inner branch (was "mechanism real but unwired").** The client/daemon/
  worker/`AgentSession`/provider chain, transcript append, and event
  streaming back to the client all match what `handle_session_prompt`/
  `AgentSession::prompt` actually do, including the "generation-aware
  events" detail (`SessionState.generation`, `src/protocol.rs:531`,
  bumped on every respawn precisely so attach-stream cursors can detect
  it). **Closed**: `session.rs`'s own tool-calling loop
  (`execute_python_tool_call`) now loops on `pending_host_request`,
  dispatching to `AgentSession::handle_host_request` and
  `ToolRuntime::resume_execute` until the cell finishes -- the "typed
  host request" arm is real machinery with a live caller now, not
  unwired.
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

**Originally mostly False/N/A** -- `rlm(...)` as a kernel-callable
function didn't exist, so most of this document's claims had no analog
to check. **Since closed in part**: `rlm(task, name=None, model=None)`
is now real (a kernel-callable coroutine over the `host.request` comm
protocol, admitting a child session through the same daemon round trip
`session spawn` uses) -- see the marketing-copy section above for the
mechanism. `RLM_DEPTH`/`RLM_MAX_DEPTH` recursion limits are now real too
(see below), and so is a parent-scoped child registry -- `await
rlm_list_subagents()`/`await rlm_delete_subagent(id)`, backed by
`Request::SessionList` filtered by `parent_id` and `Request::
SessionStop` respectively, no separate registry data structure (see
below). Child usage/cost attribution to the parent turn is real too now
(see below). Still genuinely absent: `RLMSpawnHandle` as a typed object
(the reply is a plain dict, not an attribute-access object), an `rlm`
namespace object (`rlm_list_subagents`/`rlm_delete_subagent` are bare
top-level functions, same simplification `rlm(...)` itself already
made), and model search via `rlm.find_models()` -- each tracked
separately below. The specific points worth calling out individually:

- **Persistent kernel, Jupyter protocol over ZeroMQ, HMAC-SHA256 signed
  frames.** -- **True.** `zmtp.rs`/`sha256.rs`, hand-rolled and verified
  byte-exact against a real `ipykernel`.
- **Three Jupyter channels: shell, iopub, control -- control used
  specifically so a host-request reply doesn't deadlock a running
  cell's `execute_request`.** -- **Now True (was False).** Originally
  found False: `ipython_runtime.rs` connected only `shell`/`iopub`, the
  direct root cause of `HEARTBEAT_MARKER` being a stdout hack instead of
  a real host-request protocol. **Closed**: `control` (DEALER, same
  6-frame `<IDS|MSG>` framing as `shell`, confirmed by direct raw-socket
  probing against a real `ipykernel` before writing any Rust) is now
  connected in `start()`, with `send_control`/`recv_control` sharing
  `build_signed_message` with `send_shell`/`recv_shell`. Given real,
  immediate use in `shutdown()` (a graceful `shutdown_request` before
  the process-kill fallback, also verified against a real kernel) rather
  than sitting unused until the host-request protocol lands. The
  `HEARTBEAT_MARKER` stdout hack is untouched for now -- generalizing it
  into a real `host.request` comm protocol over this channel is the next
  increment.
- **`AgentSession.runRlmChild()` checks `RLM_DEPTH < RLM_MAX_DEPTH`
  before admitting a child; children inherit the parent's own maximum
  depth.** -- **Now True (was False).** Originally found False: `rlm(...)`
  admitted children with no depth check at all. **Closed**:
  `SessionState.rlm_depth`/`rlm_max_depth` are now persisted fields,
  checked client-side in `handle_rlm_run` before a child is ever admitted
  (`RLM_DEPTH >= RLM_MAX_DEPTH` returns `{"error": ...}` instead of
  spawning). The daemon computes both centrally in `handle_session_new`:
  a child gets `parent.rlm_depth + 1` and the parent's own
  `rlm_max_depth` inherited unchanged; a root session gets depth `0` and
  `RUSTY_PRIME_AGENT_RLM_MAX_DEPTH` (default `1`, matching this
  document's own stated default).
- **"The TypeScript parent maintains the authoritative direct-child
  registry"; `list_subagents()` returns stable child IDs/session IDs/
  names/directories/running-or-completed status; `delete_subagent()`
  accepts an exact child ID, session ID, or unique name, cancels/closes
  the runtime, and does not erase the transcript or artifacts on disk.**
  -- **Now True (was False).** Originally found False: no registry, no
  `list_subagents`/`delete_subagent` of any kind. **Closed**: two new
  `handle_host_request` kinds, `"rlm.list_subagents"`/
  `"rlm.delete_subagent"`, back kernel-callable `rlm_list_subagents()`/
  `rlm_delete_subagent(id)`. No separate registry data structure exists
  -- a child's own persisted `parent_id` (set once, at admission) already
  is the durable record, so `list_subagents()` is `Request::SessionList`
  filtered to this session's own direct children, and `delete_subagent()`
  resolves `id` against that same filtered set (by session id, falling
  back to an exact unique name match) before issuing `Request::
  SessionStop` -- gracefully stopping the worker, leaving `state.json`/
  `transcript.jsonl` untouched, matching "does not erase the transcript
  or artifacts on disk" exactly. Only direct children are visible/
  deletable -- an unrelated session or a grandchild is rejected the same
  as an unknown id, matching "parent-scoped." No separate durable
  tombstone record is written; the stopped child's own persisted `status:
  Stopped` already serves that purpose. `active-session ID` and `session
  ID` collapse to the same concept here (no separate slot-id layer, same
  simplification `rlm_child_id` already made).
- **Usage/cost attribution: child assistant usage folded into the parent
  turn via a `child_usage_attributed` transcript entry.** -- **Now True
  (was False).** Originally found False, confirmed absent by grep: no
  session anywhere recorded a model call's own token usage, not even for
  its own turns, so there was nothing to fold into anything. **Closed in
  two parts**: (1) real per-turn usage tracking, closing the underlying
  gap first -- `provider::parse_response` now reads `rp-server`'s own
  top-level `usage: {prompt_tokens, completion_tokens, total_tokens}`
  object (confirmed already present in a real response body, not
  invented -- this project was simply discarding it), and
  `TranscriptEntry.usage` persists it on every real assistant turn; (2)
  `session::AgentSession::attribute_child_usage`, triggered by a new
  daemon background poll once a child's own worker stops (the closest
  real "child task finished" signal this project's architecture has),
  sums the child's own transcript usage and appends a `Role::System`
  entry with `child_usage_attributed: Some(ChildUsageAttribution {
  child_session_id, parent_message_sequence, child_usage, aggregate_usage
  })` to the parent's own transcript -- idempotent via a scan of the
  parent's own transcript, not a separate registry. `parent_message_
  sequence` is this project's `sequence` field playing the role of "the
  target parent assistant message ID," captured once at admission
  (`SessionState.spawned_from_sequence`).
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
  broadcast.** -- **Partial (was fully False).** **Closed**: a
  kernel-callable `agent_message.send(message, receiver_role="parent"|
  "child", receiver_name=...)` skill now exists (`worker::bootstrap_kernel`,
  see the RLM Runtime Architecture section above), reusing `session
  message`'s own delivery underneath. **Still False**: no steering-vs-
  follow-up delivery mode (this project's REPL has no concept of
  "steering" at all, see `PARITY.md`'s TUI entry), no `deliveryStatus`
  receipt (the reply is just `{"delivered_to", "sequence"}`), and no
  `"all"` broadcast target (only a specific parent or one named direct
  child).
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
  **Partial (one clause now closed).** `session goal set|show|pause|
  resume|complete|clear` all exist and match. **Closed**: a
  kernel-callable `goal` Python skill now exists (`goal.get()`/
  `goal.create(task, token_budget=...)`/`goal.complete()`, see the RLM
  Runtime Architecture section above) -- `execute_python` code can now
  query and complete the session's own goal. **Still False**: no
  `--budget` token-budget flag on `/goal`/`session goal set` (confirmed
  absent by grep), and `goal.create`'s own `token_budget` parameter is
  accepted but not enforced -- this project's `GoalState` still has no
  token/wall-clock budget concept anywhere for it to hook into.
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

## README additions (CLI command list, "Built for Long-Running Work")

Claims not already covered by the marketing-copy/architecture passes
above:

- **"`prime-agent agents` -- Browse running, idle, and saved sessions."**
  -- **Partial.** `session list` returns every session regardless of
  status (Active/Stopped/Crashed all show up), but it's a flat text/JSON
  dump, not a browsable picker.
- **"`prime-agent attach <agent>` -- Reattach to a running session."** --
  **True, and broader.** `session attach <id>` transparently respawns a
  `Stopped` session or recovers a `Crashed` one via `ensure_worker_running`
  (`src/daemon/mod.rs:175-204`) -- covers what upstream splits across
  `attach` and `--resume`.
- **"`prime-agent --resume <path|id>` -- Resume a saved session."** --
  **False as a distinct flag.** No `--resume`/`-r` flag exists anywhere
  in `cli.rs`; no path-based addressing exists at all, only UUIDs.
- **"`prime-agent status`."** -- **True.** `daemon status` reports
  `protocol_version`/`pid`/`generation`/`sessions_active`.
- **"`prime-agent doctor [--fix]`."** -- **False.** Confirmed absent.
- **"`prime-agent update [--force]`."** -- **False/N/A.** No self-update
  subcommand exists.
- **"`prime-agent shutdown [--force]`."** -- **Partial.** `daemon
  shutdown` does stop every active worker and the `rp-server` sidecar,
  matching the substance, but there's no `--force` flag -- shutdown is
  always unconditional.
- **"Direct agent-to-agent communication: ...can discover one another,
  exchange messages, and steer active work."** -- **Partial.** `session
  message`/`session children` deliver directly, but "steer active work"
  has no analog -- `session_repl`'s stdin loop is fully synchronous, so
  there's no way for one agent's message to interrupt another's in-flight
  turn.

## quickstart.md

- **`ANTHROPIC_API_KEY` env var / `/login` API-key path.** -- **True in
  substance** for the env-var half (`auth.json` with env-var precedence
  already covered); OAuth `/login` already confirmed False.
- **"Restart, or run `/reload`, after changing context files."** --
  **Partial.** Restarting works (context files are re-read fresh every
  `build_turns` call); no `/reload` command exists in `session_repl`.
- **`prime-agent @README.md "Summarize this"` (`@file` CLI arguments).**
  -- **False.** No `@`-prefixed file-argument parsing exists; `Print`
  takes only free-text argv.
- **Image paste, `!command` shell passthrough, `/model`/`/effort`
  scoped-model cycling.** -- **False for two of three; image paste now
  real** (bounded to "reference a local image file" via `/file`, `@`, or
  `session prompt --image` -- see `PARITY.md`'s "Interactive TUI: image
  paste support" entry). `!`-dispatch and mid-session model/thinking-
  level cycling stay False -- no `!`-dispatch exists, and model/thinking-
  level are still fixed at `session new` time with no mid-session change.
- **"Continue Later": `-c`/`-r [path|id]`.** -- **False as top-level
  flags** -- covered functionally by `session list` + `session attach`
  instead.
- **Non-interactive mode: `-p`, piped stdin, `--mode json`/`rpc`.** --
  **Partial.** All exist and work, but `print_once` (`src/client.rs:
  123-170`) never reads or merges piped stdin -- `text` comes only from
  argv.

## usage.md

- **Interactive-mode UI (startup header, `--verbose`, footer, `/usage`),
  editor features (`@` fuzzy search, Tab completion, image paste,
  external-editor hotkey).** -- **False/N/A, three clauses now real.**
  No startup header/footer/`/usage`, no external-editor hotkey -- both
  still absent. **Real now**: `@` fuzzy search and Tab completion both
  exist, in a bounded, text-only form -- Tab completes a partial
  `/command` name or (after `@`) a fuzzy-matched file path
  (`client::complete_repl_line`/`fuzzy_matches`), and any `@<path>` left
  in a submitted line expands inline into that file's content
  (`client::expand_at_references`). Image paste is also real now,
  bounded to "reference a local image file" (`/file`, `@`, or `session
  prompt --image`; see `PARITY.md`'s "Interactive TUI: image paste
  support" entry) -- not a terminal clipboard/paste-protocol capture,
  which stays out of scope for the reasons given there. No live interactive
  dropdown -- that still needs terminal cursor-positioning primitives
  `termctl` doesn't have yet, see `ARCHITECTURE.md`'s own "rich editor"
  entry. Multi-line input (`Ctrl-J`) is real too, though it wasn't named
  in this particular upstream bullet.
- **Slash-command table (23 commands).** -- **Partial, most named clauses
  now closed.** `/quit` (aliased `/exit`), `/compact`, `/heartbeat`, a
  bounded `/fork`/`/file`/`/export`/`/tree` exist, plus (closed since
  this was last written -- see PARITY.md's "full slash-command surface"
  entry) bounded forms of `/name`, `/refine` (previously a top-level-CLI-
  only command), `/session` (list-only, not the full interactive
  picker), `/model` (list-only, not mid-session switching), `/reload`
  (a confirmation, not a missing mechanism -- context files already
  re-read fresh every turn), `/new`, and `/resume`. **Still absent, real
  gaps**: `/login` (no account system to log into), mid-session `/model
  <name>`/`/effort <level>` switching (no protocol support to mutate an
  already-running session), `/clone`, `/share`. `/usage` and `/mcp
  login|logout` are also absent, for the same "no underlying data
  model/primitive exists" reason.
- **`/export [file]` exports to HTML.** -- **Partial.** `/export <path>`
  exists but writes pretty-printed JSON, not HTML.
- **`/share` (upload as a private gist).** -- **False**, already tracked
  in `PARITY.md` ("nothing on the other end to send it to").
- **Message queue (Enter=steering, Alt+Enter=follow-up, queue
  reordering).** -- **Partial, one clause now closed.** **Closed**:
  typing a follow-up while a prompt is still generating no longer blocks
  or gets dropped -- it's queued and dispatched, in order, once the
  in-flight reply lands (see `PARITY.md`'s "Interactive TUI: steering
  vs. follow-up message queue" entry). **Still False**: no `Enter`-vs-
  `Alt+Enter` keybinding distinction (there's exactly one behavior for a
  submitted line while busy: queue it), no steering at all (interrupting
  an in-flight prompt -- no cancellation primitive exists anywhere in
  this project yet), and no queue reordering/editing UI once a line is
  queued.
- **Session flags: `-c`, `-r [path|id]`, `--no-session`, `--fork
  <path|id>`.** -- **Partial.** None exist as top-level flags; `--fork`
  exists only as the `session fork <id>` subcommand, keyed by UUID not
  path.
- **CLI command list: `agents`, `list [--all]`, `attach`, `stop`,
  `rename`, `send`, `schedule`, `status`, `doctor`, `shutdown [--force]`,
  `package install/remove/list/update`, `update [--force]`, `config`.**
  -- **Partial.** `list`/`attach`/`stop`/`rename`/`schedule`/`status`/
  `shutdown` map to real subcommands. `send` maps to `session message`
  but requires an existing parent/child relationship rather than
  addressing any agent freely. `doctor`, `update`, `package *`, and
  `config` are all absent.
- **Model options: `--provider`, `--model`, `--api-key`, `--thinking`,
  `--models` (cycling).** -- **Partial.** `--model provider/id` and
  `--thinking low|medium|high` exist. No `--provider` flag (provider is
  only ever embedded in the `"provider/model"` string), no `--api-key`
  flag, no `--models` cycling.
- **Tool options: `--tools`/`-t <list>`, `--no-builtin-tools`,
  `--no-tools`.** -- **Partial.** `--tools read|mcp` exists but accepts
  only those two closed values, no short alias, no negation flags.
- **Resource options: `-e`/`--extension`, `--skill <path>`,
  `--prompt-template <path>`, `--theme <path>`, `--no-context-files`.**
  -- **False**, all of them -- skills/prompt-templates exist only via
  fixed on-disk discovery directories, with no path-flag override
  surface, and Extensions/Themes don't exist as subsystems.
- **Autonomous options: `--autonomous-max-continuations`,
  `--autonomous-gate-retries`, `--autonomous-gate-timeout-ms`,
  `--autonomous-max-tokens`.** -- **Partial.** `--max-turns
  --max-time --quality-gate` all exist and run for real. No gate-retry/
  gate-timeout tracking (one gate, checked every turn) and no
  token-budget flag at all (this project's own doc comment says so
  explicitly).
- **`--cwd`, `--system-prompt`, `--append-system-prompt`, `--offline`.**
  -- **False**, none exist; no working-directory override, and (per the
  `SYSTEM.md` gap) no system-prompt object to replace or append to.

## sessions.md

- **"Sessions auto-save to `~/.prime/agent/sessions/`. Each session is a
  JSONL file with a tree structure."** -- **Partial, one clause now
  closed.** Auto-saves too, but as a per-session *directory*
  (`state.json` + `transcript.jsonl`), not one flat JSONL file -- still
  true. **Closed**: the transcript now does have a real tree structure
  (see session-format.md below).
- **Session picker (search, sort toggle, rename, delete-via-trash).** --
  **False.** No interactive picker exists; `session list` is a flat
  print and there's no delete command at all (only manual directory
  removal).
- **`/tree`/`/fork`/`/clone` comparison.** -- **Partial, one clause now
  closed.** `/fork` has a real (bounded) analog -- `session fork` matches
  reasonably well. **Closed**: `/tree` now has a real analog too --
  `harness session tree <id>` / `/tree` in `session_repl` for display,
  `harness session set-active-leaf <id> <sequence>` / `/tree <sequence>`
  for navigation. `/clone`'s live-state duplication is still missing --
  see session-format.md below.

## session-format.md

- **"Sessions... form a tree structure via `id`/`parentId` fields,
  enabling in-place branching."** -- **Partial, one clause now closed.**
  No separate `id` field (this project addresses tree position via the
  pre-existing `sequence: u64` instead of a new id concept), but
  `TranscriptEntry::parent_sequence: Option<u64>` is a real `parentId`
  analog, and `SessionState::active_leaf_sequence` tracks the branch
  point `AgentSession::set_active_leaf` can redirect mid-session --
  in-place branching itself is real. **Closed**: a CLI/REPL surface now
  exposes it (`session tree`/`/tree` display, `session set-active-leaf`/
  `/tree <sequence>` navigation) -- still no true interactive picker, the
  same "no raw-mode UI yet" gap the rest of the TUI surface has.
- **File location: one `<session-id>.jsonl` file per session.** --
  **False.** A directory with separate `state.json`
  (pointer/recovery metadata) and `transcript.jsonl` (append-only log).
- **Session version field / migration history (v1 -> v2 -> v3).** --
  **N/A.** No version field exists; backward compatibility is handled
  field-by-field via `#[serde(default)]` instead.
- **Message type union (`UserMessage`/`AssistantMessage`/
  `ToolResultMessage`/`BashExecutionMessage`/`BranchSummaryMessage`/
  `CompactionSummaryMessage`, typed content blocks, per-message `Usage`
  with cost).** -- **Still Partial, one clause now closed.** `TranscriptEntry`
  has a flat `role` enum (plus an extra `System` role upstream doesn't
  have) and a single `text: String` field -- no typed content-block
  array, no image content, still true. **Closed**: a per-message `usage`
  object is no longer absent -- `TranscriptEntry.usage: Option<Usage>`
  (`prompt_tokens`/`completion_tokens`/`total_tokens`, no dollar-cost
  field) is real now, set on every real assistant turn.
- **`BranchSummaryEntry`, `ChildUsageAttributionEntry`, `LabelEntry`,
  `AgentStatusEntry`, `GitStateEntry`.** -- **`ChildUsageAttributionEntry`
  and `BranchSummaryEntry` now True in spirit (were False); `LabelEntry`/
  `AgentStatusEntry`/`GitStateEntry` remain False.**
  `TranscriptEntry.child_usage_attributed: Option<ChildUsageAttribution>`
  is a flat optional field, not a separate typed entry class the way a
  real message-type union would have it, but it carries exactly the data
  `ChildUsageAttributionEntry` would -- `child_session_id`,
  `parent_message_sequence`, `child_usage`, `aggregate_usage`.
  `TranscriptEntry.branch_summary: Option<Box<BranchSummary>>` is the
  same shape decision for `BranchSummaryEntry` -- `branch_leaf_sequence`,
  `entry_count`, `summary`, produced on-demand by `session::AgentSession::
  branch_summarize`, now that the tree structure this entry always
  depended on is real (see session-format.md above). `LabelEntry`/
  `AgentStatusEntry`/`GitStateEntry` still don't exist -- each still
  depends on an extension system, still absent.

## rlm.md

- **The callable `rlm` object preloaded in kernel globals
  (`await rlm(...)`, `.list_subagents()`, `.delete_subagent()`,
  `.host_request(...)`).** -- **Partial (was False).** No `rlm` object of
  any kind, namespaced or otherwise, exists in kernel globals -- there is
  no `rlm.list_subagents()`/`rlm.delete_subagent()` method-call syntax,
  and never a bare `rlm.host_request(...)` either. **What's now real**:
  the equivalent functionality, as bare top-level coroutines instead of
  namespaced methods -- `rlm(task, ...)`, `rlm_list_subagents()`,
  `rlm_delete_subagent(id)` -- all defined in `bootstrap_kernel`
  alongside `rlm_heartbeat` and the underlying `host_request(kind,
  payload)` coroutine, a deliberate, repeated simplification (see the RLM
  Runtime Architecture section above), not an oversight.
- **`goal`/`agent_message`/`compact` skills all calling
  `rlm.host_request(...)`.** -- **Now True (was False).** Originally
  found False, confirmed each had zero kernel presence -- goal/
  compaction/messaging were all CLI/daemon-level only. **Closed**: three
  new namespace objects in `bootstrap_kernel` (`goal`/`agent_message`/
  `compact`, matching upstream's own dotted-call syntax exactly, not the
  bare-function simplification `rlm(...)` itself uses) call `host_request`
  with five new kinds -- `goal.get`/`goal.create`/`goal.complete`/
  `compact.now` (all handled entirely in-process, no daemon round trip,
  since this session's own goal/compaction state lives right here) and
  `agent_message.send` (resolves `receiver_role="parent"`/`"child"` to a
  target session id, then delivers via this project's own existing
  `session message` mechanism). See the RLM Runtime Architecture
  section's own entry for the full mechanism.
- **Child usage attribution, parent-scoped registry surviving
  compaction/restart, recursion depth limits.** -- **All three now
  closed.** Recursion depth limits (`SessionState.rlm_depth`/
  `rlm_max_depth`), the parent-scoped registry (`rlm_list_subagents()`/
  `rlm_delete_subagent()`), and child usage attribution
  (`session::AgentSession::attribute_child_usage`) -- see the RLM Runtime
  Architecture section above for all three mechanisms.
- **Automatic compaction preserving kernel state.** -- **True.** The
  kernel process is untouched by compaction, which only changes
  `build_turns`'s provider-facing output.

## skills.md

- **Agent Skills standard validation (`name` format/length rules,
  directory-match check, lenient warnings on violation).** -- **False.**
  `skills::discover` only ever reads the `description` field; `name`,
  length rules, and the directory-match check are never read or
  validated, and there's no warning mechanism of any kind.
- **`disable-model-invocation: true` + `/skill:name` explicit invoke.**
  -- **False.** Neither half exists -- the field is never read, and no
  `/skill:name` command surface exists anywhere.
- **"Skills with missing description are not loaded."** -- **False**,
  inverted: a dedicated unit test
  (`a_skill_with_no_description_field_still_discovers_with_none`) proves
  a skill with no `description` still discovers fine.
- **Multiple discovery locations (global, project, package manifest,
  `settings.json` array, `--skill` flag, built-in shipped skills).** --
  **False.** Only one directory is ever scanned (global tier only,
  already documented as deliberate) -- no project tier, no manifest
  tier, no settings array, no CLI flag, and no skills ship built-in.
- **Python-backed skills: `pyproject.toml`, per-skill `src/` layout,
  editable install into a kernel venv, `[project.scripts]` CLI
  wrapper.** -- **False**, none of it. `bootstrap_kernel` does a bare
  `sys.path.insert` onto the flat skills directory -- no venv, no
  install step, no dependency management, no `pyproject.toml` detection
  at all. A skill here is "a directory with `SKILL.md`" whose sibling
  `.py` files happen to be importable, not the packaged contract
  skills.md describes.
- **Built-in skills (`prime-intellect`, `skill-creator`, `websearch`)
  shipping by default.** -- **False.** No built-in skills ship at all.

## compaction.md

- **Trigger formula `contextTokens > contextWindow - reserveTokens`,
  configurable `reserveTokens`.** -- **Partial.** A trigger genuinely
  fires automatically, but against one flat threshold
  (`compact_trigger_tokens`, default 6,000) compared to a rough
  `len()/4` token estimate -- no per-model context-window catalog and
  no separate reserve concept; one number plays both roles.
- **`keepRecentTokens` walk-backward-accumulate algorithm (default
  20,000).** -- **True in mechanism, false in default.**
  `find_compaction_fold_count` does exactly this walk, but the default
  is 2,000, and token counts use the same rough estimate, not a real
  tokenizer.
- **Split-turn handling (never cut mid-turn or between a tool call and
  its result; two-summary merge).** -- **False.** The fold walk treats
  every transcript entry identically regardless of role -- a cut can
  land immediately after a tool-call entry, separating it from its
  result.
- **Structured summary format (`## Goal`/`## Progress`/etc. sections,
  `<read-files>`/`<modified-files>` tags).** -- **False.** The
  summarization prompt just asks for "an updated, concise summary... no
  preamble" -- no structured template, no file-tracking tags.
- **`compaction.enabled` settings toggle.** -- **False.** No toggle
  exists; the only way to suppress auto-compaction is to never
  configure a real model.
- **Kernel-callable `compact.status()`/`compact.run(...)`.** --
  **False**, already settled, reconfirmed against `bootstrap_kernel`
  directly (defines nothing named `compact`).

## prompt-templates.md

- **Filename-as-command, `description`/`argument-hint` frontmatter.**
  -- **True.**
- **"`description` optional; falls back to the body's first line."** --
  **False.** No fallback exists -- a missing `description` is just
  `None`.
- **Argument grammar `$1`/`$2`/`$@`/`$ARGUMENTS`/`${@:N}`/`${@:N:L}`.**
  -- **True, verbatim.** `expand_args` implements this exactly --
  already noted in `PARITY.md` as matching upstream's own grammar.
- **Discovery: global + project-local directories, package manifest
  tier, `settings.json` array, `--prompt-template` flag,
  `--no-prompt-templates`.** -- **Partial.** The global-plus-project-
  local pair is real and matches precedence exactly; no manifest tier,
  no settings array, no CLI flags.

## json.md

- **`--mode json` event vocabulary (`agent_start`/`turn_start`/
  `message_update` with streaming deltas/`tool_execution_*`/etc.).** --
  **False.** None of these event names exist; this project's `--mode
  json` instead prints its own existing `Request`/`Response`/
  `SessionEvent` wire types as JSON lines -- a much smaller,
  non-streaming vocabulary (`ProviderReply` is a complete reply or
  complete tool-call batch, never a partial delta).
- **Session header first line (`{"type":"session","version":3,...}`
  with `cwd`/`timestamp`).** -- **False.** Different shape entirely
  (`session_attach_started`, no version/cwd/timestamp fields).

## rpc.md

- **Request/response correlation via an `id` field; generic
  `{"type":"response","command":...,"success":...}` envelope.** --
  **False.** No correlation-id field anywhere; every response is
  directly tagged by its own `type`, and errors are one shared
  `Response::Error{message, conflict}` variant, not a per-command
  `success:false` echo.
- **JSONL framing (LF-only, `\r` stripped).** -- **True**, incidentally
  -- `transport::LineStream` already does exactly this, though it's the
  same framing every subcommand uses, not something built to match this
  spec.
- **Full command set (~30 commands: `steer`, `abort`, `cycle_model`,
  `bash`, `fork`, `clone`, `export_html`, etc.).** -- **False** for the
  overwhelming majority -- only `session_prompt`, `session_compact`,
  and `session_fork` have any real analog; the rest of `protocol::
  Request`'s variants don't map to this list at all, and this list's
  other ~27 entries have no analog in `protocol::Request`.
- **`Model` object (`contextWindow`, `cost`, `reasoning`, `input`
  modalities).** -- **False.** The actual model-catalog entry carries
  only `id`/`owned_by`/`context_length` -- no pricing, reasoning-support,
  or modality fields.

## acp.md

Genuinely new information -- `PARITY.md` only knew ACP was deferred
pending a wire-shape spike, without the shapes themselves. All findings
below are **N/A** (zero ACP code exists), reported as reference for a
future spike:

- Transport: one JSON-RPC 2.0 message per line, NDJSON on stdin/stdout
  -- the same LF-delimited-JSON framing `transport.rs` already
  implements, a smaller adaptation than a from-scratch protocol.
- Exactly five methods (`initialize`, `session/new`, `session/prompt`,
  `session/cancel`, `session/close`), one session per connection.
- `session/update` mapping (assistant text -> `agent_message_chunk`,
  tool start/finish -> `tool_call`/`tool_call_update`, an IPython cell
  specifically as a `tool_call` of kind `execute`) maps directly onto
  the real `execute_python` tool with no new abstraction -- confirms
  `PARITY.md`'s own guess that a non-streaming `ProviderReply` could
  still emit one legal `session/update` chunk per turn.
- **New concrete blocker found:** `protocol::Request` has no
  cancel/abort primitive of any kind -- `session/cancel` (and rpc.md's
  `abort`) would need one added first, independent of ACP itself.
  `max_tokens` as a stop reason still has no honest backing today: real
  per-turn token usage is tracked now (`TranscriptEntry.usage`, see the
  RLM Runtime Architecture section's child-usage-attribution entry), but
  nothing consumes it as a stop condition anywhere -- `session_autonomous`
  still only tracks turns/time, not a token budget, so this is now a
  missing *policy*, not a missing data source.

## mcp-integrations.md

- **"MCP integrations are Python-backed skills the model `import`s
  inside the kernel."** -- **False**, architecturally the opposite:
  `--tools mcp` exposes MCP tools as ordinary offered tools in the
  tool-calling loop, not as skill packages the model imports.
- **`mcpServers` config in `settings.json` (per-server `type`/`url`/
  `oauth`/`headers`/`enabled`).** -- **False/N/A.** No such concept
  exists; `--tools mcp` talks to exactly one fixed endpoint (`rp-server`'s
  own built-in gateway), configured on `rp-server`'s own side, not a
  rusty_prime_agent-owned settings surface.
- **Enable-by-login lifecycle (`/login` -> OAuth, `auth.json` keyed
  `mcp:<name>`, `/mcp login|logout`).** -- **False/N/A.** No `/login`,
  no `/mcp` subcommand, no OAuth; MCP tool access is unconditional
  whenever `--tools mcp` is passed and `rp-server` is reachable.

## sdk.md

- **`AgentSession` interface (`steer`, `followUp`, `subscribe`,
  `setModel`, `cycleModel`, `navigateTree`, `abort`, `dispose`).** --
  **False** as a matching shape. The embeddable layer is just
  `AgentSession::create` + `.prompt(text)` -- none of those methods
  exist, and compaction is automatic-only, not caller-invokable.
- **`defineTool()`.** -- **Partial.** The equivalent is implementing
  `ToolRuntime`/`ModelProvider` yourself and passing a `Box<dyn
  ToolRuntime>` -- same underlying idea, no single-function
  registration API.
- **`AgentSessionRuntime` with `newSession`/`switchSession`/`fork`/
  `importFromJsonl` session-replacement API.** -- **False.** No
  stateful runtime-replacement object exists; `dispatch_one_shot` is a
  single request/response call.
- **API-key resolution priority: runtime override > `auth.json` > env
  var > fallback resolver.** -- **False, reversed.** The actual
  precedence is env var beats `auth.json`, the opposite order, with no
  runtime-override tier at all.

## providers.md

- **Subscription-based OAuth providers.** -- **False**, already
  tracked.
- **Provider table (23 named backends).** -- **Partial.** Only 4 have a
  built-in entry (`openai`/`anthropic`/`gemini`/`groq`) plus Ollama
  unconditionally; the other ~19 have no built-in wiring at all
  (hand-registerable via `providers.json` as a generic entry, but none
  pre-configured or named).
- **"Auth file credentials take priority over environment variables."**
  -- **False, and inverted.** `resolve_auth_env` explicitly skips
  `auth.json` once the matching env var is set -- env var wins, the
  opposite of the documented precedence. This is a real, deliberate
  divergence already stated plainly in `PARITY.md`'s own `auth.json`
  entry, not an oversight -- but it means this specific upstream
  sentence is false as a description of actual behavior here.
- **Key resolution: shell command, env-var-name indirection, or
  literal.** -- **Partial.** Literal and `!command` both work; the
  env-var-name-indirection form does not -- any non-`!`-prefixed string
  is sent as a literal value, so `{"key": "MY_KEY"}` would ship the
  literal string `"MY_KEY"` as a credential rather than looking up an
  env var of that name.
- **Cloud providers (Azure/Bedrock/Cloudflare/Vertex-specific auth
  flows).** -- **False.** Zero cloud-specific code exists anywhere.

## custom-provider.md / models.md

Direct analog is `providers.json`'s `CustomProvider` struct (`base_url`,
`kind` only). Checked field-by-field against both docs' configuration
tables:

- **`ProviderConfig`/Model-config fields (`name`, `apiKey`, `api`,
  `streamSimple`, `headers`, `authHeader`, `models[]`, `oauth`,
  `reasoning`, `thinkingLevelMap`, `input[]`, `cost`, `contextWindow`,
  `maxTokens`, `compat`).** -- **False, essentially all of them.**
  `CustomProvider` has exactly two fields; there is no per-model
  configuration object anywhere in the registration path at all.
- **Thinking-level map (`off`/`minimal`/`low`/`medium`/`high`/
  `xhigh`, six levels).** -- **False.** The CLI flag accepts only
  `low`/`medium`/`high` -- three levels, no per-model override.
- **Overriding a built-in provider by reusing its name (upsert-by-id
  merge).** -- **False, inverted.** A custom entry reusing a built-in
  provider's name is silently *dropped*, not merged -- confirmed by a
  dedicated unit test.
- **Custom OAuth (`oauth.login`/`refreshToken`), custom streaming API.**
  -- **False/N/A.** No streaming abstraction and no OAuth concept exist
  anywhere in the `ModelProvider` trait.

## settings.md

`Settings` has exactly two fields (`compact_trigger_tokens`,
`compact_keep_recent_tokens`). Checked every key-family the doc lists:

- **`compaction.keepRecentTokens`.** -- **Partial/True.** Real, direct
  match in semantics, different default (2,000 vs. 20,000) and flat
  instead of nested.
- **`compaction.reserveTokens`/`compaction.enabled`.** -- **False**,
  both -- no reserve concept, no toggle.
- **`defaultProvider`/`defaultModel`/`defaultThinkingLevel`, `theme`,
  update-check settings, `telemetry.*`, `retry.*`, `branchSummary.*`,
  `steeringMode`/`followUpMode`, `terminal.*`/`images.*`, `shellPath`,
  `idleEvictionMinutes`, `sessionDir`, `enabledModels`,
  `markdown.codeBlockIndent`, resource-array settings (`packages`,
  `extensions`, `skills`, `prompts`, `themes`).** -- **False, all of
  them.** None exist as `Settings` fields -- confirmed by the struct's
  complete two-field definition.
- **Two-tier global+project precedence with nested merge.** --
  **False.** Global-only, single file, no merge logic, already stated
  as deliberate in `PARITY.md`.

## extensions.md / themes.md

**Important correction to `PARITY.md`'s existing reasoning.**
`PARITY.md`'s Extensions entry currently states "there's no manifest
format, no registration API, no capability list anywhere in this
project's own reach to bound a first increment against" -- searched
against *this project's own* docs, which is accurate. But `prime-agent`'s
own `extensions.md`/`themes.md` **do** contain a concrete spec:
`extensions.md` documents a full manifest/registration format (a
default-export factory receiving an `ExtensionAPI`, `pi.registerTool()`/
`registerCommand()`/`registerShortcut()`/`registerProvider()`/`on(event,
handler)` for ~25 named lifecycle events with documented payload shapes),
and `themes.md` documents a full JSON token spec (51 required color
tokens across 6 categories, 4 value formats). Neither is implemented
here -- **False** as implemented, unchanged -- but the *reasoning* for
why this stays unattempted needs updating: it's not "no spec exists
anywhere to bound against," it's "a spec exists in prime-agent's own
docs, but building against it still needs the interactive TUI first for
Themes (nothing to render tokens onto) and is a large surface for
Extensions relative to this project's current CLI/daemon shape." See the
`PARITY.md` fix landed alongside this entry.

## tui.md

Confirms `PARITY.md`'s existing conclusion straightforwardly: describes
a full component framework (`Component`/`Focusable` interfaces,
overlay positioning, built-in widgets) that presupposes the real
interactive terminal UI `PARITY.md` already scoped separately as
"needs a new subsystem." Nothing here reveals a smaller spec than that
-- there's no way to build any of it without the TUI event loop
underneath first. **False/N/A** throughout, nothing new to flag.

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
- [x] **A real `host.request` typed channel (done, see the "RLM control
      channel" work).** `tool_runtime::HostRequest`/`ToolRuntime::
      resume_execute` plus a kernel-side `host_request(kind, payload)`
      coroutine now exist and are proven end-to-end against a real
      kernel. `rlm_heartbeat` itself was deliberately *not* migrated onto
      it yet -- the stdout marker still works and costs nothing to leave
      running alongside the new channel -- but the channel a migration
      would use is real now, not a "not worth building ahead of a second
      caller" candidate anymore. Migrating `rlm_heartbeat` and adding the
      first real request `kind` (`rlm.run`) is tracked as ongoing work,
      not a followup.
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

From the recursive doc-tree pass:

- [ ] **A protocol-level cancel/abort `Request` variant.** `protocol.rs`
      has no cancellation primitive of any kind today. Surfaced
      independently by two different docs (rpc.md's `abort`, acp.md's
      `session/cancel`) as a real prerequisite gap, not just an RPC
      nicety -- worth its own bounded slice regardless of whether RPC
      mode or a future ACP spike ever get there.
- [ ] **Fix, or explicitly document, the `auth.json`-vs-env-var
      precedence inversion.** `providers.md` states the auth file takes
      priority over environment variables; `resolve_auth_env` does the
      opposite (env var always wins, `auth.json` never even consulted
      once the var is set) -- already a deliberate, stated design choice
      in `PARITY.md`'s own `auth.json` entry, but worth a one-line note
      there marking it as a permanent divergence from upstream's
      documented contract rather than an open gap.
- [ ] **Env-var-name indirection in `auth::resolve_key`.** Only literal
      and `!command` forms exist; upstream's third form (`{"key":
      "MY_KEY"}` meaning "look up this named env var") is silently
      treated as a literal today, which would ship a garbage literal
      string as a credential if anyone copied that pattern from
      upstream's own docs. Bounded fix: try `std::env::var(raw)` when
      `raw` matches a bare-identifier shape and isn't `!`-prefixed,
      falling back to literal otherwise.
- [ ] **Skill frontmatter validation beyond `description`.** `skills::
      discover` only ever reads `description`; `name`/`license`/
      `compatibility`/`disable-model-invocation` are silently ignored.
      `frontmatter::parse` already returns the full field map -- a
      bounded slice adds the remaining fields plus lenient (warn, don't
      fail) validation surfaced through `harness skill list`.
- [ ] **`disable-model-invocation` + a `/skill:name` explicit-invoke
      command.** Two small, separable pieces: skip a flagged skill in
      the `execute_python` tool description, and a `/skill:name [args]`
      `session_repl` command in the same shape as the existing `/fork`/
      `/export` commands.
- [ ] **Turn-boundary-aware compaction cut points.** `find_compaction_
      fold_count` folds by raw entry count with no role awareness --
      a cut can land immediately after a tool-call entry, separating it
      from its result. Bounded fix: after computing the naive cut index,
      walk to the nearest `User`/`Assistant`-role boundary instead.
- [ ] **Persist compaction `instructions` on `CompactionState`.**
      `compact_now` already receives `instructions` as a parameter but
      never stores it -- one more `Option<String>` field alongside
      `summary`/`compacted_at_ms` closes this.
- [ ] **A `compaction.enabled` settings toggle.** Today the only way to
      suppress automatic compaction is to never configure a real model.
      One more `Option<bool>` field on `Settings` plus one check in
      `maybe_compact`, following the exact precedent the two existing
      compaction fields already set.
- [ ] **An interactive session browser (`prime-agent agents`-style).**
      `session list` already returns the right data (every status
      tier); a bounded slice adds a simple filter/select-to-attach text
      picker on top, no new data source needed.
- [ ] **Resume-by-partial-ID convenience.** Sessions are addressed only
      by full UUID today; a small prefix-match helper ahead of `session
      attach`/`session fork` would need no protocol change.
- [ ] **`daemon shutdown --force`.** Currently unconditional; a `--force`
      flag distinguishing "graceful `WorkerShutdown` to every session"
      from "skip the round trip and just clean up sockets" is a small
      addition to the existing handler.
- [ ] **A `--no-session`/ephemeral-mode flag.** No such flag exists on
      `session new`/`-p` today; would skip `state.json`/`transcript.jsonl`
      persistence, reusing the in-memory `AgentSession::create` path the
      embeddable SDK already established for a non-daemon caller.
- [ ] **Piped-stdin merging for `-p`.** `print_once` never reads stdin
      today; a bounded slice checks `stdin().is_terminal()` and, if
      piped, merges it into `text` before the existing `SessionNew`/
      `SessionPrompt` calls -- no protocol change needed.

Not candidates -- structurally out of scope, same reasoning as
`PARITY.md`'s "Needs a new subsystem" section: prompt-as-a-variable
(context itself exposed as a slice-able Python object, distinct from
`rlm(...)` -- see the RLM Runtime Architecture section above, since
`rlm(...)` itself is no longer in this "not a candidate" bucket, it
shipped) and routing *all* tool use/subagent orchestration through
kernel code (only `rlm(...)` does, `--tools read|mcp` still doesn't).
Each of those two would require the Python-first control environment
this project has deliberately not built (see `PARITY.md`'s "RLM
programming model" and "Recursive subagents" entries for why). Also not
a candidate: restructuring the
worker model so a parent process hosts child runtimes in-process
("one root session tree") instead of today's one-process-per-session
design -- that's a foundational rewrite of the daemon/worker
architecture (crash isolation, `parent_id`-based routing, the whole
socket-per-session model), not a bounded slice, and today's design
already gets the properties that matter (independent lifecycle,
independent crash recovery, daemon-mediated messaging) through a
different, already-working mechanism.
