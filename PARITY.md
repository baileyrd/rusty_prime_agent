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
tractable to close incrementally versus what's not yet implemented in
this project's current shape.

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
  -- `tool_runtime::ToolRuntime`'s code-execution boundary is real now
  (see the RLM programming model entry below), but actual Python-package
  "skills" on top of it are a separate, larger surface still out of
  scope; this increment covers only the plain-text template half of that
  surface, which needed none.
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
  `rlm(...)` sentence) don't apply here -- `session spawn` doesn't thread
  a `--runtime`/`--tools` flag of its own (a child session gets
  `NoopToolRuntime`/no tools regardless of its parent's, unlike `model`,
  which it does inherit), and there's no retry-policy concept to inherit
  either way.
- [x] **A minimal interactive REPL** (`session repl <id>`), bounded,
  non-Python parity with `prime-agent`'s interactive TUI. Reads lines
  from stdin, sends each as an ordinary `SessionPrompt`, prints the
  reply, until stdin hits EOF or a line is exactly `/exit`/`/quit`;
  replays the session's existing transcript first, so resuming a
  session in the REPL shows its prior turns the same way `session
  attach` would. None of the TUI's own editor/message-queue features
  (file reference, image paste, steering vs. follow-up queuing,
  `/tree`/`/fork`/`/clone`/`/compact`/`/export`/`/share`) -- those stay
  unimplemented below; this is the bare loop underneath all of that, the
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
  Last revision of this file marked this "not implemented" on the
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
- **Upstream fix: `rusty_tokio` epoll reactor busy-spin.** Manually
  verifying the real success path above (a `session prompt` against a
  real, slow model backend) surfaced a second, unrelated bug one layer
  down: the CLI process was burning most of its wall-clock wait time as
  actual CPU, not blocked I/O. Isolated with a minimal repro against a
  real slow HTTP response and confirmed with `strace -c`: `rusty_tokio`'s
  Linux epoll reactor (`Reactor::register`) registered every fd
  level-triggered (no `EPOLLET`), and a connected socket is almost
  always writable, so `epoll_wait` returned immediately on *every* call
  for as long as any fd sat open -- ~864k calls in a 12s wait that
  should have needed exactly one. `kqueue.rs` (macOS/BSD) already got
  this right (`EV_CLEAR`); `epoll.rs` was just missing the equivalent
  flag, which the crate's own retry-until-`WouldBlock` I/O design
  already assumed. Fixed and merged upstream
  (`baileyrd/rusty_tokio#265`); this repo's own `Cargo.toml` pin bumped
  to pick it up. Not this project's own code to fix -- flagged here
  since it's the kind of cross-repo dependency issue this document
  otherwise wouldn't have a home for.
- [x] **`--thinking <level>`** (`session new --thinking low|medium|
  high`), parity with `prime-agent --thinking <level>`. Last revision of
  this file marked this "genuinely not implemented" on the assumption that
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
- [x] **Real tool-calling loop** (`session new --tools read`, built-in
  `read_file`/`list_dir` tools). Last revision of this file lumped this
  in with the RLM programming model's IPython-kernel boundary as
  Python-bound and not implemented -- reading `rusty_provider`'s actual
  source (`crates/core/src/types.rs`) showed that conflated two
  different things: `tool_runtime::ToolRuntime` really is that boundary
  (still `NoopToolRuntime` as of this entry, untouched by it -- see the
  RLM programming model entry further down for where that later changed),
  but `rp-server`'s `/v1/chat/completions` already speaks full
  OpenAI-style tool-calling (`ChatRequest.tools`/`tool_choice`,
  `ChatMessage.tool_calls`, `Role::Tool`) independently of any kernel --
  our own `ModelProvider` just never asked for any of it. `provider::
  ModelProvider::respond` now takes the full turn history plus an
  offered tool list (`ChatTurn`/`TurnRole`/`ToolDef`, hand-rolled to
  `rp-server`'s wire shape, not a `rp_core` dependency) and returns
  either `ProviderReply::Text` or `ProviderReply::ToolCalls`;
  `AgentSession::prompt` loops on the latter (execute each call via
  `tools::execute`, append a `Role::Tool` result entry, ask again),
  capped at 8 rounds to bound a runaway loop. `EchoProvider` ignores the
  offered tools entirely and never emits `ToolCalls`, so every existing
  session (no `--tools` flag) is completely unaffected -- proven by the
  full existing suite passing unchanged. Read-only first: `read_file`/
  `list_dir`, plain `std::fs`, no path sandboxing (same single-local-user
  trust model `session_autonomous --quality-gate`'s unsandboxed shell
  execution already established) -- `--tools shell`/write-capable tools
  are a natural v2 extension of the same flag, not built now. Manually
  verified against this sandbox's real Ollama setup: the plumbing
  round-trips correctly when a model emits a real, structured
  `tool_calls` response (covered by unit tests against synthetic
  `rp-server`-shaped JSON); the small local test model (`qwen2.5:0.5b`)
  itself proved an unreliable tool-caller in practice (it sometimes
  narrates a tool call as prose instead of emitting one), a known
  small-model limitation, not a plumbing gap -- `tests/ollama_provider.rs`'s
  own real-tool-call test is written to tolerate that.
- [x] **MCP integration** (`session new --tools mcp`), parity with
  `prime-agent`'s MCP support. Last revision of this file lumped this in
  with "Skills, extensions, themes" as not implemented -- `rp-server`
  already ships its own built-in MCP gateway (`crates/mcp/`): its native
  tools (`chat_completion`/`list_models`/`embeddings`) merged with
  every tool proxied from a configured `[[mcp.upstreams]]` entry
  (namespaced `"{upstream}/{tool}"`), all served over one `/mcp`
  endpoint -- so this project only ever needs a *minimal client* against
  that one endpoint, not a from-scratch multi-upstream gateway. `[mcp]
  enabled = true` is now emitted unconditionally by `rp_server::
  write_config` (harmless with no upstreams configured, same reasoning
  `[providers.ollama]` already gets there).
  New `mcp_client::McpClient`: `initialize`/`notifications/initialized`
  + `tools/list` + `tools/call` against `rp-server`'s `/mcp`, hand-rolled
  to match its actual wire behavior -- **spiked first** by probing a
  real sidecar directly (`curl`), the same "reproduce first" discipline
  this project used for its original AF_UNIX bug, before writing any
  client code. Two things the spike caught that weren't obvious from the
  docs alone: every response is SSE-framed (`Content-Type: text/
  event-stream`) *even for a single non-streaming call* -- `rp-server`
  answers `406 Not Acceptable` unless `Accept` includes both
  `application/json` and `text/event-stream` -- and those SSE responses
  are `Transfer-Encoding: chunked`, which this project's hand-rolled
  `http_client` had never needed to decode before (`get`/`post_json`'s
  peers had only ever answered with `Content-Length` framing);
  `http_client::decode_chunked` is the fix, exercised only by MCP calls,
  a no-op for every existing caller. The server closes the connection
  after answering one request (confirmed by direct probe), so no change
  to this client's existing "one request per connection, read to EOF"
  design was needed beyond that. `--tools mcp` is a separate, mutually
  exclusive value from `--tools read` in this pass (`AgentSession`
  dispatches tool-set discovery and tool execution to either
  `tools::execute` or `McpClient` based on `state.tools`, not both at
  once) -- merging multiple tool sources into one offered set is a
  natural v3 extension of the same flag, not built now. Needs a running
  `rp-server` sidecar even for an `EchoProvider` session (the tools live
  on `rp-server` itself, independent of chat completions), so `session
  new --tools mcp` triggers `rp_server::ensure_running` the same way
  `--model` already does, and fails loudly (not silently) when it can't
  come up. Manually verified end to end against this sandbox's real
  Ollama + `rp-server` setup: a real model actually called `rp-server`'s
  native `list_models` tool through the gateway and incorporated the
  (real, if empty in this minimal test config) result into its reply --
  stronger confirmation than the built-in-tools entry above got, since
  this small test model reliably emits a real structured tool call for
  this specific tool/prompt pair, unlike its unreliable behavior with
  `read_file`.
- [x] **The RLM programming model's IPython-kernel boundary** (`session
  new --runtime ipython`, `execute_python` tool), parity with
  `prime-agent`'s persistent-IPython RLM control environment. Every prior
  revision of this file marked this "genuinely not implemented" as
  architecturally Python-bound; that held for the actual code-execution
  half, but not for the reason assumed. `tool_runtime::ToolRuntime` was
  always the right boundary (still true) -- what changed is that backing
  it with something real turned out tractable within this project's own
  shape, not a TypeScript-scale undertaking.

  The original plan for this increment named the `zeromq` crate (pure-Rust
  ZMTP) as an explicit, justified dependency exception. Reading its actual
  `Cargo.toml` (`cargo tree` against a throwaway scratch crate) found it
  depends directly on real `tokio` unconditionally (`default =
  ["tokio-runtime", ...]`; its `async-std-runtime` feature swaps to a
  *different* foreign runtime, not a fix) -- exactly the "two async
  runtimes competing in one process" anti-pattern `rp_server`'s own doc
  comment explains why `rp-server` runs as a separate OS process rather
  than being linked in as a library. Rather than accept that (or bridge it
  across a second OS thread running its own runtime), the wire protocol
  itself was hand-rolled directly on `rusty_tokio::io::TcpStream`
  (`zmtp.rs`): the ZMTP 3.0 NULL-mechanism greeting, `READY` command
  exchange, and DEALER/SUB multipart framing, confirmed byte-exact against
  a real, running `ipykernel` via raw-socket probes (no pyzmq, no libzmq)
  before any Rust code was written -- the same "reproduce first"
  discipline this project used for the original AF_UNIX bug and the MCP
  gateway spike. This is no bigger an undertaking than `http_client`'s own
  hand-rolled HTTP/1.1 client, and more consistent with this project's
  established "hand-roll wire protocols over pulling in a dependency"
  posture than the plan's original `zeromq` assumption was.
  Jupyter's message-signing scheme (HMAC-SHA256 over each message's
  header/parent_header/metadata/content frames) needed a second thing this
  project had never needed before: a hash function. `sha256.rs`
  hand-rolls SHA-256 and HMAC-SHA256 directly from FIPS 180-4/RFC 2104
  rather than adding a `sha2`/`hmac` crate exception alongside the
  now-avoided `zeromq` one -- unlike ZMTP's or HTTP's own framing, SHA-256
  is a small, exactly-specified algorithm with no ambiguity to get subtly
  wrong, and the implementation is pinned against the official FIPS 180-4
  and RFC 4231 test vectors, not just self-consistency. The HMAC key
  itself is generated locally (timestamp + pid, mixed through `sha256`,
  the same non-cryptographic-but-adequate approach `session::
  new_session_id` already established) rather than needing a `rand`
  dependency -- adequate because the real security boundary is the same
  one a genuine Jupyter installation relies on: the connection file never
  leaves this session's own, already-private state directory.

  `ipython_runtime::IpythonKernelRuntime` backs `ToolRuntime` for real:
  `start` spawns `python3 -m ipykernel_launcher -f <connection file>` (five
  ports picked the same way `rp_server::pick_free_port` picks `rp-server`'s
  one), opens `shell` (DEALER) and `iopub` (SUB, subscribed to
  everything), and completes a `kernel_info_request`/`reply` handshake;
  `execute` sends `execute_request` and reads iopub until `status: idle`,
  capturing `stream` (stdout/stderr), `execute_result`, and `error`
  content into `ExecutionOutcome`. Direct testing against a real kernel
  caught a real bug worth recording: an early version broke its iopub loop
  on the *first* `status: idle` it saw, but a freshly-booted kernel emits
  its own unsolicited `busy`/`idle` pair during startup, unrelated to any
  request -- the fix checks `parent_header.msg_id` against the
  `execute_request`'s own `msg_id`, the same correlation a real Jupyter
  client performs, to tell a request's own iopub traffic apart from
  everything else on the same broadcast socket. Only `shell`+`iopub` are
  opened; `stdin` (kernel-side `input()`), `control` (interrupt/
  `shutdown_request`), and `heartbeat` aren't implemented for this pass, so
  `shutdown` tears the kernel down with a plain process kill rather than a
  graceful control-channel request. The kernel subprocess is deliberately
  left un-detached (unlike every other spawned process this project
  manages): it's meant to live and die with the one worker/session that
  owns it, which also means `ipykernel`'s own parent-poller kills it for
  free if the worker crashes without a clean `shutdown`.

  Reaching the kernel from a prompt reuses the existing tool-calling loop
  (Increment 3) rather than inventing a second turn-loop mechanism:
  `session new --runtime ipython` offers an `execute_python` tool
  (independent of, and combinable with, `--tools read|mcp`) whose calls
  are routed to `self.tool_runtime.execute(code)` instead of
  `tools::execute`/the MCP client. A Python-level exception (e.g. `raise
  ValueError(...)`) comes back as an ordinary tool result the model can
  see and recover from, not a `HarnessError` -- only a genuine plumbing
  failure (a dropped connection, a timed-out handshake) propagates as one.
  Real end-to-end coverage (spawn, handshake, `execute_request` round
  trip, state persisting across calls within one kernel, a Python
  exception surfacing correctly, clean shutdown) lives as an `#[ignore]`d
  test directly in `ipython_runtime.rs`'s own test module rather than
  `tests/`, since this project's binary has no `[lib]` target for an
  integration test to link a Rust-level unit test against -- run
  explicitly against a real local `ipykernel` install (`pip install
  ipykernel`), the same infra-gated pattern `tests/ollama_provider.rs`
  uses. CI-safe coverage (`tests/ipython_runtime.rs`) proves the flag
  reaches all the way from `session new` through the daemon to the
  worker's `ToolRuntime` selection without needing a real kernel, by
  pointing `RUSTY_PRIME_AGENT_IPYTHON_BIN` at a binary name that can't
  exist.

  Explicitly still not implemented: interrupt/cancel (needs `control`),
  kernel restart-on-crash, rich display data (DataFrames/plots/widgets --
  `execute_result`'s `data` only ever reads `text/plain` here), and
  multi-kernel pooling beyond one kernel per session. Real `prime-agent`
  "skills" (importable Python packages, `packages/coding-agent/docs/
  skills.md`) and `/heartbeat`/`rlm_heartbeat` (its manual re-entry
  triggers) both turned out to be tractable on top of this same kernel
  boundary -- see the next two entries.
- [x] **Skills packaging** (real, importable Python packages for
  `session new --runtime ipython`), parity with `prime-agent skills.md`.
  Every prior revision of this file bundled this in with "extensions,
  themes" as an undefined, out-of-scope surface -- that held only while
  the underlying code-execution boundary (the entry above) was still a
  stub; once it was real, this became tractable within this project's
  existing shape, the same story as MCP integration and the tool-calling
  loop before it. Prompt templates already cover `skills.md`'s
  plain-text half (`prompt_template.rs`'s own doc comment says so); this
  closes the Python-package half.

  A skill is a directory under a new `paths::global_skills_dir`
  (`<state_dir>/skills/`): a `SKILL.md` (`description` frontmatter,
  model-facing) alongside a real Python package (`__init__.py`, plus
  whatever else the package needs). `SKILL.md`'s frontmatter is parsed
  by a small shared `frontmatter.rs`, factored out of
  `prompt_template.rs::parse` rather than duplicated (both modules only
  ever read a couple of flat string keys, so one hand-rolled `---\nkey:
  value\n---\n<body>` parser serves both). `skills::discover` never
  inspects the Python files themselves -- a broken `__init__.py`
  surfaces as an ordinary `ImportError` the model sees and can recover
  from when it actually tries `import <name>`, the same "let the callee
  reject malformed input" philosophy `tools::execute` already
  established, not something worth validating twice.

  Global tier only, deliberately, unlike `prompt_template::discover`'s
  global-plus-project-local pair: skill *loading* has to run inside the
  worker process (it needs a live kernel connection, via
  `tool_runtime::ToolRuntime::execute`), which runs with the daemon's
  own cwd, not the session-creation caller's -- unlike
  `prompt_template::discover`, whose callers are always client-side,
  where the real cwd is available. A correct project-local tier would
  need the CLI's cwd threaded through `Request::SessionNew`/`WorkerArgs`
  on every worker respawn (`thinking`'s "always supplied" pattern, not
  `goal`'s "New-only" one) -- real, but separate scope, not attempted
  here rather than silently half-done.

  `worker::run` installs skills once, right after `tool_runtime.start()`
  succeeds and before session construction: when `--runtime ipython` and
  at least one skill is discovered, one `execute_request`
  (`sys.path.insert(0, <skills dir>)`) puts every installed skill's
  parent directory on the kernel's own `sys.path`, so `import <name>`
  resolves for the rest of that kernel's life -- zero skills installed
  means zero extra round trips. `session::enabled_tool_defs` appends
  each skill's name and description to the `execute_python` tool's own
  description (recomputed every `prompt` call, so a skill installed or
  removed between prompts is picked up without a session restart),
  telling the model what it can `import` without a human having to say
  so. `harness skill list` (parity with `prompt-template list`'s own
  shape: pure local scan, no daemon) reports what's installed.

  Real end-to-end coverage lives as an `#[ignore]`d test in
  `ipython_runtime.rs`'s own test module, alongside the RLM entry's real-
  kernel test: a genuine two-file Python package on disk, `sys.path`-
  inserted and `import`ed inside a real kernel, its function actually
  called and its return value checked -- proof the mechanism works
  independent of whether a real model ever decides to `import` anything
  on its own (the same small-model tool-call reliability caveat this
  project's test suite already documents elsewhere). CI-safe coverage
  (`tests/skills.rs`) proves discovery/listing/CLI wiring without a real
  kernel.

  Explicitly still not implemented: the project-local tier above, a `skill
  install`/packaging-and-distribution command (drop-a-directory-in stays
  manual, same as prompt templates always have), skill versioning or
  dependency management, and skill-provided *tools* of their own -- a
  skill is code the model imports and calls itself inside
  `execute_python`, not a second tool-generation surface alongside
  `--tools read|mcp`.
- [x] **`/heartbeat` and `rlm_heartbeat()`** -- the REPL-command and
  RLM-function manual entry points into the same "re-enter a session
  periodically" mechanism `session schedule` already covers server-side,
  parity with `prime-agent`'s two triggers. Every prior revision of this
  file cut both together, needing either "the TUI" (this project's
  `session repl` is the bare loop underneath the TUI, not the TUI itself,
  but plenty for a REPL command) or a real kernel to call something *from*
  (real now). Both send the exact same continuation prompt
  `session_autonomous` already sends each turn (`"Continue working toward
  the goal: <text>"`), requiring an `Active` goal -- the same precondition
  `session_autonomous` itself has.

  A real hazard surfaced by reading `schedule.rs` directly before
  assuming anything, not just recalling it from memory: its own module
  doc comment says it's "owned entirely by the daemon supervisor" --
  `write_all` is a plain, unlocked `std::fs::write`, and `take_due` is
  read-modify-write. A second process calling `schedule::add` on the same
  `schedules.json` concurrently with the daemon's own background
  `fire_due_schedules` poll is a real lost-update race, not a theoretical
  one -- which rules out `rlm_heartbeat()` (running *inside the worker
  process*, via `tool_runtime::ToolRuntime::execute`) touching that file
  directly. Fixed by routing through the daemon's existing public
  `Request::ScheduleAdd` handler instead: nothing stops a worker process
  from opening an ordinary client connection to its own `daemon.sock`
  (`transport::connect`, exactly what every `client.rs` function already
  does) -- `daemon::handle_schedule_add` already works for any
  `session_id`, including a worker scheduling on behalf of its own
  session, so no daemon-side code changed at all.

  `rlm_heartbeat()` also can't just call `self.prompt()` directly: it's
  invoked from inside `execute_python_tool_call`, itself inside the
  *outer* `prompt()` call's own tool-loop -- reentrant recursion there
  would append a second prompt's turns before the outer call's own
  pending tool-result turn, an incoherent transcript ordering, not merely
  a borrow-checker inconvenience. `worker::bootstrap_kernel` (renamed
  from `install_skills`, generalized to always run when `--runtime
  ipython` rather than only when skills exist) defines `rlm_heartbeat()`
  in the kernel's own globals -- calling it prints a fixed marker
  (`session::HEARTBEAT_MARKER`) `execute_python_tool_call` watches every
  call's stdout for, strips out of what the model sees, and dispatches to
  a new `trigger_heartbeat` method (no `Active` goal: explains, schedules
  nothing; otherwise: one `Request::ScheduleAdd` round trip,
  `ScheduleKind::Once { at_ms: now_ms() }`, the same near-immediate
  one-shot pattern `client::session_spawn` already established) --
  sequencing it as a distinct, later top-level turn once the daemon's
  next poll (`SCHEDULE_POLL_INTERVAL`) picks it up, instead of racing the
  in-flight call.

  `/heartbeat` in `session_repl` needs none of that indirection -- it's a
  fresh top-level REPL action, not nested inside anything, so it fetches
  the goal via the already-existing `fetch_goal` helper (shared with
  `goal_show`/`session_autonomous`) and calls the exact same
  `send_prompt` every other REPL line already uses, immediately, no
  scheduling latency.

  Explicitly not implemented: rate-limiting/deduplicating rapid repeated
  `rlm_heartbeat()` calls (same "no sandboxing for a single local user"
  trust model already accepted elsewhere), arguments to either trigger
  (both parameterless, matching `prime-agent`'s own simplest form), and
  any change to `session_autonomous`'s own bounded loop -- these are two
  more manual entry points into the same mechanism, not a third
  continuation policy.

## Identified gaps, not yet started

Surfaced by a documentation-level review of `prime-agent`'s real docs
(`packages/coding-agent/docs/*.md`, 35 files) rather than source
reading -- unlike "Needs a new subsystem" below, each of these fits this
project's existing shape without a new subsystem. None has been scoped
or implemented yet.

- **Automatic context compaction.** `prime-agent`'s `compaction.md`:
  triggers when `contextTokens > contextWindow - reserveTokens`
  (`reserveTokens` defaults to 16,384 tokens, `keepRecentTokens` to
  20,000), summarizes older turns via the model itself, and is also
  user-triggerable with `/compact [instructions]`. This project has no
  equivalent at any layer -- a session's transcript just grows. That's a
  real omission specifically because `session_autonomous`, `session
  schedule --every`, and now `rlm_heartbeat()`/`/heartbeat` all exist to
  keep a session running indefinitely with no human in the loop, and none
  of them has any mitigation for the context eventually overflowing the
  provider's own window -- the session just runs until a real provider
  rejects an oversized request. The most load-bearing gap in this list,
  since every feature that would need it is already shipped.
- **RPC mode** (`--mode rpc`). `prime-agent`'s `rpc.md` describes a
  bidirectional JSON-RPC channel over stdin/stdout (`set_model`,
  `cycle_model`, `steer`, `follow_up`, `abort`, `bash`, `compact`, ...)
  for embedding `prime-agent` inside another program. This project's
  `--mode json` is one-way: it dumps the wire-protocol event stream, but
  there's no single command channel a client can write back through
  besides the ordinary `Request`/`Response` calls `client.rs` already
  makes over `daemon.sock`. Same transport this project already has
  (JSONL over a socket) -- the gap is a designed command surface, not new
  plumbing.
- **Interval-repeating heartbeats.** `prime-agent`'s heartbeats
  (`long-running-agents.md`) support `every <interval>` with a label,
  plus list/pause/resume/clear. This project's `rlm_heartbeat()`/
  `/heartbeat` (see "Medium-effort" above) fire exactly once per manual
  call -- there's no repeating variant. `schedule.rs` already has
  `ScheduleKind::Every { interval_ms }` for `session schedule add
  --every`; `trigger_heartbeat` reusing that instead of always sending
  `ScheduleKind::Once` is the shape of the fix, just not attempted yet.

## Needs a new subsystem

Architecturally significant `prime-agent` capabilities that would each
require a genuinely new subsystem this project has no analog of (most a
Python control environment, one an account/identity system) -- not
attempted here, and not silently implied by anything in
`ARCHITECTURE.md`'s "Known gaps" section:

- **Extensions, themes.** Named in this bullet's original heading
  alongside "skills" (now done, see the medium-effort section above) with
  no further elaboration anywhere in this project's own docs or
  `prime-agent`'s docs this project has read -- left here rather than
  silently dropped, since scoping an undefined surface isn't this
  document's job to invent.
- **The interactive TUI**'s rich editor/message-queue features (file
  reference, image paste, steering vs. follow-up queuing, `/tree`,
  `/fork`, `/clone`, `/compact`, `/export`, `/share`). (The bare
  read-a-line/send-a-prompt loop underneath the TUI itself is done --
  `session repl`, see the medium-effort section above.)
- **`/login`**, an in-session OAuth-style flow to Prime Intellect's own
  hosted account system. `prime-agent`'s own
  [quickstart](https://github.com/PrimeIntellect-ai/prime-agent/blob/main/packages/coding-agent/docs/quickstart.md)
  presents `/login` and setting an API key beforehand
  (`export ANTHROPIC_API_KEY=...`) as two alternative paths to the same
  destination -- a configured model backend. This project only ever had
  the second path: `rp_server.rs` reads `OPENAI_API_KEY`/
  `ANTHROPIC_API_KEY`/`GEMINI_API_KEY`/`GROQ_API_KEY` straight from the
  environment (see the "Medium-effort" section's provider-selection
  entry above). There's no Prime Intellect account for a local
  single-user harness to log into, and no other identity/account system
  this project has ever needed. Unlike the rest of this section, the
  missing subsystem isn't a Python control environment -- it's an OAuth
  client plus somewhere real to send it, and there's nothing on the
  other end for this project to authenticate against.
- **ACP mode** (Agent Client Protocol). `prime-agent`'s `--mode acp`
  speaks JSON-RPC 2.0 to editor integrations (Zed, VS Code). This is an
  editor-ecosystem integration surface this project has never targeted --
  there's no editor plugin on the other end to talk to, the same
  reasoning the `/login` bullet above uses for having no account to log
  into.
- **An embeddable SDK** (`createAgentSession()`, custom tools via
  `defineTool()`, programmatic model/auth configuration). Would need this
  project to become a library crate rather than a binary-only one --
  `Cargo.toml` has no `[lib]` target today. A real, nameable architectural
  change, not attempted.
- **A tree-structured session data model** (`/tree`/`/fork`/`/clone`'s
  underlying `id`/`parentId`/active-leaf JSONL structure, see
  `session-format.md`). Deeper than the already-cut TUI commands
  themselves: this project's session state (`state.json`/
  `transcript.jsonl`) is linear, one worker owning one line of turns, and
  branching would change that data model, not just add REPL commands on
  top of it.
- **Context files** (`AGENTS.md`/`CLAUDE.md` auto-loaded from a global
  dir and walked up from the current directory, `SYSTEM.md`/
  `APPEND_SYSTEM.md` system-prompt overrides). No equivalent exists here;
  a session only ever gets the text passed to `session new`/`session
  prompt`, plus whatever the Continual Harness accumulates on top.
- **A `settings.json` config file** (project + global, merged). Every
  knob in this project is a CLI flag or an env var, set fresh per
  invocation -- there's no persistent, mergeable config file layer
  underneath them.
- **`auth.json` with shell-command key resolution** (e.g.
  `"key": "!security find-generic-password ..."`). This project only
  ever reads a key from a literal env var (`rp_server.rs`'s
  `write_config`, see the "Medium-effort" section's provider-selection
  entry above) -- there's no indirection layer for resolving a key via an
  arbitrary shell command.
- **Custom / arbitrary OpenAI-compatible provider registration.** No way
  to point at an arbitrary base URL; `rp-server`'s own compiled-in
  provider list is the only surface, and this project has no extension
  mechanism (see the "Extensions" bullet above) to register a new one at
  runtime.

## Process

Each increment above gets implemented, tested (a real integration test
under `tests/`, not just a unit test), and checked off in place, with a
short note if the implementation diverged from the plan. This file is
updated in the same commit as the increment it tracks, not after the
fact.
