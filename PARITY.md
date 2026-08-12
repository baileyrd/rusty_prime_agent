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
- [x] **`--print`/`-p` one-shot mode** -- `harness -p "..."` (parity with
  `prime-agent -p`). Turned out to need one real behavior, not just a
  naming alias: unlike every other subcommand, it transparently starts a
  daemon if none is running (`client::ensure_daemon_started`, factored
  out of `daemon_start`) and creates its own unnamed session, then prints
  only the reply text -- no session id, no daemon-startup noise, no
  `[seq] role:` prefix.

## Medium-effort, real gaps

Would need a new subsystem, but one that composes with the existing
daemon/worker split rather than requiring the Python control environment:

- [x] **A real `ModelProvider` backend.** `provider::EchoProvider` was a
  deliberate Phase 1 stand-in (Non-Goal: "stub with a fake provider that
  echoes turns"); `prime-agent` streams real model responses through
  `--provider`/`--model`/`--api-key`. `provider::OllamaProvider`
  (`RUSTY_PRIME_AGENT_PROVIDER=ollama`, `EchoProvider` stays the default)
  is a real, network-calling backend built on the existing
  `ModelProvider` trait boundary rather than a redesign of it -- routed
  through [`rusty_provider`](https://github.com/baileyrd/rusty_provider)'s
  `rp-server` (an OpenAI-compatible provider router this org already
  maintains, per the "check a sibling repo before writing anything from
  scratch" rule) rather than calling Ollama's own API directly, so the
  same path composes with any other backend `rusty_provider` already
  supports (OpenAI, Anthropic, Gemini, Groq, ...), not just Ollama.
  `rp-server` runs as a supervisor-owned sidecar process (`rp_server.rs`,
  spawned once at daemon startup, torn down on `daemon shutdown`) rather
  than a library dependency -- it's built on real `tokio`, not this
  project's `rusty_tokio`, and two async runtimes in one process is a
  worse boundary than an HTTP call to a process this project already
  manages the lifecycle of. `http_client.rs` is a hand-rolled, ~100-line
  HTTP/1.1 client (connect, one request, read to EOF) rather than a
  `reqwest`/`hyper` dependency, matching this project's narrow dependency
  floor -- every call it makes is a single round trip to a loopback peer
  this project itself spawned. Verified against a real, locally-pulled
  model (`tests/ollama_provider.rs`, `#[ignore]`d since it needs
  `ollama serve` + a pulled model + `rp-server` on `PATH`, none of which
  CI has reason to provide) -- confirmed a live `qwen2.5:0.5b` completion
  round-tripped through the full stack. That run also surfaced two
  latent timeout bugs the `EchoProvider`-only test suite could never
  have caught: `client.rs`'s 5s `RESPONSE_TIMEOUT` and `http_client.rs`'s
  30s `REQUEST_TIMEOUT` were both tuned against an instant fake reply and
  too tight for real (even small, CPU-only) model inference (~29s
  observed) -- `SessionPrompt`'s own response wait now uses a separate,
  much larger `PROMPT_RESPONSE_TIMEOUT` (120s) than every other request.
- [x] **Multi-provider selection, `session new --model provider/model` /
  `-p --model provider/model`** -- parity with `prime-agent --model
  provider/id`. Originally shipped as a single global
  `RUSTY_PRIME_AGENT_PROVIDER=ollama` on/off switch; generalized to a
  per-session `--model` flag once it was clear the same `rp-server`
  sidecar already routes to whichever backend the `"provider/model"`
  string names (`rp_server::write_config` now activates a
  `[providers.*]` block for every provider this process has a real key
  for -- `OPENAI_API_KEY`/`ANTHROPIC_API_KEY`/`GEMINI_API_KEY`/
  `GROQ_API_KEY` -- plus `[providers.ollama]` unconditionally). `model`
  is recorded in `SessionState` so a resume/recover respawn reconstructs
  the same backend rather than re-resolving it from whatever the daemon's
  current environment happens to say. `RUSTY_PRIME_AGENT_MODEL` remains
  as a server-side default for callers that don't pass `--model`
  explicitly. `provider::RustyProviderModel` (renamed from
  `OllamaProvider`, since it addresses any configured backend, not just
  Ollama) is the one `ModelProvider` impl this covers.
  Environment variables: `RUSTY_PRIME_AGENT_MODEL` (default `--model`,
  optional), `RUSTY_PRIME_AGENT_RP_SERVER_BIN` (default `rp-server`, i.e.
  on `PATH`), `RUSTY_PRIME_AGENT_OLLAMA_BASE_URL` (default
  `http://127.0.0.1:11434/v1`), plus each provider's own real
  `*_API_KEY` to activate it.
- [x] **`--mode json`** -- a leading global flag (`harness --mode json
  session list`, parity with `prime-agent --mode json`) that switches
  every public subcommand's rendering from this project's own
  human-readable text to raw `Response`/`SessionEvent` JSON lines. Reuses
  this project's own wire types as the JSON vocabulary rather than
  modeling `prime-agent`'s much richer `AgentSessionEvent` schema
  (`agent_start`/`turn_start`/`message_*`/tool-execution events), which
  assumes a streaming model and tool-execution pipeline this project
  doesn't have -- see `cli::OutputMode`'s doc comment.
- [x] **Scheduling** (`session schedule add|list|cancel`, parity with
  `prime-agent schedule <list|add|cancel>`). A one-shot (`--at
  TIME`) or recurring (`--every DURATION`) prompt the daemon itself
  injects into a session later, with no client attached -- persisted
  per-session (`schedule.rs`, `sessions/<id>/schedules.json`) so it
  survives a daemon restart, fired by a background poll loop
  (`daemon::SCHEDULE_POLL_INTERVAL`, 5s) that turns a due entry into an
  ordinary internal `SessionPrompt`, indistinguishable from a
  client-issued one from the worker's point of view. `--at`/`--every`
  take a short duration string (`30s`/`5m`/`2h`/`1d`) or, for `--at`, a
  raw Unix-epoch-milliseconds integer -- not a full RFC 3339/ISO 8601
  parser, matching this project's narrow dependency floor (no `chrono`
  pulled in for this). A recurring entry that's overdue by more than one
  interval (e.g. the daemon was down a while) skips forward to the next
  future fire time rather than firing a burst of catch-up prompts.
- [x] **Persistent goals** (`session new --goal <text>`, `session goal
  (set <text...>|show|pause|resume|complete|clear) <id>`, parity with
  `prime-agent --goal`/`/goal`). A durable `GoalState { text, status:
  Active|Paused|Completed, created_at_ms, updated_at_ms }` on
  `SessionState.goal`, mutated through the worker the same way `session
  rename` is (never written directly by the daemon, to avoid racing the
  worker's own `state.json` writes) except for the read-only `goal show`,
  which the daemon answers directly from disk like other catalog-style
  reads. `pause`/`resume`/`complete` are deliberate no-ops (not errors)
  when there is no current goal to transition; `set` always replaces
  whatever was there, `Active`, even over a `Completed` one. This is
  purely the durable state a future bounded-autonomous-continuation
  policy would read -- it does not itself make the agent act on the goal;
  see "Heartbeats, bounded autonomous mode" below for that remaining
  piece.

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
- **Heartbeats, bounded autonomous mode** (`/heartbeat`, `--autonomous*`).
  (Scheduling itself -- `prime-agent schedule` -- and persistent goals --
  `prime-agent --goal`/`/goal` -- are both done; see the medium-effort
  section above.)
- **The interactive TUI** and its editor/message-queue features (file
  reference, image paste, steering vs. follow-up queuing, `/tree`,
  `/fork`, `/clone`, `/compact`, `/export`, `/share`).
- **Model catalog listing, thinking-level controls.** (Multi-provider
  *selection* itself -- `--model provider/model` -- is done; see the
  medium-effort section above. `prime-agent model list`'s catalog browse
  and `--thinking <level>` are not.)

## Process

Each increment above gets implemented, tested (a real integration test
under `tests/`, not just a unit test), and checked off in place, with a
short note if the implementation diverged from the plan. This file is
updated in the same commit as the increment it tracks, not after the
fact.
