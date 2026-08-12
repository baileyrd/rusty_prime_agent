# Parity tracker: `rusty_prime_agent` vs `prime-agent`

`rusty_prime_agent` is a small Rust harness that deliberately mirrors one
slice of [`PrimeIntellect-ai/prime-agent`](https://github.com/PrimeIntellect-ai/prime-agent)'s
*operational* architecture -- a daemon supervisor, per-session worker
processes, crash recovery, generation-numbered events -- without
attempting to reimplement `prime-agent` itself. `prime-agent` is a mature,
much larger TypeScript project: a persistent-IPython RLM control
environment, a Continual Harness, recursive subagents, skills/extensions,
scheduling, goals, autonomous mode, and a full TUI, wired to real model
providers. This document tracks that gap honestly and records what's
tractable to close incrementally versus what's out of scope for this
project's current shape.

See `ARCHITECTURE.md` for how this project's own pieces fit together, and
`packages/coding-agent/docs/architecture.md` in the `prime-agent` repo for
the reference design this project's daemon/worker split is modeled on.

## Already mirrored (operational architecture)

- **Daemon supervisor + per-session worker processes**, matching
  `prime-agent`'s supervisor/worker split: the supervisor owns discovery,
  routing, and worker health; each worker owns one session's state and
  transcript. (`prime-agent`: `daemon.md`; this project: `daemon`/`worker`
  modules.)
- **Crash recovery from disk, not from a still-running process's memory**:
  both a supervisor restart (adopts still-live workers, no respawn) and a
  worker crash (full transcript replay, generation bump, recovery marker)
  recover from the persisted `state.json`/`transcript.jsonl`, the same
  "durable recovery baseline" model `prime-agent`'s daemon architecture
  uses.
- **Generation-numbered events**: every new worker process taking
  ownership of a session bumps a generation counter, mirrored from
  `prime-agent`'s own generation-aware event stream (see the sequence
  diagram in `packages/coding-agent/docs/architecture.md`).
- **JSONL-framed IPC over local sockets**, one tier public (CLI <->
  supervisor) and one tier private (supervisor <-> worker) -- the same
  two-tier shape `prime-agent`'s `AgentConnection` <-> supervisor <->
  worker split uses, simplified to line-delimited JSON instead of a
  versioned binary/JSON-RPC protocol.

## Tractable near-term increments

Concrete, scoped features that fit the existing daemon/worker/protocol
shape without requiring a real model backend or a Python control
environment:

- [x] **`session stop <id>`** -- parity with `prime-agent stop <agent>`:
  gracefully shut down one session's worker without shutting down the
  whole daemon (`daemon shutdown` already did this for every session at
  once; there was no single-session equivalent). Idempotent against an
  already-stopped or already-crashed session.
- [x] **`session rename <id> <name>`** -- parity with `prime-agent rename
  <agent> <name>`. Routed through the owning worker (like `session
  prompt`), not written to `state.json` directly by the daemon, since the
  running worker is `state`'s one owner and would otherwise clobber a
  direct write on its next periodic `write_state`.
- [x] **Richer `session list` output** -- `prime-agent`'s `agents`/`list
  [--all]` surface worker pid and generation per agent; `SessionSummary`
  now carries both (it already had `status`, which doubles as the
  idle/running/crashed signal `--all` shows). `daemon status`'s own
  `sessions_active` count was left as-is -- it's a daemon-wide aggregate,
  not a per-session listing, so there was nothing to enrich there.
- [ ] **`--print`/`-p` one-shot mode** -- `prime-agent -p "..."` prints a
  response and exits instead of entering the (nonexistent, for this
  project) interactive TUI. This project's CLI is already one-shot for
  everything except `session attach`, so this is mostly a naming/doc
  question once `session prompt` semantics are confirmed equivalent.

## Medium-effort, real gaps

Would need a new subsystem, but one that composes with the existing
daemon/worker split rather than requiring the Python control environment:

- **A real `ModelProvider` backend.** `provider::EchoProvider` is a
  deliberate Phase 1 stand-in (Non-Goal: "stub with a fake provider that
  echoes turns"); `prime-agent` streams real model responses through
  `--provider`/`--model`/`--api-key`. The `ModelProvider` trait boundary
  already exists for this; a real backend is an HTTP-calling
  implementation behind it, not an architecture change.
- [x] **`--mode json`** -- a leading global flag (`harness --mode json
  session list`, parity with `prime-agent --mode json`) that switches
  every public subcommand's rendering from this project's own
  human-readable text to raw `Response`/`SessionEvent` JSON lines. Reuses
  this project's own wire types as the JSON vocabulary rather than
  modeling `prime-agent`'s much richer `AgentSessionEvent` schema
  (`agent_start`/`turn_start`/`message_*`/tool-execution events), which
  assumes a streaming model and tool-execution pipeline this project
  doesn't have -- see `cli::OutputMode`'s doc comment.

## Out of scope for this project's current shape

Architecturally significant `prime-agent` capabilities that would each
require a genuinely new subsystem (a Python control environment, a
scheduler, cross-agent messaging, a durable-state refinement engine) --
not attempted here, and not silently implied by anything in
`ARCHITECTURE.md`'s "Known gaps" section:

- **The RLM programming model** (persistent IPython kernel,
  `tool_runtime::ToolRuntime`'s one deliberate open seam is exactly this
  boundary, but Phase 1 backs it with `NoopToolRuntime` only).
- **Recursive subagents** (`rlm(...)`, agent-to-agent messaging,
  `receiver_role="parent"/"child"`).
- **The Continual Harness** (`/refine`, durable supplemental
  prompts/memories/skill descriptions with rollback).
- **Skills, extensions, prompt templates, themes, MCP integrations.**
- **Scheduling, heartbeats, persistent goals, bounded autonomous mode**
  (`prime-agent schedule`, `/heartbeat`, `--goal`, `--autonomous*`).
- **The interactive TUI** and its editor/message-queue features (file
  reference, image paste, steering vs. follow-up queuing, `/tree`,
  `/fork`, `/clone`, `/compact`, `/export`, `/share`).
- **Multi-provider auth, model catalog, thinking-level controls.**

## Process

Each increment above gets implemented, tested (a real integration test
under `tests/`, not just a unit test), and checked off in place, with a
short note if the implementation diverged from the plan. This file is
updated in the same commit as the increment it tracks, not after the
fact.
