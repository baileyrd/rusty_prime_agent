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
- [x] **Bounded autonomous mode** (`session autonomous <id> --max-turns N
  [--max-time DURATION] [--quality-gate CMD]`, parity with `prime-agent
  /autonomous`'s turn/token/time budgets and user-defined quality gates).
  Requires an existing `Active` goal (`session goal set`); repeatedly
  sends a `Continue working toward the goal: <text>` `SessionPrompt` until
  `--max-turns` turns have gone out, `--max-time` (if given) elapses, or
  `--quality-gate` (if given, an arbitrary shell command -- `sh -c` on
  Unix, `cmd /C` on Windows) exits zero, at which point the goal is
  marked `Complete`. No token budget: neither `EchoProvider` nor
  `RustyProviderModel`'s `rp-server` round trip surfaces token counts
  today, so only turns and wall-clock time are tracked. The goal is
  re-fetched at the top of every iteration, so an external `pause`/
  `complete`/`clear` from another client is honored as a normal stop
  condition on the next turn rather than raced against. Deliberately a
  one-shot foreground CLI loop (parity with `session prompt` and every
  other subcommand here), not a persistent daemon-side background loop --
  `prime-agent /autonomous` itself runs inside an already-live
  interactive session, which this project doesn't have (see "The
  interactive TUI" below); an always-on daemon-side autonomous loop
  would be the larger, genuinely-new-subsystem version of this and isn't
  attempted.
- [x] **Prompt templates** (`prompt-template list`, `prompt-template
  render <name> [args...]`, `session prompt-template <id> <name>
  [args...]`, parity with `prime-agent`'s `packages/coding-agent/docs/
  prompt-templates.md`). Markdown-plus-YAML-frontmatter snippets
  (`description`, `argument-hint`, then a body) discovered from a global
  directory (`<state_dir>/prompts/*.md`) and a project-local one
  (`.rusty-prime-agent/prompts/*.md`, project-local wins on a name
  collision), invoked by filename minus `.md`. Positional-argument
  substitution matches `prime-agent`'s own grammar: `$1`/`$2`/... for
  individual arguments, `$@`/`$ARGUMENTS` for all of them joined by a
  space, `${@:N}`/`${@:N:L}` for a 1-indexed slice. `prompt-template
  list`/`render` are pure local directory scans -- no daemon connection
  at all, unlike almost every other subcommand -- while `session
  prompt-template` expands and sends the result as an ordinary
  `SessionPrompt`, parity with typing `/name args...` in `prime-agent`'s
  live editor. This is deliberately **not** parity with real
  `prime-agent` "skills" (`packages/coding-agent/docs/skills.md`), which
  are "importable Python packages" wired to the RLM control environment
  -- that half needs real code execution (`tool_runtime::ToolRuntime`'s
  deliberately open seam, Phase 1 backs it with `NoopToolRuntime` only)
  and stays out of scope below; this increment covers only the
  plain-text template half of that surface, which needed none.
- [x] **The Continual Harness** (`session harness (add <id> <prompt|
  memory|skill> <text...>|list <id>|rollback <id> <index>)`, `session
  refine <id>`, parity with `prime-agent`'s Continual Harness paper
  abstraction, `arxiv.org/abs/2605.09998`: "stores supplemental prompts,
  memories, skill descriptions, and reusable subagent specifications as
  durable state that Prime Agent can refine through small,
  evidence-backed updates"). Subagent specifications are left out --
  they're tied to recursive subagents, a separate item below -- so this
  covers prompts/memories/skill descriptions only.
  `HarnessState { notes, history }` on `SessionState.harness`, mutated
  through the worker like `goal`/`schedule`; every successful `Add` or
  `Rollback` appends the resulting `notes` to `history` as a fresh
  entry, so `history.last()` always mirrors the current notes and a
  rollback becomes part of the auditable trail rather than erasing
  anything from it -- parity with "recorded refinement history."
  `session refine <id>` is `/refine`'s "reviews the current trajectory
  and applies a small, evidence-backed update": it fetches the session's
  transcript (the last 20 entries) and current notes, asks the model to
  propose one addition, and records the reply as a new `Memory` note.
  Unlike `prime-agent`'s own hidden analysis call, the review prompt
  goes through as an ordinary, visible `SessionPrompt` turn -- this
  project's `ModelProvider` trait has no side channel for a provider
  call that skips the transcript, and adding one for this alone wasn't
  worth it, so the "evidence" behind a refinement stays inline and
  auditable instead of happening invisibly.
- [x] **Recursive subagents** (`session spawn <parent-id> [--model
  PROVIDER/MODEL] [--name NAME] <task text...>`, `session children
  <id>`, `session message <from-id> <to-id> <text...>`, bounded,
  non-Python parity with `prime-agent`'s `rlm(...)`/`receiver_role=
  "parent"/"child"`, `packages/coding-agent/docs/rlm.md`). The
  underlying mechanism `rlm(...)` actually uses -- "the TypeScript host
  creates a normal child `AgentSession` with an independent context and
  session directory" -- is exactly `session new` plus a recorded
  `parent_id`, already fully within reach; only the Python/IPython
  invocation surface (`rlm(...)` called from kernel code) is out of
  scope. `session spawn` creates the child (inheriting the parent's
  `model` unless `--model` overrides it, parity with "the child
  inherits the parent model... unless the call requests another") and
  enqueues the task text as a near-immediate one-shot schedule rather
  than a blocking `SessionPrompt` -- parity with `rlm(...)` "returns
  immediately after task admission... never waits for or returns the
  child's answer", reusing the daemon's existing background
  schedule-firing loop as the async dispatch this project already has
  instead of inventing a new one. `session message` is the analog of
  `agent_message.send`: only a session's own parent or one of its own
  children is a valid target, validated client-side against `session
  list`'s `parent_id` field (this project's whole trust model is a
  single local caller, so this doesn't need server-side enforcement of
  its own) and delivered as an ordinary, visible `SessionPrompt`.
  Skills/tools/retry-policy inheritance (the rest of that same
  `rlm(...)` sentence) don't apply here -- this project's tool runtime
  is `NoopToolRuntime` and it has no retry-policy concept to inherit.
- [x] **A minimal interactive REPL** (`session repl <id>`), bounded,
  non-Python parity with `prime-agent`'s interactive TUI. Reads lines
  from stdin, sends each as an ordinary `SessionPrompt`, prints the
  reply, until stdin hits EOF or a line is exactly `/exit`/`/quit`;
  replays the session's existing transcript first, so resuming a
  session in the REPL shows its prior turns the same way `session
  attach` would. None of the TUI's own editor/message-queue features
  (file reference, image paste, steering vs. follow-up queuing,
  `/tree`/`/fork`/`/clone`/`/compact`/`/export`/`/share`) -- those stay
  out of scope below; this is the bare loop underneath all of that, the
  same "extract the tractable session-level mechanism, leave the rich
  surface out" move as `session spawn`/prompt templates above.
- [x] **Model/provider catalog listing** (`harness model list`), the
  provider tier of `prime-agent model list`'s catalog browse: which of
  the known providers (`openai`/`anthropic`/`gemini`/`groq`/`ollama`,
  the exact same set `rp_server::write_config` already activates
  `[providers.*]` blocks for) this process's own environment actually
  configures right now, read straight off the same env-var check
  `write_config` itself uses (`rp_server::known_providers`) so this can
  never drift from what a real `session new --model <name>/...` would
  be able to reach. A pure environment-variable check -- no daemon
  connection, no network call. Plain `model list` deliberately stays
  this provider-tier-only view; see the next entry for the real
  per-model catalog.
- [x] **Real per-model catalog** (`harness model list --detailed`).
  Last revision of this file marked this "out of scope" on the
  assumption that real model IDs need "a live query against each
  provider's own API" -- untestable in CI, unattemptable without keys.
  Reading `rusty_provider`'s actual source (`crates/server/src/
  routes.rs`) showed that's not how it works: `rp-server` already
  exposes `GET /v1/models`, sourced from its own `route_aliases()`/
  `configured_providers()`/`priced_models()`, no live per-provider API
  call needed. `--detailed` starts (or reuses) an `rp-server` sidecar
  directly from the CLI (`rp_server::ensure_running`, the same call
  `daemon::Supervisor` makes for `session new --model`) and prints
  `rp_server::ModelCatalogEntry`'s `id`/`owned_by`/`context_length`.
  Fails loudly, not silently, when `rp-server` isn't installed/
  reachable -- covered by a deterministic CI-safe negative test; the
  real success path (`#[ignore]`d, needs `rp-server` + `ollama serve`)
  extends the same infra-gated pattern `tests/ollama_provider.rs` uses.
  Manually verifying that real path against this sandbox's own Ollama
  setup surfaced a genuine hang: this is the first one-shot CLI command
  to call `rp_server::ensure_running` directly (every prior caller was
  either the long-lived daemon, or force-exits via `std::process::exit`)
  -- its reaper task `.await`s the sidecar's `Child::wait()`, which
  never resolves for a deliberately long-lived detached process,
  and `#[rusty_tokio::main]`'s generated `Runtime::drop` waits
  *unboundedly* for that blocking-pool job before letting the process
  exit. Fixed in `main.rs` by exiting explicitly on the success path
  too (the same "blunt but honest" `std::process::exit` convention its
  error path and `WorkerShutdown`/`handle_daemon_shutdown` already use),
  not by changing `rp_server.rs`'s existing, daemon-tested reaping
  behavior -- see `main.rs`'s own doc comment for the full reasoning.
- [x] **`--thinking <level>`** (`session new --thinking low|medium|
  high`), parity with `prime-agent --thinking <level>`. Last revision of
  this file marked this "genuinely out of scope" on the assumption that
  `rp-server`'s wire contract for it was unknown/unverifiable -- reading
  `rusty_provider`'s actual source (`crates/core/src/types.rs`) showed
  `ChatRequest.reasoning: Option<ReasoningConfig>` already exists,
  `ReasoningConfig.effort` taking exactly OpenAI's `"low"`/`"medium"`/
  `"high"` vocabulary. `SessionState.thinking` is threaded through
  `session new`/`WorkerArgs`/`RustyProviderModel` the same way `model`
  is (fixed for a session's whole lifetime, re-supplied from persisted
  state on every worker respawn -- see `worker::WorkerArgs::thinking`'s
  own doc comment for why that's `model`'s pattern and not `goal`'s);
  `RustyProviderModel`'s request body includes `"reasoning":
  {"effort": ...}` only when set. No effect on `EchoProvider` sessions.

## Out of scope for this project's current shape

Architecturally significant `prime-agent` capabilities that would each
require a genuinely new subsystem (a Python control environment) -- not
attempted here, and not silently implied by anything in
`ARCHITECTURE.md`'s "Known gaps" section:

- **The RLM programming model** (persistent IPython kernel,
  `tool_runtime::ToolRuntime`'s one deliberate open seam is exactly this
  boundary, but Phase 1 backs it with `NoopToolRuntime` only; recursive
  subagents' own Python invocation surface, `rlm(...)` itself, is the
  same boundary -- see the medium-effort section above for the
  session-level mechanism underneath it, which is done).
- **Skills, extensions, themes, MCP integrations.** (Prompt templates --
  the plain-text, non-Python half of `prime-agent skills.md`'s surface --
  are done; see the medium-effort section above. Real "skills" stay out
  of scope: they're "importable Python packages" wired to the RLM
  control environment, same boundary as the RLM programming model item
  above.)
- **`/heartbeat` and `rlm_heartbeat`** -- the TUI-command and RLM-function
  triggers for the same "re-enter a session periodically" mechanism
  `prime-agent schedule`/`session schedule` already covers server-side
  (see the medium-effort section above); these two are just alternate
  entry points into it that need the TUI or the RLM programming model,
  both themselves out of scope below. (Scheduling, persistent goals, and
  bounded autonomous mode -- `prime-agent schedule`/`--goal`/`/goal`/
  `/autonomous` -- are all done; see the medium-effort section above.)
- **The interactive TUI**'s rich editor/message-queue features (file
  reference, image paste, steering vs. follow-up queuing, `/tree`,
  `/fork`, `/clone`, `/compact`, `/export`, `/share`). (The bare
  read-a-line/send-a-prompt loop underneath the TUI itself is done --
  `session repl`, see the medium-effort section above.)

## Process

Each increment above gets implemented, tested (a real integration test
under `tests/`, not just a unit test), and checked off in place, with a
short note if the implementation diverged from the plan. This file is
updated in the same commit as the increment it tracks, not after the
fact.
