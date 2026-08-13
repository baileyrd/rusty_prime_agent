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
- [x] **`auth.json` with shell-command key resolution**, parity with
  `prime-agent`'s own `auth.json`. New `src/auth.rs`: `auth::load
  (state_root)` reads `<state_root>/auth.json` (global only, same
  cwd-visibility reason `settings.json`/`skills::discover` are, and same
  permissive "malformed or missing reads as no entries" stance
  `settings::load` established), mapping provider name to a `{"key":
  ...}` entry; `auth::resolve_key` returns a literal `key` string as-is,
  or (a string prefixed with `!`) the trimmed stdout of running the rest
  as a shell command -- `sh -c`/`cmd /C`, the exact cross-platform split
  `client::run_quality_gate` already used for an identical need, bounded
  by a 10s timeout so a forgotten interactive prompt (e.g. a GUI Keychain
  dialog) can't hang `rp-server` sidecar startup indefinitely. A
  non-zero exit is a loud error, not a silent "unconfigured".

  Precedence, highest wins: an already-set env var beats an `auth.json`
  entry for that same provider, which beats being unconfigured at all --
  the same "env var wins" order `settings.json`'s own overrides
  established, for a different pair of tiers (no hardcoded default to
  fall back to for an API key). `rp_server::resolve_auth_env` computes,
  for every `OPTIONAL_PROVIDERS` entry whose env var isn't already set,
  a resolved `(api_key_env, key)` pair from `auth.json`; `write_config`
  now activates a `[providers.*]` block when either condition holds, and
  `ensure_running` hands each resolved pair to the spawned `rp-server`
  child directly via `Command::env` -- the daemon's own process
  environment is never mutated (`std::env::set_var`), so an `auth.json`
  edit takes effect on the very next sidecar spawn without a daemon
  restart, and without leaking into anything else the daemon does.

  `known_providers` (`harness model list`, no daemon involved) only
  checks whether an `auth.json` entry *exists*, the same presence-only
  check it already used for env vars -- it deliberately never resolves a
  `!command` entry, so a plain listing never runs an arbitrary command as
  a side effect.

  Verified two ways: CI-safe unit tests (`src/auth.rs`'s own `load`/
  `resolve_key` in isolation -- a literal key, a `!echo ...` command, a
  failing command reported loudly; `src/rp_server.rs`'s
  `resolve_auth_env`/`write_config` -- an already-set env var skips
  `auth.json` entirely, including a `!command` that would error loudly if
  it ever actually ran; a literal and a `!command` entry both resolve and
  activate the right `[providers.*]` block) and a real end-to-end pass in
  this sandbox against a real `rp-server` sidecar: an `auth.json`
  `!echo ...` entry for `groq` actually reached the spawned child's
  environment and activated `groq` in both the generated
  `provider-config.toml` and `rp-server`'s own real `/v1/models`
  catalog, and a pre-set `GROQ_API_KEY` env var was confirmed (via a
  marker-file command that never got created) to short-circuit
  `auth.json` entirely rather than double-resolving it.
- [x] **Custom / arbitrary OpenAI-compatible provider registration**, a
  `<state_root>/providers.json` letting a session point at a
  self-hosted vLLM server, LM Studio, a company-internal proxy, or any
  other OpenAI-wire-compatible endpoint that `rp-server`'s own
  compiled-in `OPTIONAL_PROVIDERS` list has no entry for.

  Confirmed against `rusty_provider`'s real router source
  (`crates/router/src/config.rs`/`lib.rs`) before implementing, not
  guessed: a provider's *name* is an arbitrary TOML table key
  (`HashMap<String, ProviderConfig>`), routed on by splitting an
  incoming `"name/model"` string on the first `/` -- `kind` is the only
  closed enum (`openai`/`anthropic`/`gemini`), and any wire-compatible
  self-hosted endpoint registers as `kind = "openai"`, exactly
  `rusty_provider`'s own shipped `config.example.toml` pattern (`groq`/
  `together`/`fireworks`, three arbitrary names all `kind = "openai"`).
  This meant `provider.rs`/`cli.rs`/`client.rs` needed zero changes:
  `--model <name>/<model>` was always an opaque, unvalidated string
  forwarded straight through to `rp-server`, which is the only thing
  that ever rejects an unknown name (a 4xx from `/v1/chat/completions`).

  New `src/providers.rs`: `providers::load(state_root)` reads
  `{"<name>": {"base_url": "...", "kind": "openai"}}` entries
  (`kind` optional, defaults to `"openai"`), same permissive-parse
  stance `settings::load`/`auth::load` already take. `rp_server::
  all_providers` merges this with the hardcoded `OPTIONAL_PROVIDERS`
  into the one list `write_config`/`known_providers`/`resolve_auth_env`
  now all iterate instead of the bare const; a custom entry reusing a
  reserved name (a built-in provider name, or `"ollama"`) is silently
  dropped rather than erroring or colliding into a duplicate
  `[providers.*]` TOML table. A registered provider's key is supplied
  the exact same way a built-in one's is: an env var (derived as
  `<NAME>_API_KEY`, non-alphanumerics folded to `_`), or an `auth.json`
  entry keyed by the same provider name -- `auth.rs` needed no changes
  at all, since it was already a plain name-keyed map.

  Verified three ways: CI-safe unit tests (`src/providers.rs`'s own
  `load` in isolation; `src/rp_server.rs`'s `all_providers` merging in a
  custom entry, dropping one that reuses a reserved name, and
  `resolve_auth_env`/`write_config` resolving a custom provider's own
  `auth.json` entry and activating its `[providers.*]` block), a CI-safe
  integration test (`tests/model_list.rs`: a registered custom provider
  is listed, and its derived env var flips it to `configured`), and a
  real end-to-end pass in this sandbox against a real `rp-server`
  sidecar: Ollama's own OpenAI-compatible endpoint
  (`http://127.0.0.1:11434/v1`) registered under the made-up name
  `my-vllm` (not the built-in `ollama` special case) appeared as
  `my-vllm/*` in `rp-server`'s real `/v1/models` catalog, and `session
  new --model my-vllm/qwen2.5:0.5b` round-tripped a real completion
  through it.
- [x] **An embeddable SDK** (`[lib]` target), bounded parity with
  `prime-agent`'s own `createAgentSession()`/`defineTool()`/
  programmatic model-and-auth configuration -- deliberately two honest
  embedding layers matching this project's actual daemon/worker/socket
  architecture, not an assumed in-process agent loop the way
  `prime-agent`'s own SDK works:

  1. **In-process, no daemon at all.** `session::AgentSession::create`
     is exactly what `session.rs`'s own unit tests already constructed
     directly (`Box::new(EchoProvider)`/`Box::new(NoopToolRuntime)`), now
     `pub` (along with `provider`/`tool_runtime`/`protocol`/`error`/
     `paths`) for any external crate to do the same: a real, driveable
     session with no daemon/worker/socket machinery in the loop.
     Not a pure in-memory session, though -- `create` still does real
     filesystem I/O under a caller-supplied `state_root`, the same
     durability a daemon-backed session gets. `provider::ModelProvider`/
     `tool_runtime::ToolRuntime` were already plain `pub trait`s, already
     object-safe and `Send + Sync`, already how `AgentSession` stores
     them internally -- implementing either yourself is this project's
     answer to `defineTool()`, no separate registration API needed.
  2. **Drive a *running* daemon.** `dispatch_one_shot` (re-exported at
     the crate root) sends one `protocol::Request` over an already-
     running daemon's socket and returns a typed `protocol::Response` --
     the same connect-send-receive primitive `client.rs`'s own CLI-
     output functions already built on internally, promoted to `pub`
     instead of them: every `client::session_*`/`client::daemon_*`
     function renders straight to this process's own stdout
     (`println!`/`print_json`), which makes sense for a CLI binary and
     no sense for an external embedder, so those stay crate-internal.

  New `src/lib.rs`, taking over the full `mod` list `main.rs`'s own
  `run` dispatch used to own, splitting it into the public embedding
  surface above plus everything else staying a plain (still
  crate-internal-usable) `mod`. `Cargo.toml` gained a `[lib] name =
  "rusty_prime_agent"` section alongside the existing `[[bin]] name =
  "harness"`; `main.rs` shrank to the handful of process-level concerns
  that are genuinely bin-only and would be wrong to impose on an
  embedding host process -- `harden_inherited_stdio` (see that
  function's own doc comment) and the explicit `std::process::exit`
  (`rp_server::ensure_running`'s reaper task needs it, see that entry
  above) -- calling `rusty_prime_agent::run(&args)` for everything else.
  No internal module needed new visibility beyond that: item-level `pub`
  was already liberal throughout (nothing outside the crate could ever
  tell the difference before now), so this was almost entirely a
  question of which `mod` declarations became `pub mod`, not a rewrite
  of internal APIs.

  Explicitly out of scope for this increment: any semver/stability
  guarantee on the new public surface (`publish = false`/`0.1.0` stay),
  docs.rs-quality rustdoc coverage beyond what's here, and exposing
  `daemon`/`worker`/`ipython_runtime`/`zmtp` publicly -- nothing in
  either embedding layer needs them directly.

  Verified with two new CI-safe integration tests -- the first tests in
  this project not shaped as `std::process::Command::new(common::bin())`
  (every `tests/*.rs` file compiles as its own crate linking the lib the
  same way a real external embedder would, so this is genuine proof, not
  a same-crate shortcut): `tests/embedded_session.rs` constructs and
  prompts an `AgentSession` directly (plus a `create`-then-`recover`
  round trip proving real disk persistence with no daemon ever
  involved), and `tests/dispatch_one_shot.rs` drives a real running
  daemon (`common::daemon_start`, still a real subprocess) through the
  re-exported `dispatch_one_shot` call instead of parsing CLI stdout.
- [x] **Session-level forking, `session fork <id> [--at N] [--name
  NAME]`** -- a bounded, honest slice of `prime-agent`'s `/tree`/`/fork`/
  `/clone` (`id`/`parentId`/active-leaf JSONL structure, per
  `session-format.md`), landed only after closely investigating whether
  any bounded slice of that structure was buildable at all. It isn't:
  `TranscriptEntry`'s `sequence: u64` is a total, linear order
  interpreted by one meaning everywhere it's read (`build_turns`,
  `session attach`'s replay, `find_compaction_fold_count`'s backward
  walk, ...), so real intra-session branching (multiple divergent
  continuations of *one* transcript, with an active-leaf pointer to
  switch between) would need every one of those reworked at once -- not
  a field addable underneath them the way compaction's own boundary
  was, genuinely one atomic, invasive change. What *is* real and bounded
  is **session-level** forking: `session fork` creates a brand-new,
  independent session (own directory, own `state.json`/
  `transcript.jsonl`, own worker) whose starting transcript is a copy of
  an existing session's own transcript up through `--at N` (or the whole
  thing, if omitted) -- reusing this project's existing session-creation
  machinery the same way recursive subagents did, not a new intra-
  session data structure. Explicitly does NOT deliver: `/tree`
  visualization (nothing to visualize beyond `session children`'s
  existing parent/child view), active-leaf switching mid-session (each
  fork is a separate, permanently-diverged session, not a pointer moved
  within one), or `/clone`'s live-state duplication (a running kernel
  connection or MCP session dies with the source worker, same as any
  other session boundary).

  New `protocol::ForkedFrom { session_id, at_sequence }` on
  `SessionState`/`SessionSummary` (`#[serde(default)]`, the same
  pre-existing-`state.json` pattern every field added since Phase 1
  uses) -- deliberately distinct from `SessionState::parent_id`
  (recursive subagents relate whole sessions by *ownership*; this
  relates them by *shared transcript history*, and conflating the two
  would make one field mean two structurally unrelated things).
  `session::snapshot_for_fork` reads a source session's `state.json`/
  `transcript.jsonl` straight off disk (the same "files are the source
  of truth" reasoning `catalog::scan` already relies on, so this works
  whether or not the source's own worker happens to be running) and
  truncates to `--at N`, erroring loudly (a conflict, not silently
  clamped) if `N` is past the transcript's real end.
  `session::seed_forked_session` writes a fresh `state.json`/
  `transcript.jsonl` for the new session id, carrying forward the
  source's `model`/`thinking`/`tools`/`runtime` configuration but
  deliberately NOT `goal`/`harness` -- both are narrative fields whose
  accuracy depends on the *full* history they were last updated against,
  which a truncated copy may not match, so a fork starts with neither.
  `daemon::handle_session_fork` spawns the new worker with
  `WorkerMode::Resume` (not `New`, which always starts an empty
  transcript; not `Recover`, which would misleadingly append a crash
  marker) -- `AgentSession::recover`'s ordinary full-replay picks up
  exactly what `seed_forked_session` wrote, the same path any other
  resumed session goes through.

  Verified with unit tests (`snapshot_for_fork` truncating correctly and
  rejecting an out-of-range sequence; `seed_forked_session` producing a
  `state.json`/`transcript.jsonl` pair `AgentSession::recover` replays
  correctly, including the model/thinking-carries-forward,
  goal-does-not-carry-forward distinction) and a real end-to-end
  integration test suite (`tests/session_fork.rs`): truncation at a
  given sequence, forking the whole transcript, a display name, an
  out-of-range `--at` reported as a conflict, an unknown source session
  reported as a conflict, the fork and source staying fully independent
  after further prompts on each, and `--mode json`'s `forked_from`
  provenance -- plus a manual pass in this sandbox confirming the same
  end to end against the real compiled binary.
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
  `tests/`. `Cargo.toml` now has a `[lib]` target (see the "Embeddable
  SDK" entry below), but `ipython_runtime` is deliberately *not* part of
  its public surface -- `IpythonKernelRuntime` stays crate-internal on
  purpose, so a same-crate unit test is still the only way to construct
  one directly, same as before that target existed. Run explicitly
  against a real local `ipykernel` install (`pip install ipykernel`),
  the same infra-gated pattern `tests/ollama_provider.rs` uses. CI-safe coverage (`tests/ipython_runtime.rs`) proves the flag
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

  **Interval-repeating form** (`rlm_heartbeat(every="10m")`,
  `/heartbeat every <duration>`), parity with `prime-agent`'s
  `rlm_heartbeat.create(interval=...)`/`/heartbeat every 10m`: both
  triggers now optionally accept a duration string (`cli::
  parse_duration_ms`, made `pub(crate)` and reused as-is -- the exact
  same `30s`/`5m`/`2h`/`1d` shorthand `session schedule add --every`
  already parses, not a second parser to keep in sync). The kernel side rides the
  duration string along on the same printed marker line
  (`worker::bootstrap_kernel`'s `rlm_heartbeat(every=None)` prints
  `marker + (every or "")`, since a plain `print()` is the only channel
  back to this process; `session::extract_heartbeat_marker` splits it
  back out) and requests `ScheduleKind::Every { interval_ms }` instead of
  `ScheduleKind::Once` from the same `Request::ScheduleAdd` round trip --
  no new scheduling mechanism, `schedule.rs`'s existing recurring-fire
  support already does the work. `session_repl`'s `/heartbeat every
  <duration>` is different in kind from plain `/heartbeat`, not just in
  degree: a repeating heartbeat is a standing re-entry into the session,
  not a single "send it now" action, so it registers a real schedule
  (`client::schedule_add`) rather than sending a prompt immediately.
  Either trigger's resulting schedule is listed/canceled the same way
  any other one is (`session schedule list`/`cancel <id> <schedule-id>`)
  -- no separate heartbeat-specific list/pause/resume/clear surface
  needed, unlike `prime-agent`'s own heartbeat-specific management
  commands.

  Explicitly not implemented: rate-limiting/deduplicating rapid repeated
  `rlm_heartbeat()` calls (same "no sandboxing for a single local user"
  trust model already accepted elsewhere), and any change to
  `session_autonomous`'s own bounded loop -- these remain manual entry
  points into the same re-entry mechanism, not a third continuation
  policy.
- [x] **Automatic context compaction** (`session compact <id>
  [instructions...]`, `/compact [instructions]` in `session_repl`, plus
  an automatic trigger inside `AgentSession::prompt`'s own loop), parity
  with `prime-agent`'s `compaction.md`. Previously listed here as an
  identified-but-unstarted gap: "a real omission specifically because
  `session_autonomous`, `session schedule --every`, and `rlm_heartbeat()`/
  `/heartbeat` all exist to keep a session running indefinitely... none of
  them has any mitigation for the context eventually overflowing the
  provider's own window". `prime-agent`'s own trigger compares real
  tracked token usage against a real per-model context window; this
  project has neither (`provider::parse_response` never reads
  `rp-server`'s `usage` field, and no per-model context-window catalog
  exists), so `maybe_compact` uses a single fixed, deliberately
  approximate token estimate instead (`text.len() / 4`, overridable via
  `RUSTY_PRIME_AGENT_COMPACT_TRIGGER_TOKENS`/
  `RUSTY_PRIME_AGENT_COMPACT_KEEP_RECENT_TOKENS`) -- exact enough to
  decide "should compaction fire", not exact enough to enforce a hard
  budget a real token-aware client would. `EchoProvider` sessions
  (`state.model.is_none()`) never trigger it automatically and treat a
  manual `session compact`/`/compact` as a plain, honest no-op ("nothing
  to compact") -- there's no real model to ask for a summary, and
  `EchoProvider` never risks a real context-window overflow anyway.

  `transcript.jsonl` is never rewritten, truncated, or replayed
  differently -- compaction only changes what `build_turns` sends to the
  provider on the *next* prompt (a synthetic `TurnRole::System` turn
  carrying the running summary, replacing everything at or before the
  compacted boundary); `session attach`/`session repl` still show every
  turn that ever happened, preserving `session.rs`'s own "full JSONL
  replay, single source of truth" load-bearing decision untouched. A
  fresh `Role::System` transcript entry ("(compacted N turns into a
  running summary)") is appended each time compaction actually fires, so
  it's visible in the durable record too, not just inferred from the
  provider-facing turns. Re-summarized, not appended, on each fire: the
  summarization call is given the previous summary (if any) plus the
  newly-old turns, so `CompactionState::summary` always covers everything
  up through its boundary in one piece.

  Verified against a real model in this sandbox
  (`ollama_provider_compacts_after_crossing_the_trigger_threshold`,
  `#[ignore]`d for the same real-infra reasons as this file's other
  `ollama_provider.rs` tests): with the trigger/keep-recent thresholds set
  tiny via the env-var overrides above, two real prompts against
  `qwen2.5:0.5b` produced a genuine model-written summary and the expected
  transcript marker.

  Explicitly not implemented: real per-provider token accounting (would
  need `rp-server`'s `usage` field parsed and a per-model context-window
  catalog neither exists today), a growing chain of separate summaries
  (deliberately re-summarized into one running summary instead, see
  above), and any interaction with `session_autonomous`'s own turn/time
  budget (compaction is orthogonal to that loop, not a third stop
  condition).
- [x] **RPC mode** (`session rpc <id>`), parity with `prime-agent --mode
  rpc`. `prime-agent`'s `rpc.md` describes its own ~30-command custom
  protocol (by its own words, "not JSON-RPC 2.0") over stdin/stdout for
  embedding the agent in another program. Rather than inventing and
  maintaining a second command vocabulary, `session_rpc` reuses the wire
  protocol's own `Request`/`Response`/`SessionEvent` types directly --
  the same "don't invent a second JSON schema" choice `--mode json`
  already made (see `cli::OutputMode`'s own doc comment). Any `Request`
  variant is accepted (not a narrower session-scoped allowlist the way
  `prime-agent`'s own command set is) -- consistent with this project's
  blanket single-local-caller trust model, not an oversight.

  Two concurrent lanes share one stdout, serialized through a shared
  `rusty_tokio::sync::Mutex<()>` so a line from one never interleaves
  with a line from the other. The initial attach
  (`Response::SessionAttachStarted` plus the snapshot event) happens
  synchronously before the stdin loop starts -- caught during manual
  testing: spawning the event-forwarding lane as pure fire-and-forget
  before entering the stdin loop raced an empty/immediately-closed stdin
  against that background task ever running at all, so `harness session
  rpc <id> </dev/null` could exit with no output whatsoever depending on
  scheduling. Doing the first attach round trip inline first, then
  handing the *already-connected* stream to the background task for
  everything after, makes the initial snapshot deterministic. The
  foreground loop reads one stdin line at a time, each wrapped in its
  own `spawn_blocking` call (not one long-lived blocking task) so the
  loop stays `.await`-able between reads, parses it as a `Request`,
  dispatches it over an ordinary one-shot connection, and prints the
  `Response`. `Request::SessionAttach` sent as a command is rejected
  locally with an explanatory error rather than forwarded -- this mode
  already streams that session's events automatically, and the one-shot
  dispatcher isn't built to drain a second, redundant streaming
  connection.

  A second real race, this one caught by CI rather than manual testing
  (macOS specifically): a single piped command's own `SessionEvent`s can
  still be sitting unread on the background lane's socket when stdin
  hits EOF and this function would otherwise return immediately --
  `harness session rpc <id>` given exactly one command line could print
  the command's `Response` but exit before ever printing the `turn`
  events that same command produced. Not provider/network latency (by
  the time a `Response` is printed, its `SessionEvent`s are already
  broadcast -- see `session::AgentSession::append`), purely this
  process's own task-scheduling latency -- so a bounded 300ms grace sleep
  after the stdin loop ends, before actually returning, closes the
  common single-command case deterministically (confirmed with 8
  back-to-back local runs after the fix). An event from something other
  than a just-dispatched command (a concurrent schedule firing, another
  attached client's own prompt) can still race process exit -- an honest
  limitation no fixed grace window fully closes, and the one genuinely
  remaining best-effort edge in this design. Ends at stdin
  EOF, same convention `session_repl` already uses.

  Explicitly not implemented: everything in `rpc.md`'s much larger
  command surface that has no equivalent `Request` variant yet (`bash`,
  `set_model`/`cycle_model`, `fork`/`clone`, `get_session_stats`,
  `export_html`, ...), streaming message deltas (this project's provider
  path isn't streaming at all -- `provider::ProviderReply` is a complete
  reply or a complete tool-call batch, never a partial delta), and the
  Extension UI sub-protocol (`select`/`confirm`/`input`/`editor` dialogs)
  -- there is no extension system for it to serve (see "Needs a new
  subsystem" below).
- [x] **Context files** (`AGENTS.md`/`CLAUDE.md` auto-loading), parity
  with `prime-agent`'s own auto-loaded context files. A prior revision of
  this document filed the whole "Context files" bullet under "Needs a
  new subsystem" (assuming it needed the same project-local-discovery
  machinery `prompt_template::discover`/a hypothetical project-tier
  skills discovery would) -- on closer look, only the *project-local*
  half (walked up from cwd) actually needs that, for the same reason
  `skills::discover` stayed global-only: the worker process has no
  access to the CLI caller's own cwd. The *global* half doesn't have
  that problem at all, and turned out to be a small, bounded increment
  reusing a mechanism this project already had: `session::
  read_context_file` checks `<state_dir>/AGENTS.md`, then
  `<state_dir>/CLAUDE.md` (first found wins, not merged; empty/
  whitespace-only treated as missing), read fresh on every
  `build_turns` call -- the exact same "no caching, no persisted state,
  an edit takes effect on the next prompt" property `skills`/
  `prompt_template` discovery already have. Its content becomes an
  even-earlier system turn than the compaction summary's own (see that
  entry above), the same "provider-facing only, `transcript.jsonl`
  never touched" shape compaction's injection already established.

  Verified two ways: a unit test constructing a real `AgentSession`
  in-process (no daemon needed, `AgentSession::create` is a plain async
  call) and inspecting `build_turns`'s own output directly, and a real
  end-to-end test against `ollama/qwen2.5:0.5b` (`#[ignore]`d, same
  real-infra reasons as this project's other `ollama_provider.rs`
  tests) confirming a fact stated only in `AGENTS.md` actually reached
  the model's reply.

  The project-local half stays unimplemented for the same cwd reason
  `skills::discover`'s own project-local tier does; `SYSTEM.md`/
  `APPEND_SYSTEM.md` stay unimplemented too -- see "Needs a new
  subsystem" below.
- [x] **A `settings.json` config file**, parity with `prime-agent`'s own
  persistent config layer -- global only, same cwd-visibility reason
  `skills::discover`/`read_context_file` are global-only (no project tier
  attempted, and no merge between tiers to speak of as a result). Scoped
  narrowly to the only two tunables that make sense as a persistent
  default rather than a one-off override: the compaction thresholds
  (`compact_trigger_tokens`/`compact_keep_recent_tokens`, previously
  env-var-only). `prime-agent`'s own `settings.json` covers real estate
  this project has no equivalent knob for at all (`enabled`/telemetry,
  retry policy) and isn't attempted here.

  Precedence, highest wins: an env var beats `settings.json`, which beats
  the hardcoded default -- the same order the compaction thresholds'
  env-var overrides already established, just with one more fallback
  tier (`crate::settings::load`) inserted underneath. Field names are
  `snake_case` (`compact_trigger_tokens`, `compact_keep_recent_tokens`),
  matching this project's own JSON convention throughout rather than
  copying `prime-agent`'s own camelCase verbatim. Malformed or missing
  JSON reads as "no settings" (every field `None`) rather than a hard
  error -- the same permissive stance the env-var overrides already take
  for an unparseable value.

  Verified two ways: unit tests in both `src/settings.rs` (`load` in
  isolation -- missing file, malformed JSON, an empty object, unknown
  fields ignored) and `src/session.rs` (`compact_trigger_tokens`/
  `compact_keep_recent_tokens` actually consulting a real settings.json
  on a real state root, and an env var still winning when both are set),
  plus a real end-to-end test against `ollama/qwen2.5:0.5b`
  (`#[ignore]`d, same real-infra reasons as this project's other
  `ollama_provider.rs` tests) proving a `settings.json`-only threshold
  (no env var set at all) actually triggers a real compaction round trip
  through the daemon/worker/provider, not just the in-process unit tests.

## Needs a new subsystem

Architecturally significant `prime-agent` capabilities that would each
require a genuinely new subsystem this project has no analog of (most a
Python control environment, one an account/identity system) -- not
attempted here, and not silently implied by anything in
`ARCHITECTURE.md`'s "Known gaps" section:

- **Extensions.** Named in this bullet's original heading alongside
  "skills"/"themes" (skills now done, see the medium-effort section
  above) with no further elaboration anywhere in this project's own docs
  -- re-investigated closely rather than left as a bare assumption,
  since every other item on this list turned out to have *some* real
  spec to bound against once actually looked for. The search came up
  with exactly one concrete trace anywhere in this codebase: RPC mode's
  own "Explicitly not implemented" note (see that entry above) names an
  "Extension UI sub-protocol (`select`/`confirm`/`input`/`editor`
  dialogs)". That's real, but it describes a UI-facing dialog surface an
  *already-registered* extension would call to prompt a human (pick from
  a list, yes/no, free text, an editor buffer) -- structurally the
  mirror image of everything "extension-shaped" this project has
  actually built (`ToolRuntime`/`ModelProvider` trait implementability
  for an embedding host, MCP tools the *model* invokes, skills the model
  `import`s): all three of those are model- or embedder-initiated;
  dialogs are UI-initiated, asking a human something. It is not itself a
  specification of what an extension *is*, how one registers, or what
  else besides dialogs it could add -- there's no manifest format, no
  registration API, no capability list anywhere in this project's own
  reach to bound a first increment against. Implementing just the four
  dialog RPC verbs, with nothing in this codebase ever positioned to
  invoke them, would be indistinguishable from the RPC mode entry's
  already-tracked "rest of `rpc.md`'s command surface" gap under a
  different heading -- double-counting one gap as two, not a genuine new
  increment. Stays genuinely undefined, not silently dropped.
- **Themes.** Same conclusion as Extensions, reached the same way: no
  trace anywhere in this project's own docs beyond the same original
  "Skills, extensions, themes" heading skills eventually got carved out
  of. No format, no palette/token spec, no rendering surface this
  project has (a plain-text CLI/REPL has no theming surface to begin
  with; the interactive TUI a theme would apply to is itself a separate,
  already-tracked, not-yet-attempted item, see below) -- nothing to
  scope a bounded increment against.
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
- **Intra-session tree branching** (`/tree`'s underlying `id`/`parentId`/
  active-leaf JSONL structure inside one session's own transcript, see
  `session-format.md`). Investigated closely (see the "Session-level
  forking" entry above for what a closer look actually found) and
  confirmed still out of scope: `TranscriptEntry`'s `sequence: u64` is a
  total, linear order interpreted by one meaning everywhere it's read
  (`build_turns`, `session attach`'s replay, `find_compaction_fold_count`'s
  backward walk, ...) -- real branching needs every one of those to
  resolve "the transcript" against a chosen leaf/path first, all at once,
  not a field addable underneath them the way compaction's own boundary
  or `session fork`'s own provenance marker were. Unlike those two, this
  is one atomic, invasive change with no honest bounded slice to land
  first -- `/tree` visualization and active-leaf switching mid-session
  stay unimplemented alongside it.
- **`SYSTEM.md`/`APPEND_SYSTEM.md`** (system-prompt override/append).
  This project has no base system prompt at all to override or append
  to outside of `AGENTS.md`/`CLAUDE.md` auto-loading (see "Medium-effort"
  above) and the compaction summary -- there's nothing for either to
  hook into without a larger design change than either of those needed.
## Process

Each increment above gets implemented, tested (a real integration test
under `tests/`, not just a unit test), and checked off in place, with a
short note if the implementation diverged from the plan. This file is
updated in the same commit as the increment it tracks, not after the
fact.
