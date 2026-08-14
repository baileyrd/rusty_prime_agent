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
  fall back to for an API key). **Confirmed, not silently divergent:**
  `prime-agent`'s own `providers.md` states the opposite order (`auth.json`
  wins over an env var); this project deliberately keeps env-var-wins
  instead -- a permanent, intentional divergence from that documented
  upstream contract, not an open gap to close later (see `CLAIMS_AUDIT.md`'s
  own now-closed checklist entry for this). `rp_server::resolve_auth_env` computes,
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
  session data structure at the time this was written. (Both `/tree`
  visualization and active-leaf switching mid-session exist now -- see
  the "Intra-session tree branching" and "`/tree` navigation +
  active-leaf switching" entries further down -- but that's a separate
  mechanism from `session fork`'s own session-level copy, not something
  this entry grew after the fact.) `/clone`'s live-state duplication
  still isn't delivered by anything: a running kernel connection or MCP
  session dies with the source worker, same as any other session
  boundary.

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
- [x] **Intra-session tree branching: `id`/`parentId`/active-leaf
  transcript model** -- this item was previously written off ("one
  atomic, invasive change with no honest bounded slice to land first,"
  see the old "Needs a new subsystem" writeup this replaces) on the
  reasoning that `TranscriptEntry`'s `sequence: u64` is a total order
  every reader (`build_turns`, `find_compaction_fold_count`'s backward
  walk, ...) is built to interpret directly, so real branching would
  need all of them reworked at once. Revisited and actually built,
  because that framing missed a cheaper move: `sequence` didn't need to
  stop being a total append order -- what was missing was a *second*
  field recording each entry's actual predecessor, plus one chosen
  pointer saying which leaf is currently "the conversation." Once
  framed that way, this is additive, not invasive.

  New `TranscriptEntry::parent_sequence: Option<u64>` (`#[serde(default)]`)
  -- parity with `session-format.md`'s `parentId`, addressed via this
  project's own existing `sequence` identity rather than inventing a
  separate id scheme. Set once, automatically, by the shared
  `append_entry` helper (used by both `append` and
  `append_child_usage_attribution`) to whatever `SessionState::
  active_leaf_sequence` was at that exact moment -- no caller building a
  `TranscriptEntry` needs to know active-leaf tracking exists. New
  `SessionState::active_leaf_sequence: Option<u64>` (`#[serde(default)]`)
  names the current tip; `append_entry` also advances it to the entry it
  just wrote, so ordinary linear conversation flow always has
  `parent_sequence` equal to "the previous entry" with zero extra work
  anywhere else.

  The backward-compatibility rule that makes this genuinely additive:
  an entry with `parent_sequence: None` and `sequence > 1` is a *legacy*
  entry, written before this field existed and defaulted via
  `#[serde(default)]` -- `active_chain`'s own walk treats that case as
  an *implicit* link to `sequence - 1`, the flat order every
  pre-existing transcript already has. Only `sequence == 1` (a genuine
  root) with `parent_sequence: None` ends the walk for real. This means
  no session's `transcript.jsonl` ever needs rewriting to backfill real
  parent links into old entries -- a hard requirement given this
  project's own established "`transcript.jsonl` is never rewritten or
  truncated" precedent (already relied on by compaction).
  `AgentSession::recover` backfills `active_leaf_sequence` from the
  transcript's last entry when it's still `None` after loading
  `state.json`, the exact same reconciliation `last_sequence` already
  gets ("`state.json` is only ever a best-effort cache").

  New `AgentSession::active_chain(&self) -> Vec<&TranscriptEntry>` walks
  from `active_leaf_sequence` back to the root via `parent_sequence`
  (falling back to the legacy rule above) and reverses to root-to-leaf
  order -- "the actual conversation," as opposed to `self.transcript`
  (every entry ever appended, across every branch, in write order --
  still the full, unfiltered audit trail `session attach`/
  `snapshot_event` show). `build_turns` and `compact_now`'s candidate
  collection both switched from iterating `self.transcript` directly to
  `self.active_chain()` -- the exact two places the original "out of
  scope" writeup named as needing to "resolve 'the transcript' against a
  chosen leaf/path first." Compacting or replying from an inactive
  branch would otherwise fold or continue history the active
  conversation never actually had.

  New `AgentSession::set_active_leaf(&mut self, sequence) -> Result<u64>`
  (`pub(crate)`): validates `sequence` names a real entry anywhere in
  `self.transcript` (any branch -- switching *to* a different branch is
  the whole point), then mutates and persists
  `active_leaf_sequence`, the same "mutate + persist, no transcript
  entry" shape `rename` already has. It does not itself create a
  branch; the *next* `append_entry` call is what actually reveals one --
  if the redirected leaf already has a child down the previously-active
  path, the new append becomes a second child of that same parent, a
  real fork. New wire protocol `Request::SessionSetActiveLeaf {
  session_id, sequence }` / `Response::SessionSetActiveLeafAck {
  active_leaf_sequence }`, valid on both transports and forwarded from
  the public daemon socket to the owning worker's private socket
  unchanged, the same relay `SessionRename`/`SessionCompact` already
  use.

  This first increment was deliberately scoped to data model +
  protocol/backend mechanism only: no CLI/REPL command called
  `set_active_leaf` yet, matching this project's own established
  pattern of landing a protocol/backend increment before its CLI
  surface (`rlm_depth`/`rlm_max_depth` landed the same way). The
  `/tree` navigation surface that drives it is the very next entry in
  this list.

  Verified with new unit tests: ordinary linear `append` calls each
  produce `parent_sequence` equal to the previous entry; `set_active_leaf`
  followed by an `append` produces two distinct entries sharing the same
  `parent_sequence` (a real fork) with `active_chain` resolving to only
  the new branch; `set_active_leaf` with an unknown sequence is rejected
  as a conflict; and `AgentSession::recover` against a hand-written,
  pre-feature `state.json`/`transcript.jsonl` pair (no
  `active_leaf_sequence` field, no `parent_sequence` field on either
  line) correctly backfills and reconstructs the full linear chain via
  the legacy fallback rule. The full existing suite (unit + integration)
  stayed green after switching `build_turns`/`compact_now` onto
  `active_chain`, confirming no regression to ordinary single-branch
  sessions.
- [x] **`/tree` navigation + active-leaf switching** -- the CLI/REPL
  surface deferred from the increment just above. `harness session tree
  <id>` (display) and `harness session set-active-leaf <id> <sequence>`
  (navigation) as top-level commands, plus `/tree` and `/tree <sequence>`
  wired into `session_repl`'s loop the same "one command name, display
  with no argument, act with one" shape a bounded REPL slice can afford
  without a real interactive picker (`prime-agent`'s own `/tree` is a
  TUI feature with one; this project has no raw-mode UI yet to build one
  in, see the "Needs a new subsystem" TUI entry below).

  `client::session_tree` reconstructs the tree client-side from the two
  fields the wire protocol already carries end to end
  (`TranscriptEntry::parent_sequence`, `SessionState::
  active_leaf_sequence`, both already serialized onto `SessionEvent::
  Snapshot`) rather than adding a new pre-rendered request/response shape
  -- the same "the client renders, the wire carries data" split every
  other `--mode text` renderer in this project already follows. Its own
  `effective_parent` helper mirrors `AgentSession::active_chain`'s
  legacy-fallback rule exactly, so a pre-branching session still renders
  as the flat chain it always was. `client::session_set_active_leaf` is
  the first client surface to reach `Request::SessionSetActiveLeaf` at
  all.

  Fixed a real gap this increment's own tests surfaced: `set_active_leaf`
  returning `Err` for an unknown sequence, then propagated via a bare `?`
  out of `worker::handle_private_connection`, closed the private
  connection with no response at all -- every request relayed across
  that boundary before this one (`SessionRename`, `SessionCompact`)
  happens to never fail, so nothing had exercised this path. The daemon's
  own relay then saw the closed connection as "worker closed before
  responding" (a protocol error, not the real conflict), and the CLI
  printed an opaque message instead of the actual "no transcript entry at
  sequence 999" text. Fixed by matching `set_active_leaf`'s `Result`
  explicitly in `handle_private_connection` and writing a `Response::
  Error { conflict: true, .. }` back over the private connection instead
  -- the same explicit-match-not-`?` shape `daemon::Supervisor::
  handle_session_fork` already uses for its own genuinely-failable step,
  now established as the pattern for private-transport requests too.

  Verified with a new `tests/session_tree.rs` integration suite: ordinary
  prompting reports a linear chain with the current active leaf; `--mode
  text` marks exactly the active entry `(active)`; `set_active_leaf`
  followed by a prompt produces a real fork (two entries sharing one
  parent, `active_leaf_sequence` pointing down the new branch, both
  branches still present in the full transcript); an unknown sequence
  reports a conflict with the bad sequence in the message; and an empty
  session reports "no turns yet" rather than an empty/malformed tree.
  Plus a manual pass in this sandbox confirming the same end to end
  against the real compiled binary, including through `session_repl`'s
  `/tree`/`/tree <sequence>` lines.
- [x] **Branch summaries (`BranchSummaryEntry`)** and **`/clone`,
  re-investigated fresh.** This item was previously tracked as
  structurally absent -- `CLAIMS_AUDIT.md` used to say `BranchSummaryEntry`
  "depends on the tree structure... already confirmed absent." That
  premise no longer holds (the tree structure landed two entries up), so
  it earned an honest second look rather than staying cut on a stale
  reason -- the same standing instruction that reopened intra-session
  branching itself in the first place. `/clone` got the same fresh look:
  re-confirmed still out of scope, for a *different* reason than
  branching had (see below), not left unexamined.

  **Branch summaries: real, built.** Same shape decision as
  `ChildUsageAttribution` before it: a flat, optional
  `protocol::BranchSummary { branch_leaf_sequence, entry_count, summary }`
  field on `TranscriptEntry` (boxed -- its `String` was enough to trip
  clippy's `large_enum_variant` on `SessionEvent`, the same reasoning
  `SessionEvent::Snapshot` already boxes its own `SessionState` for),
  not a separate typed message-union class. Unlike `compact_now` (which
  *shrinks* the active chain in place), `AgentSession::branch_summarize`
  is read-only with respect to the branch it describes: it walks
  `branch_leaf_sequence`'s own branch back to wherever it diverges from
  the chain active *right now* (a new `effective_parent_sequence` free
  function, factored out of `active_chain`'s own walk so both share one
  implementation of the legacy-fallback rule), asks the session's own
  model to summarize those turns the same way `compact_now` asks it to
  fold old ones, and appends the result as an ordinary `Role::System`
  entry on the *current* active chain -- a durable, visible record of
  "here's what happened over on that other branch," not a mutation of
  the branch itself. `(false, None)` (not an error) for the same two
  honest no-op cases `compact_now` already established the shape for: no
  model configured, or `branch_leaf_sequence` is already part of the
  active chain (nothing "other" to summarize). An unknown
  `branch_leaf_sequence` is a real conflict, the same validation
  `set_active_leaf` already does -- and matched explicitly rather than
  propagated via `?` in `worker::handle_private_connection`, the same
  fix `SessionSetActiveLeaf` needed for the identical reason.

  New `Request::SessionBranchSummarize { session_id,
  branch_leaf_sequence }` / `Response::SessionBranchSummarizeAck {
  summarized, summary }`, forwarded from the public daemon socket to the
  owning worker's private socket unchanged, same relay every other
  session-mutating request uses. `harness session branch-summary <id>
  <sequence>` as a top-level command, plus `/branch-summary <sequence>`
  wired into `session_repl`, the same "protocol + top-level command +
  REPL wrapper" shape `/compact`/`/fork`/`/tree` already established.
  `session tree`'s own display was extended to show a short description
  for a `BranchSummary` entry (`(branch summary of sequence N, K turns)`)
  in place of its otherwise-empty `text` field.

  Verified with new unit tests in `session.rs` (unknown sequence is a
  conflict; a no-op with no model configured; a no-op for a sequence
  already on the active chain; a real summary -- using `EchoProvider`
  with `state.model` set to a fake string, the same "provider and model
  string are independent parameters" trick `seed_forked_session`'s own
  test already relies on -- correctly appended on the *current* active
  chain with the right `BranchSummary` fields), CI-safe integration
  tests in `tests/session_tree.rs` covering the same no-op/conflict
  cases end to end against a real daemon, and a new `#[ignore]`d
  real-model test in `tests/ollama_provider.rs`
  (`ollama_provider_summarizes_an_abandoned_branch`) -- run against this
  sandbox's real Ollama + `rp-server` and confirmed passing before this
  was written up. Plus a manual pass confirming `session tree`'s own
  enriched display end to end.

  **`/clone`: re-investigated, confirmed still out of scope, for a
  different reason than before.** The old reasoning ("depends on the
  tree structure") is gone now that the tree structure exists -- but the
  real blocker was never the *data model*, it's what `prime-agent`'s
  `/clone` actually duplicates: *live* interpreter/kernel state (in-flight
  Python variables, imported modules, open connections), not just
  durable transcript/config data. This project's own architecture has no
  mechanism for that at any layer -- `session stop`/a worker crash
  already lose an `IpythonKernelRuntime`'s live kernel state today (only
  `transcript.jsonl`/`state.json` survive), and `session fork` was built
  around exactly that same limit (`ARCHITECTURE.md`'s own "Session
  forking" section). A `session clone` that copied everything `session
  fork` already copies (full transcript, no truncation, goal/harness
  carried forward too) would just be `session fork` with the truncation
  and narrative-reset options turned off -- not a meaningfully different
  feature, and not what `/clone` is actually for upstream. Real live-state
  duplication would need OS-level process forking of a running kernel
  (or full interpreter-state serialization) -- a genuinely different,
  much larger mechanism than anything this increment's branch-summary
  work touched, so it stays unimplemented.
- [x] **Interactive TUI: raw-mode rendering foundation** -- the first of
  several increments building toward parity with `prime-agent`'s real
  interactive TUI (panes, cursor control, live re-rendering), previously
  tracked as one blocked-on-nothing-existing item in "Needs a new
  subsystem." Scoped deliberately narrow: raw terminal mode plus the
  minimal live re-rendering it enables (this project's own manual
  byte-level echo/backspace/cancel handling), not the rich editor
  (multiline input, `@` fuzzy file search, tab completion -- a later,
  separate increment) built on top of it.

  New `src/termctl.rs`: hand-rolled direct OS primitives via the
  `libc`/`windows-sys` FFI this project already depends on
  (`procutil.rs`'s own precedent -- a handful of small, direct syscalls,
  not a protocol or a parser), not a new terminal-UI dependency like
  `crossterm`. The same "hand-roll a narrowly scoped OS/protocol
  concern, don't hand-roll everything" reasoning that chose to hand-roll
  SHA-256/HMAC/a ZMTP client for RLM (see this file's "5a"/"5b" entries)
  while still using `serde_json` rather than a hand-rolled JSON parser --
  raw-mode terminal control is squarely the former. `termctl::is_tty()`
  (both stdin and stdout must be a real interactive terminal --
  `libc::isatty` on unix, `GetConsoleMode` succeeding on Windows) is what
  `session_repl` checks before engaging any of this at all: every one of
  this project's own tests pipes stdin/stdout (`Stdio::piped()`), so
  `is_tty()` reports `false` under test and the loop falls through to
  exactly the same `BufRead`-equivalent blocking-read behavior it always
  had -- raw mode is additive for a real interactive caller, never a
  requirement a non-interactive one has to satisfy.

  `termctl::RawModeGuard::enable()` puts the terminal into the standard
  raw-mode recipe (no canonical/line-buffered input, no local echo, no
  signal-generating control characters -- `Ctrl-C` arrives as a plain
  byte instead of `SIGINT`, handled explicitly by `session_repl`'s own
  read loop -- no CR-to-NL translation, no output post-processing) via
  direct `termios`/`tcgetattr`/`tcsetattr` calls on unix and the
  equivalent `GetConsoleMode`/`SetConsoleMode` input-mode flags on
  Windows, and restores the original mode on `Drop` -- including on an
  early return or panic unwind, so a crashed or `?`-short-circuited REPL
  never leaves the user's shell stuck in raw mode. Deliberately does not
  include terminal-size querying (`TIOCGWINSZ`/
  `GetConsoleScreenBufferInfo`) -- nothing in this increment needs it (no
  line-wrapping, no multi-line rendering yet); left for whichever later
  increment (the rich editor) actually needs the terminal's width.

  `session_repl`'s stdin loop now reads through `next_repl_line`/
  `read_raw_line` when connected to a real terminal (falling straight
  back to the pre-existing `read_line`-based path otherwise, unchanged):
  byte-by-byte reading with this project's own minimal manual editing --
  printable bytes echoed and appended, Backspace/Delete erases the last
  byte (visually and from the buffer), `Ctrl-C` cancels the current line
  (prints `^C`, starts a fresh one, the same behavior most REPLs already
  give `Ctrl-C` at a prompt) rather than exiting the process, `Ctrl-D` on
  an empty line signals EOF, Enter submits -- no multi-line editing, no
  cursor movement within the line, no history, no completion, all
  correctly deferred to the rich-editor increment rather than bundled in
  here. The rest of `session_repl`'s own command dispatch (`/heartbeat`,
  `/compact`, `/file`, `/fork`, `/tree`, `/branch-summary`, `/export`,
  ...) is untouched -- raw mode changes only how one line of input is
  read, layered underneath the existing loop the same way `/tree`
  layered onto it earlier, not a rewrite of it.

  Verified with CI-safe unit tests for the deterministic half of the
  raw-mode recipe (`termctl::make_raw`, a pure function taking a `termios`
  and returning the raw-mode-adjusted one, factored out specifically so
  it's testable without a real terminal -- confirms `ICANON`/`ECHO`/
  `ISIG`/`IEXTEN`/`IXON`/`ICRNL`/`BRKINT`/`INPCK`/`ISTRIP`/`OPOST` are
  cleared, `VMIN`/`VTIME` set to `1`/`0`, and `c_cflag` left untouched).
  `is_tty()`/`RawModeGuard::enable()` themselves aren't asserted on
  directly in CI -- their real behavior depends on the test process's
  own actual stdin/stdout, which isn't consistent across environments
  the way `cargo test`'s captured output makes other assertions
  reliable, the same reasoning real-model/real-kernel features in this
  project use `#[ignore]`d tests instead of CI assertions for. The full
  existing `tests/repl.rs` suite stayed green unchanged, confirming the
  piped/non-tty fallback path behaves identically to before this
  increment. Plus a real end-to-end verification pass in this sandbox
  against an actual pseudo-terminal (Python's `pty` module): typing
  `hel<Backspace>lo<Enter>` produced the correctly-edited `helo` prompt
  (confirming byte-level echo/backspace really ran); sending a raw
  `Ctrl-C` byte produced this project's own `^C` output and left the
  process alive and immediately responsive to a following `/exit` line --
  proof `ISIG` was genuinely disabled (a real terminal's default
  disposition would otherwise have delivered `SIGINT` and killed the
  process outright, not produced this project's own handled `^C` text).
- [x] **Interactive TUI: rich editor -- multi-line input, `@` fuzzy file
  search, Tab completion.** Builds directly on the raw-mode foundation
  just above: three named pieces of `prime-agent`'s rich-editor surface,
  each investigated for an honest bounded slice rather than assumed to
  need a full interactive dropdown/overlay UI (the thing `termctl`'s own
  doc comment already flagged as needing terminal cursor-positioning
  primitives that module deliberately doesn't have yet).

  **Multi-line input**: raw mode leaves `\r` (Enter) and `\n` (`Ctrl-J`)
  genuinely distinct bytes (cooked mode's `ICRNL` translation, which
  would fold them together, is one of the flags raw mode already
  clears) -- `read_raw_line` now treats `\r` as submit and `\n` as
  "insert a literal newline, keep composing," so a multi-paragraph
  prompt can be typed as itself instead of joined by hand into one
  `session prompt` line. Backspacing across a line boundary the user
  already committed with `Ctrl-J` rejoins the buffer but doesn't try to
  move the terminal's own cursor back up a line it's already scrolled
  past -- correctly deferred, needing the same cursor-positioning
  primitives noted above.

  **Tab completion and `@` fuzzy search share one mechanism**
  (`complete_repl_line`), triggered two ways: the buffer's very first
  word starting with `/` completes against a fixed list of this
  project's own REPL commands (`REPL_SLASH_COMMANDS`, kept as one list
  specifically so it can't drift from `session_repl`'s real dispatch);
  the *current* word (wherever it is in the line) starting with `@`
  fuzzy-completes the path fragment after it against real filesystem
  entries (`complete_at_path`). "Fuzzy" here means subsequence matching
  (`fuzzy_matches`: every character of the typed fragment appears
  somewhere in the candidate, in order, case-insensitively -- so `mn`
  completes toward `main.rs`), not just a prefix match, and not a
  ranked/scored match either -- there's no dropdown to rank *for*.
  Completion is bash-style: an unambiguous match completes fully, an
  ambiguous one completes to the longest shared prefix across every
  candidate (`common_prefix`), and a fragment with zero candidates or no
  further shared prefix rings the terminal bell (`\x07`) -- the same
  portable "can't complete that" signal every terminal already
  understands, no candidate-listing UI needed. This is the bounded,
  text-only slice of `prime-agent`'s own `@` fuzzy search: no live
  interactive dropdown that narrows as you type and lets you arrow
  through candidates -- that's the piece still genuinely blocked on
  `termctl` growing real cursor-positioning primitives, tracked below in
  "Needs a new subsystem," not attempted here.

  **`expand_at_references`** is the other half of the `@` slice: at
  submission time (not just while Tab-completing), every `@<path>` token
  found anywhere in the line -- typed out fully by hand, not
  necessarily Tab-completed -- that resolves to a real, readable file is
  expanded inline into that file's own content, formatted the same way
  `/file`'s own `pending_file_content` prefix already is. Placed
  precisely where referenced in the text rather than only prepended
  (unlike `/file`, which can only queue content ahead of the *next*
  whole prompt) -- a more precise placement `/file` structurally
  couldn't offer, now that `@` gives a natural point in the text to put
  it. A token that doesn't resolve to a real file (most likely an
  ordinary `@`-mention, not a botched reference) is left exactly as
  typed rather than guessed at or erroring. Deliberately applied
  regardless of whether the line came from raw-mode input or the piped/
  cooked-mode fallback every one of this project's own tests still
  uses -- so it's genuinely CI-testable without a real terminal, unlike
  the completion/multi-line pieces above.

  Verified with 16 new CI-safe unit tests directly on the pure helper
  functions (`expand_at_references` folding in a real file's content
  correctly, leaving a nonexistent path and plain `@`-free text
  untouched, preserving surrounding multi-line structure;
  `fuzzy_matches`'s in-order-subsequence/case-insensitive behavior;
  `common_prefix` across one/diverging/disjoint/zero candidates;
  `complete_at_path` fuzzy-matching real directory entries and marking
  directories with a trailing `/`; `complete_repl_line` completing an
  unambiguous command, partially completing an ambiguous one, returning
  nothing for a command with no match, correctly *not* treating a
  `/`-looking token past the first word as a command name, and
  completing an `@`-path fragment mid-line) plus two new CI-safe
  `tests/repl.rs` integration tests proving `@`-expansion end to end
  through a real (piped) `session repl` process. The full existing test
  suite (unit + integration) stayed green with zero changes elsewhere,
  confirming no regression to ordinary single-line, non-`@` REPL use.
  Plus a real end-to-end pass in this sandbox against an actual
  pseudo-terminal: typing `line one<Ctrl-J>line two<Enter>` produced a
  single prompt whose text was `line one\nline two` (the embedded
  newline intact, confirmed by `EchoProvider`'s own reply echoing it
  back); typing `/tr<Tab>` visibly erased and replaced itself with
  `/tree`; typing `@<dir>/mn<Tab>` visibly erased and replaced itself
  with the real, fuzzy-matched `@<dir>/main.rs`, and submitting that
  line produced a reply containing the referenced file's actual
  content -- proof the completion and the expansion genuinely connect
  end to end, not just independently in isolation.
- [x] **Interactive TUI: image paste support.** The "Needs a new
  subsystem" section (further down) previously framed this as blocked on
  "a content-type change to the transcript/provider boundary" -- true as
  far as it went, but a scoping pass first checked the *other* side of
  that boundary before assuming the whole thing needed building: `rp-
  server` (the sibling `rusty_provider` repo this project's own
  `RustyProviderModel` already shells out to) turned out to already have
  full, real multimodal support end to end -- `MessageContent`/
  `ContentPart` enums with `ContentPart::ImageUrl`, consumed by its
  Anthropic/Gemini/OpenAI-compatible backends alike (Ollama routes
  through the OpenAI-compatible path). The gap was never `rp-server`; it
  was entirely this project's own text-only wire/transcript shapes.

  Kept deliberately additive rather than restructuring `text`/`content`
  into a content-block union: a new `images: Option<Vec<String>>` field
  (each entry a `data:<mime>;base64,<...>` URI, the exact inline shape
  `ContentPart::ImageUrl` already accepts, so no new wire shape was
  needed on the `rp-server` side at all) sits alongside `text` on
  `protocol::TranscriptEntry`, `provider::ChatTurn`, and `protocol::
  Request::SessionPrompt`. Every existing code path that never touches
  `images` keeps compiling and behaving identically -- no restructuring
  tax paid anywhere else. `RustyProviderModel::build_request_body`
  switches a turn's JSON `content` from a plain string to a content-block
  array only when that turn actually carries images, one array entry per
  image plus (if non-empty) one text entry. Base64 encoding is a small,
  hand-rolled RFC 4648 encoder in `client.rs`, consistent with this
  project's established "hand-roll narrow protocol/encoding concerns
  instead of adding a dependency" precedent (the same reasoning behind
  the SHA-256/HMAC and ZMTP modules).

  Three real surfaces reach an image into a prompt, all going through one
  `AgentSession::prompt_with_images`: `/file <path>` and `@<path>` in
  `session_repl` (both already-existing local-file-reference mechanisms)
  now check the path's extension against a small recognized set (png,
  jpg, jpeg, gif, webp, bmp) *before* falling back to their existing
  text-inlining behavior, and route a real image to an out-of-band
  `images` list instead of inlining bytes into the prompt text -- for
  `@`, the literal `@path` token is left in the text unchanged (unlike a
  text-file `@`-expansion, which replaces the token with file content);
  for `/file`, the existing "queued" REPL feedback message names the
  image explicitly. A new `harness session prompt <id> --image <path>...
  <text...>` CLI flag (repeatable, hand-parsed since `scan_named_flag`
  only handles single-occurrence flags) is the third, non-REPL surface --
  it fails loudly on an unreadable path or unrecognized extension (unlike
  `/file`'s silent fall-through to "not an image, try as text"), since a
  CLI flag with a bad argument should error, not guess. Deliberately
  *not* added to `-p`/`--print` one-shot mode or the cross-session
  `agent_message_send` path in this increment -- both are additive follow-
  ups if ever needed, not required to prove the mechanism.

  `EchoProvider` (used by every CI-safe test) appends `" [+N image(s)]"`
  to its echoed reply text when the last user turn carried images -- a
  small, deliberate seam that lets tests prove images actually reached
  the provider without needing a real vision model for the bulk of
  coverage.

  Verified with new CI-safe unit tests in `provider.rs` (the content-
  block-array switch, including the images-present-but-empty-text and
  empty-images-list edge cases) and `session.rs` (`prompt_with_images`
  persisting images on the transcript entry and reaching the provider;
  plain `prompt` still carries no images -- a regression guard), a new
  `tests/image_paste.rs` (5 CI-safe integration tests covering single and
  multiple `--image` flags, image-only prompts with no text, and the two
  loud-failure cases: an unreadable path and a non-image extension, the
  latter confirmed via `session list` still showing zero turns -- no
  partial state written on a failed prompt), and two new `tests/repl.rs`
  integration tests for the `@`/`/file` image-detection paths. Two clippy
  fixes along the way: `SessionEvent::Turn`'s `TranscriptEntry` payload
  needed boxing (`large_enum_variant`, mirroring `Snapshot::state`'s
  existing `Box<SessionState>`) once `images` grew that struct, and the
  images-carrying append needed its own narrowly-scoped
  `append_user_turn_with_images` helper rather than adding an `images`
  parameter to the general-purpose `append` (which would have pushed it
  past clippy's `too_many_arguments` limit) -- `append`'s own signature
  and every existing caller stayed untouched.

  Real end-to-end proof against an actual vision model in this sandbox:
  `ollama pull moondream`, then a hand-rolled valid (not just PNG-magic-
  prefixed) solid-color PNG fixture -- a real zlib stream using
  uncompressed "stored" DEFLATE blocks plus real CRC32/Adler32 checksums,
  built without a new dependency for the same "narrow, self-contained
  encoding concern" reason as the base64 encoder -- fed through `session
  prompt --image <path> "What color is this image?"`. `moondream`
  correctly identified the color; the transcript snapshot confirmed the
  base64 data URI actually persisted on the user entry. `tests/
  ollama_provider.rs`'s `ollama_provider_describes_a_real_image`,
  `#[ignore]`d like this file's other real-model tests for the same "no
  reason for `ci.yml` to depend on a third repo plus a model download"
  reasoning.

  Bounded honestly, not attempted: real terminal clipboard/paste-protocol
  image capture (iTerm2's own protocol, the Kitty graphics protocol, OSC
  52) -- these are unstandardized, proprietary per-terminal wire formats,
  and `termctl.rs`'s raw-mode reader is a generic byte-stream reader with
  no image-protocol negotiation of any kind. "Image paste" here means
  "reference a local image file" (via `/file`, `@`, or `--image`), the
  same honest, bounded interpretation this project has applied to every
  other `prime-agent` surface that assumes a richer host environment than
  a terminal byte stream provides.
- [x] **Interactive TUI: steering vs. follow-up message queue.** Half of
  the surface this bullet used to name as one atomic "structurally
  absent" blob -- follow-up queuing -- turns out to be genuinely
  buildable; the other half, steering (interrupting an in-flight prompt
  rather than queuing behind it), is a real, separately-tracked gap (see
  "Needs a new subsystem" below -- as of "Bounded candidates batch 1" a
  cancellation primitive does exist, `Request::SessionInterrupt`/`harness
  session interrupt <id>`, but this REPL's own dispatch loop still can't
  call it on itself: any line typed while busy is unconditionally
  queued, never dispatched immediately, so there's no way to type
  `/interrupt` and have this same session act on it right away).

  Before this increment, `session_repl`'s stdin loop was fully
  synchronous: read one line, `.await` its full reply, only then read
  the next -- there was never a window during which a second line could
  even be read, let alone dispatched. Reading now lives on its own
  persistent background task (`rusty_tokio::spawn_blocking`, looping
  internally -- a *persistent* reader is required here, unlike
  `session_rpc`'s own per-line `spawn_blocking` call, because racing a
  *fresh* blocking read against an in-flight prompt on every loop
  iteration would risk two concurrent blocking reads on the same fd if
  the prompt happened to finish first and the read was abandoned
  mid-flight), feeding lines into an unbounded channel. The main loop
  races that channel against whatever prompt is currently in flight
  (`rusty_tokio::select!`, this project's own hand-rolled two-to-five-way
  future-racing macro -- see `rusty_tokio`'s crate docs): a line that
  arrives while nothing is in flight dispatches immediately, unchanged
  from before; a line that arrives while a prompt is still generating is
  queued and dispatched, in FIFO order, once the in-flight prompt's
  reply lands. Only ordinary prompt sends run concurrently with reading
  -- slash commands still execute synchronously once dispatched (see
  the "full slash-command surface" entry below for why widening every
  one of them to also run concurrently is a separate, larger increment).

  A real implementation hazard, found while wiring this up rather than
  assumed away: `rusty_tokio::select!` expands each branch into one
  shared `move` `poll_fn` closure, and a `move` closure captures every
  referenced outer variable *by value* -- not merely by the reference
  its usage would otherwise need. The first draft mutated `current`/
  `queue`/`reader_done` directly from inside `select!` branch bodies;
  this both silently discarded the mutation (the closure's own copy was
  dropped once the `.await` completed) and failed to compile on the
  *second* loop iteration ("borrow of moved value" -- the same outer
  variable had already been moved into the *first* iteration's now-
  dropped closure). Fixed by having every branch compute and return a
  plain value (a two-variant `Wake` enum: the prompt finished, or the
  reader produced something) with zero side effects inside the macro,
  and doing every mutation in ordinary code immediately after the
  `select!` call completes, once no closure is holding a borrow of
  anything. The channel receiver itself needed the same treatment for a
  different reason: it's reused across every loop iteration (both inside
  `select!` and in the plain idle-path `.await` below it), so it can
  never itself be moved into a `select!`-generated closure -- a fresh
  `&mut line_rx` reborrow, taken immediately before each use, is what
  actually gets captured instead.

  A second, real concurrent-output hazard: raw mode's own per-keystroke
  echo (`read_raw_line`, on the persistent background task) can now run
  at the same moment the main task prints a reply that just finished
  generating -- unlike before, when reading and printing could never
  overlap by construction. Guarded with a shared `Arc<std::sync::Mutex<
  ()>>` (not `rusty_tokio::sync::Mutex` -- the reader lives in a
  genuinely synchronous `spawn_blocking` closure that can't `.await` an
  async lock), the same shared-lock shape `session_rpc`'s own
  `stdout_lock`/`forward_events` already established for an analogous
  problem. Bounded, not exhaustive: it covers every `read_raw_line`
  write plus this increment's own two new output points, but doesn't
  reach into `session_compact`/`session_fork`/`session_tree`/etc.'s own
  internal print calls (shared with the plain top-level, non-REPL CLI
  commands, which have no such lock and no reason to need one) -- a
  slash command's own output can still, in principle, interleave with
  live keystroke echo of whatever's typed immediately afterward, now
  that the reader runs continuously regardless of what the main task is
  doing. Stated honestly rather than left silently uncovered: closing
  that residual gap would mean threading a lock through every shared
  client function's own print calls, materially larger than this
  increment's scope.

  Verified with a new CI-safe `tests/repl.rs` integration test
  (`repl_queues_lines_typed_while_a_reply_is_still_in_flight`) that pipes
  four lines at once and asserts all four replies land, in order, and
  that the "queued" notice fires -- reliable in practice (confirmed
  across repeated runs) because a kernel pipe read is effectively
  instant while a daemon round trip (a real socket hop plus transcript
  persistence) is not, so the background reader routinely gets ahead of
  the first prompt's own completion. Cross-checked the existing full
  test suite stays green (every pre-existing REPL/piped-stdin test
  behaves identically). Plus a real pty pass in this sandbox (Python's
  `pty` module, the same technique used to verify raw mode itself in the
  "raw-mode rendering foundation" entry): typing three lines back to
  back under genuine raw-mode terminal control produced two "queued"
  notices and all three replies in strict order (`[2]`/`[4]`/`[6]`),
  reproduced identically across three separate runs, with clean,
  non-garbled echo throughout -- proof the `stdout_lock` fix genuinely
  holds under a real terminal, not just in the piped/cooked-mode tests.
  (One artifact surfaced and ruled out during that manual pass: typing
  *immediately* at process spawn, before `RawModeGuard::enable()` has
  had a chance to run, can produce a doubled echo of the very first
  line -- a startup race in the pre-existing raw-mode foundation from
  task #76, not something this increment introduced, and not reachable
  by any real human typing at a real terminal.)
- [x] **Interactive TUI: full slash-command surface.** A scoping pass
  over `prime-agent`'s own ~23-command slash table (rather than treating
  "and more" from `CLAIMS_AUDIT.md`'s own earlier partial listing as the
  final word) sorted every remaining named command into three real
  buckets: commands with an existing `client::` function ready to wire
  in with zero new capability, commands buildable with real (if
  non-trivial) restructuring, and commands genuinely blocked on a
  missing subsystem this project doesn't have.

  **Wired in, zero new capability, same one-call shape every prior REPL
  command uses** (`/fork`/`/tree`/`/compact` above): `/name <text>` →
  the existing `session_rename`; `/refine` → the existing
  `session_refine` (the Continual Harness, see that entry above);
  `/session` → the existing `session_list`, a bounded slice of
  `prime-agent`'s `/session` picker (lists every session, the same as
  `harness session list`; not the full interactive search/sort/rename/
  delete-via-trash surface `sessions.md` documents -- there's no soft-
  delete primitive anywhere in this project, and no interactive picker
  UI, the same `termctl` cursor-positioning gap the rich-editor entry
  above already names); `/model` → the existing `model_list`, a bounded
  slice of `prime-agent`'s `/model` (lists configured providers, not
  mid-session switching -- see "Needs a new subsystem" below); `/reload`
  → not a missing-wiring gap at all, just a REPL command confirming in
  words what `session::build_turns` already does every single turn
  (re-reads `AGENTS.md`/`CLAUDE.md`/`SYSTEM.md` fresh) -- investigated
  rather than assumed stale, since a literal "nothing to reload" surprise
  is worse than silence for a command a `prime-agent` user might
  reasonably still type out of habit.

  **Built with real restructuring, not just wiring**: `/new [name]` and
  `/resume <id>`, bounded parity with `prime-agent`'s own `/new`/
  `/resume` (`-c`/`-r [path|id]`). Both switch which session *this same
  REPL process* operates on for the rest of the run -- `session_repl`'s
  own `session_id` parameter had to become a `let mut` local (previously
  fixed for the whole function) for either to be possible at all. `/new`
  reuses `create_session` (the same helper `session_new` itself calls)
  with only an optional display name -- every other `session new` flag
  (`--model`/`--goal`/`--thinking`/`--tools`/`--runtime`) is deliberately
  left out of this REPL slice, the same "extract the tractable
  mechanism, leave the rich surface out" bound `session spawn`/prompt
  templates already use elsewhere. `/resume <id>` validates the target
  exists (`fetch_transcript_snapshot`, the same call the REPL's own
  startup snapshot uses) before actually switching, so a typo never
  leaves the loop pointed at a session that doesn't exist -- reported the
  same conflict `session attach` would give, not a silent no-op. Both
  refuse to switch while a prompt is in flight or a message is still
  queued behind the switch command itself (`current.is_some() ||
  !queue.is_empty()`, increment #79's own state) rather than silently
  stranding a queued follow-up on whichever session happened to be
  active when it finally got sent -- a real interaction between this
  increment and the previous one, found by reasoning through what queued
  input means for a command that changes *which session* subsequent
  input targets, not assumed away.

  **Genuinely new capability, not previously named by their own bullet**
  -- worth stating honestly rather than leaving as an unlabeled gap:
  **`/model <name>`** (mid-session model switching) and **`/effort
  <level>`** (mid-session thinking-level cycling) both need a real
  protocol change (a new `Request` variant plus daemon/worker handling
  to mutate an already-running session's model/thinking-level) that
  doesn't exist in any form today -- model and thinking level are fixed
  at `session new` time, full stop. **`/usage`** needs a token/cost data
  model that was never tracked in the first place: no
  `usage_tokens`/`token_usage`/`cost_usd` field exists anywhere in
  `protocol.rs`/`session.rs` (confirmed by direct search, not inferred),
  so there's nothing for a `/usage` command to display even in a bounded
  form. **`/mcp login|logout`** needs an MCP-server-scoped enable/disable
  primitive that doesn't exist either -- MCP tool access is unconditional
  today, on or off only at the whole-session `--tools mcp` level.

  Verified with 9 new CI-safe `tests/repl.rs` integration tests: one per
  trivially-wired command (`/name`, `/refine`, `/session`, `/model`,
  `/reload`), one proving `/new` actually switches sessions (the new
  session gets the turn, the original session's turn count stays at
  zero), one proving `/resume` switches to a real existing session and
  reports a conflict (staying on the current session) for an unknown id,
  and one proving the busy-guard: a message queued behind `/new` itself
  (both queued while an earlier prompt is still in flight, the same
  reliable-in-practice race increment #79's own test exercises) makes
  `/new` refuse to switch, and the queued message correctly lands on the
  *original* session once dequeued. One test-authoring lesson from
  writing these, not a product bug: an earlier draft of the `/resume`
  test piped its setup prompt and the `/resume` line in the same burst,
  accidentally triggering the exact busy-guard race the *other* new test
  deliberately exercises -- split into two separate REPL runs (an idle
  REPL's own first line never races anything) once the cause was
  understood, rather than loosening the guard to make the test pass.
- [x] **Themes: token spec + TUI renderer.** Previously blocked on "no
  renderer to apply tokens to" -- raw mode was the input side only, no
  colored/positioned output existed anywhere. That's still mostly true
  (no cursor positioning, no boxed panels, no markdown/syntax/diff
  rendering -- this project has none of those *features* at all, so
  there'd be nothing honest for most of the real token spec to color),
  but a real renderer for the output this project actually produces
  (plain text lines) is a genuinely different, much smaller thing, and
  turned out buildable now.

  **A real verification gap surfaced while scoping this, not glossed
  over**: this sandbox has no checkout of `prime-agent`'s own source,
  only this project's own second-hand characterization of
  `docs/themes.md` ("51 required color tokens across 6 categories, 4
  value formats"). A live fetch of that file was attempted -- it
  returned detailed, plausible-looking token lists, but three separate
  calls disagreed with each other on category counts (6, 7, then 8) and,
  added up, totaled 52 tokens against the claimed 51: signs of an
  unreliable extraction, not a verified source. `src/theme.rs`'s
  `REQUIRED_TOKENS` is the 52-token, 7-category list that recurred
  consistently across those attempts, kept as-is rather than silently
  trimmed to force-fit an unverified round number, and documented
  honestly in that module's own doc comment as "closely modeled on,"
  not "byte-for-byte verified against," `prime-agent`'s real spec --
  the same distinction this file already draws elsewhere between a real
  gap and an unverifiable claim.

  Design: a theme is `{"name", "vars", "colors"}` JSON (parity with the
  one structural detail every fetch attempt agreed on), `colors` mapping
  each of the 52 required token names to a `#rrggbb` hex literal, a bare
  `0`-`255` xterm palette index, a `vars` reference, or an empty string
  (terminal default) -- all four of `prime-agent`'s own claimed value
  formats. `Theme::from_file` enforces "every token required, none
  optional" (rejecting an incomplete theme outright, not padding it with
  defaults) the same way the real spec is described as working. Two
  built-in themes, `dark`/`light` (the one other detail every fetch
  attempt agreed on), are the only themes this increment actually ships
  with real colors -- both parse and validate the full 52-token set (so
  a hand-written custom theme file has to as well) but only assign a
  real color to the handful of tokens ([`success`]/[`error`]/[`warning`]/
  [`muted`]/[`dim`]/[`accent`]/[`text`]) anything in this project's own
  output actually uses; the rest resolve to
  [`ColorValue::Default`](../src/theme.rs) (no ANSI escape emitted) on
  both built-ins, an honest reflection of "this token exists in the
  schema but nothing renders it yet" rather than a plausible-looking but
  meaningless color choice.

  `settings.json` gains a `theme` field (`"dark"`/`"light"`, or a path
  to a custom theme JSON file), read once at `session repl` startup --
  no live reload, the same stance the file's two existing fields already
  have. An unreadable path, invalid JSON, or a theme missing required
  tokens all fall back to the built-in `dark` theme with a printed,
  colorized-as-`warning` explanation -- the same "an unparseable
  override degrades to the default, non-fatal" stance `settings::load`
  itself already takes for a bad value, not a new failure mode invented
  for this increment. Colorizing is gated on `termctl::is_tty()` (the
  exact check raw mode already uses) *and* the `NO_COLOR` convention
  (<https://no-color.org>) -- both confirmed with a real pty pass, not
  just unit tests: colors render correctly (confirmed byte-for-byte
  against the expected `\x1b[38;2;R;G;Bm...\x1b[0m` sequences) under a
  genuine terminal, and `NO_COLOR=1` suppresses every one of them,
  including the "(theme: dark)" startup note that only ever prints when
  colors are actually active in the first place.

  Wired into a deliberately small, honest set of `session_repl`'s own
  output: the "(queued -- ...)" follow-up notice (`muted`), the `/new`/
  `/resume` busy-guard refusal (`warning`), `/new`/`/resume` success
  confirmations (`success`), and their failure messages (`error`) --
  not `print_entry`/`print_json`/etc., which are shared with every
  other, non-REPL caller in `client.rs` and stay untouched. Verified
  with 20 new CI-safe unit tests directly on `theme.rs`'s pure functions
  (hex/256-color/`vars`/empty-string parsing, ANSI SGR sequence
  generation, both built-in themes defining every required token,
  `resolve`'s three outcomes: unset → `dark`, a built-in name, a custom
  file) plus 3 new `tests/repl.rs` integration tests exercising
  `settings.json`'s own `theme` field end to end (an unreadable custom
  theme path, a custom theme file missing required tokens, and a valid
  built-in name producing no warning) -- these stay CI-safe despite the
  real pty verification above, since the *warning* text itself prints
  unconditionally (only its *color* depends on a real terminal), so the
  fallback behavior is observable through ordinary piped stdio.
- [x] **`session_repl`'s `/file`, `/fork`, `/export`** -- bounded parity
  with a slice of `prime-agent`'s TUI-side rich-editor/message-queue
  features, investigated piece by piece (see "Needs a new subsystem"
  below for "steering", the one piece that genuinely doesn't have a
  bounded slice yet -- along with `/clone`/`/share` -- and why; `/tree`
  itself is covered by the entry just above, image paste and follow-up
  message queuing by their own entries further up).

  `/file <path>` reads a local file client-side and queues its content
  to be prepended to the *next* line that actually sends a prompt (a
  `pending_file_content: Option<String>` local to the REPL loop,
  surviving an intervening `/heartbeat`/`/compact`/`/fork` line rather
  than being silently dropped) -- no daemon/worker/protocol change, the
  same "reuse `send_prompt`'s existing path" shape `/compact` already
  has. `/fork [--at N] [--name TEXT]` wires the already-existing
  top-level `session fork` command (`client::session_fork`) directly
  into the REPL loop -- a small local argument parser
  (`parse_repl_fork_args`) rather than reusing `cli::scan_named_flag`
  (private to `cli.rs`, shaped around a full argv slice rather than one
  already-stripped REPL line). `/export <path>` writes the session's
  current transcript (fetched fresh) to a local file as pretty-printed
  JSON -- reuses the same already-`Serialize` `TranscriptEntry` type
  `--mode json` already renders, no new format invented.

  Verified with a new `tests/repl.rs` suite: `/file` queuing content
  into the next prompt and surviving a missing-file error without
  sending anything; `/fork` creating a real new session (checked via
  `--mode json`'s `session_new` response and `session list`); `/export`
  writing a real, round-trippable JSON file -- plus a manual pass in
  this sandbox confirming `/file`+`/export` together end to end against
  the real compiled binary.
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
  existed at the time this entry was first written (file reference,
  image paste, steering vs. follow-up queuing, `/tree`/`/fork`/`/clone`/
  `/compact`/`/export`/`/share`) -- this was the bare loop underneath all
  of that, the same "extract the tractable session-level mechanism,
  leave the rich surface out" move as `session spawn`/prompt templates
  above. Most have since landed (see the `/file`/`/fork`/`/export`,
  `/tree`/active-leaf-switching, image paste, and follow-up message
  queue entries further down); steering and `/clone`/`/share` still
  haven't, see "Needs a new subsystem" below for why.
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
  everything else on the same broadcast socket.

  **Later increment: the `control` channel.** `shell`+`iopub`+`control`
  are now all opened (`stdin`/`heartbeat` remain out of scope). `control`
  exists specifically to carry `comm_open`/`comm_msg` host-request replies
  without deadlocking a running `shell` cell -- `rlm-runtime.md`'s own
  stated reason for a second channel (`shell` processes messages
  serially, so a reply sent there for a request that originated mid-cell
  would never be read until that cell's own `execute_request` finished).
  Confirmed by direct raw-socket probing against a real `ipykernel`
  before writing any Rust that `control` answers with the exact same
  6-frame `<IDS|MSG>` shape `shell` does, so `send_shell`/`recv_shell`'s
  signing logic was factored into a shared `build_signed_message` rather
  than duplicated for `send_control`/`recv_control`. Given real,
  immediate use rather than sitting unused until the host-request
  protocol itself lands: `shutdown` now attempts a graceful
  `shutdown_request` over `control` first (bounded, best-effort --
  confirmed by the same probing that the kernel replies
  `{"status":"ok","restart":false}` and exits on its own), falling back
  to the same plain process kill either way. The `HEARTBEAT_MARKER`
  stdout hack (`session::execute_python_tool_call`) is untouched for
  now -- generalizing it into a real `host.request` comm protocol over
  this channel, and using that for a kernel-callable `rlm(...)`, is
  tracked as the next increment in this series. The kernel subprocess is
  deliberately
  left un-detached (unlike every other spawned process this project
  manages): it's meant to live and die with the one worker/session that
  owns it, which also means `ipykernel`'s own parent-poller kills it for
  free if the worker crashes without a clean `shutdown`.

  **Later increment: the `host.request` comm protocol.** A real,
  generic typed-request channel now exists: `tool_runtime::HostRequest`
  (`comm_id`/`kind`/`payload`) and a new `ToolRuntime::resume_execute`
  method, backing a kernel-side `host_request(kind, payload=None)`
  coroutine `bootstrap_kernel` now defines alongside `rlm_heartbeat`.
  `IpythonKernelRuntime::execute` pauses mid-cell (returning
  `ExecutionOutcome.pending_host_request`) the moment the kernel opens a
  comm targeting `"host.request"`, instead of blocking until timeout;
  `resume_execute` sends the reply over `control` as a `comm_msg` and
  continues draining until the cell either finishes or blocks on
  another request. Confirmed against a real kernel *before* writing the
  Rust side that this needs one thing `rlm-runtime.md` doesn't spell
  out: stock `ipykernel` only ever routes `comm_msg` through its
  `shell_handlers`, never `control_handlers` -- despite `control` being
  exactly where `rlm-runtime.md` says host-request replies travel, a
  real Jupyter kernel doesn't wire that up on its own. The bootstrap
  code monkeypatches `kernel.control_handlers['comm_msg'] = kernel.
  comm_manager.comm_msg` (and `comm_close` the same way) to make this
  true, and resolves the kernel-side `asyncio.Future` via `loop.
  call_soon_threadsafe` rather than a direct `set_result` -- confirmed
  necessary because the control handler runs on a different thread than
  the one the awaited cell is suspended on, exactly the reason `rlm-
  runtime.md` itself gives for needing `call_soon_threadsafe`. Proven
  end-to-end (`execute` pausing, `resume_execute` delivering a reply and
  finishing the cell) as a new `#[ignore]`d real-kernel test in
  `ipython_runtime.rs`'s own test module, same discipline as its
  siblings. Explicitly not done yet (at that point): no request `kind`
  was dispatched to anything -- `session.rs`'s tool-calling loop didn't
  call `resume_execute` at all, so this was proven machinery with no
  live caller inside a real session. `rlm_heartbeat` was deliberately
  *not* migrated onto this channel: it already works via the stdout
  marker, migrating it isn't free, and doing so alongside the first real
  consumer was judged more useful than migrating it in isolation -- it
  remains unmigrated even now that a real consumer exists.

  **Later increment: a kernel-callable `rlm(...)`.** `session::
  execute_python_tool_call` now actually loops on `pending_host_request`
  (the missing piece the prior paragraph flagged): it calls a new
  `AgentSession::handle_host_request` dispatcher, then `ToolRuntime::
  resume_execute`, repeating until the cell finishes -- a cell may
  `await` more than one host request, and `stdout` is concatenated
  across every pause the same way one uninterrupted `execute` call's own
  `stdout` would read. `handle_host_request` currently recognizes exactly
  one `kind`, `"rlm.run"`, dispatched to `handle_rlm_run`; an
  unrecognized `kind` gets `{"error": ...}` back rather than a
  `HarnessError` -- the kernel-side caller is meant to see and handle it,
  same posture as a Python-level exception elsewhere in this same
  method. `bootstrap_kernel` defines `rlm(task, name=None, model=None)`
  as a thin wrapper -- `await rlm("do the thing")` is exactly `await
  host_request("rlm.run", {"task": "do the thing"})` -- matching
  `rlm-runtime.md`'s own "one comm target, typed request kinds" shape
  (its example: "Bundled Python skills such as `goal` call `rlm.
  host_request("goal.get", ...)`").

  `handle_rlm_run` admits the child through the *exact same*
  `Request::SessionNew`/`Request::ScheduleAdd` daemon round trip
  `client::session_spawn` (`session spawn`) already uses -- this is that
  same underlying mechanism, just issued from inside the worker process
  instead of an external CLI invocation, the same "connect to this
  session's own `daemon.sock` like any other client" pattern
  `trigger_heartbeat` already established for `rlm_heartbeat`. Returns
  immediately after admission (an `{"rlm_child_id", "name",
  "session_dir", "model"}` dict, `rlm_child_id` currently just the new
  session id -- there's no separate child-slot-id concept here the way
  `rlm-runtime.md` describes), never waiting for the child's own reply --
  parity with `rlm(...)` "returns immediately after task admission...
  never waits for or returns the child's answer." Rejects admission once
  the recursion-depth limit is reached (see the next paragraph). A child
  admitted here becomes visible through the parent-scoped registry
  described further below.

  Real end-to-end coverage of the *kernel* side (a new `#[ignore]`d test,
  `real_kernel_rlm_call_opens_an_rlm_run_host_request`) proves `rlm(...)`
  produces the right `pending_host_request` (`kind: "rlm.run"`, `payload`
  carrying `task`/`name`/`model`) and returns exactly whatever the host
  replies with. The *daemon* side (`handle_rlm_run`'s `SessionNew`/
  `ScheduleAdd` calls) is deliberately not independently re-tested here:
  it reuses request types and handlers `tests/subagents.rs`'s `session
  spawn` coverage already exercises end to end, and combining a real
  kernel with a real daemon in one test needs the compiled binary's path
  (`CARGO_BIN_EXE_harness`), which Cargo only provides to integration
  tests (`tests/*.rs`), not to a lib crate's own `#[cfg(test)]` modules
  where `handle_rlm_run` (crate-private) is reachable -- a real
  infrastructure gap, not an oversight, recorded here rather than glossed
  over.

  **A real bug found and fixed along the way, not part of `rlm(...)`
  itself:** re-running the real-kernel test suite repeatedly while
  building this increment surfaced an intermittent multi-minute hang,
  reproducing roughly every other run. Root cause, confirmed by reading
  `zmtp.rs` directly: `ZmtpSocket::connect`'s ZMTP handshake reads
  (`read_exact`/`read_frame`) have no timeout of their own -- a kernel
  that's merely slow to answer a socket (not refusing the connection
  outright) can hang a single connect attempt indefinitely. `shell`'s
  own connect already retried on outright failure, but that retry loop
  never helps against a hang (the attempt never returns to be retried),
  and `iopub`/`control` had no retry logic at all. Fixed with a new
  `connect_with_retry` helper wrapping each attempt in a
  `CONNECT_ATTEMPT_TIMEOUT` (5s) before retrying, shared by all three
  sockets, bounded by the same overall `STARTUP_TIMEOUT` deadline shell's
  retry loop already used. Confirmed fixed by five consecutive real-kernel
  test runs with zero hangs, after two reproductions of the hang without
  it.

  **Later increment: recursion depth limits (`RLM_DEPTH`/
  `RLM_MAX_DEPTH`).** Parity with `rlm-runtime.md`'s
  `AgentSession.runRlmChild()` step 1, "Check `RLM_DEPTH <
  RLM_MAX_DEPTH`," and its stated default ("root sessions may create
  children; children may not create grandchildren unless configured
  higher"). `protocol::SessionState` gains persisted `rlm_depth`/
  `rlm_max_depth: u32` fields (`#[serde(default)]`, so old `state.json`
  files without them still parse, reading as `0`); `session::
  DEFAULT_RLM_MAX_DEPTH = 1` matches `rlm-runtime.md`'s own default
  exactly. The check itself lives client-side in `handle_rlm_run`, before
  a child is ever admitted -- `if self.state.rlm_depth >=
  self.state.rlm_max_depth`, returning `{"error": "recursion depth limit
  reached (RLM_DEPTH=..., RLM_MAX_DEPTH=...)"}` -- mirroring
  `rlm-runtime.md`'s own description of the parent checking this before
  admission, not the daemon rejecting it after the fact.

  The two values themselves are computed centrally by the daemon, in
  `daemon::handle_session_new`, not sent by any client (`NewSessionMeta`'s
  two new fields are always `None` at every call site outside the daemon
  itself, resolved there after the fact): a session with a `parent_id`
  looks the parent's own persisted state up (`catalog::
  read_session_state`, the same lookup that already validates the parent
  exists) and gets `parent.rlm_depth + 1` / the parent's own
  `rlm_max_depth` **inherited unchanged** -- not re-resolved -- matching
  "the inherited maximum depth" language in `rlm-runtime.md`. A root
  session (no `parent_id`) gets `rlm_depth = 0` and `rlm_max_depth` from a
  new `RUSTY_PRIME_AGENT_RLM_MAX_DEPTH` env var (parsed as `u32`, falling
  back to `DEFAULT_RLM_MAX_DEPTH` if unset or invalid), the same env-var-
  fallback shape already used for `RUSTY_PRIME_AGENT_MODEL`. A resumed or
  recovered worker gets both values read back out of persisted state
  unchanged, same treatment as `thinking`/`runtime`; a forked session gets
  fresh values (depth `0`, default max) since a fork is a standalone
  session, not tied into its source's own recursion tree -- matching the
  `parent_id: None` treatment a fork already has. Threading the values
  into the kernel bootstrap (`RLM_DEPTH`/`RLM_MAX_DEPTH` as plain Python
  int globals, needed before any `AgentSession` exists to read them back
  out of `state.json`) required a new `WorkerArgs::rlm_depth`/
  `rlm_max_depth` pair, plumbed through as `--rlm-depth`/`--rlm-max-depth`
  flags on the `__worker-main` subprocess invocation, the same "always
  supplied by the daemon at spawn time" pattern `model`/`thinking` already
  use. `SessionSummary` (`session list`'s own display struct) is
  deliberately *not* extended with these two fields -- a display nicety
  out of scope for the mechanism itself.

  Coverage: a CI-safe unit test
  (`handle_rlm_run_rejects_admission_once_the_depth_limit_is_reached`)
  proves the client-side rejection directly against a hand-constructed
  `AgentSession` with `rlm_depth == rlm_max_depth`, no daemon or kernel
  needed since the check itself never touches either. Two CI-safe
  integration tests in `tests/subagents.rs`
  (`session_spawn_inherits_the_parents_max_depth_and_increments_depth_by_one`,
  `session_new_defaults_to_max_depth_one_with_no_env_var_set`) prove the
  daemon's actual computation end to end -- `session spawn` reuses the
  exact same `SessionNew` round trip `rlm(...)`'s `handle_rlm_run` does,
  so this is genuine coverage of the shared mechanism, not a parallel
  untested path -- reading the persisted `rlm_depth`/`rlm_max_depth`
  straight out of each session's own `state.json` (a new `tests::common::
  session_rlm_depth` helper, same pattern as the existing `worker_pid`
  helper) across three generations (root, child, grandchild) and with/
  without `RUSTY_PRIME_AGENT_RLM_MAX_DEPTH` set. No new real-kernel test
  was added for this increment: the depth check happens entirely before
  any kernel-facing code runs, so the existing
  `real_kernel_rlm_call_opens_an_rlm_run_host_request` test (which never
  hits the limit) and the CI-safe tests above already cover every code
  path a kernel-involving test could reach.

  **Later increment: a parent-scoped child registry
  (`rlm.list_subagents()`/`rlm.delete_subagent()`).** Parity with
  `rlm-runtime.md`: "the TypeScript parent maintains the authoritative
  direct-child registry... `list_subagents()` returns stable child IDs,
  ... session IDs, names, directories, and running/completed status,"
  "`delete_subagent()` accepts an exact child ID, ... session ID, or
  unique name," and "deletion cancels or closes the runtime... It does
  not erase the transcript or artifacts on disk." This project has no
  separate registry data structure to maintain, though: a child's own
  `parent_id` (set once, at admission, by `handle_rlm_run`'s
  `Request::SessionNew`) is already the durable record of the
  relationship, so "the registry" is simply `Request::SessionList`
  filtered down to this session's own direct children -- the exact same
  derivation `client::session_children` (`session children <id>`)
  already performs, reached from inside the worker process instead of
  the CLI. Two new `handle_host_request` kinds, `"rlm.list_subagents"`/
  `"rlm.delete_subagent"`, back two new kernel-callable coroutines
  (`rlm_list_subagents()`/`rlm_delete_subagent(id)`) defined in
  `worker::bootstrap_kernel` alongside `rlm(...)` -- bare top-level
  functions, not methods on an `rlm` namespace object, the same
  deliberate simplification `rlm(...)` itself already made (see
  `CLAIMS_AUDIT.md`'s `rlm.md` entry: no `rlm` object of any kind,
  namespaced or otherwise, exists in kernel globals).

  `handle_delete_subagent`'s `id` is matched against a direct child's
  `session_id` first, falling back to an exact, unique `name` match --
  `active-session ID` and `session ID` collapse to the same concept here,
  same simplification `rlm_child_id` already made. Only a *direct* child
  of this session may be listed or deleted -- matching "parent-scoped":
  an unrelated session, or a grandchild admitted by one of this session's
  own children, is invisible to `list_subagents()` and rejected by
  `delete_subagent()` the same as an unknown id, not silently accepted.
  "Cancels or closes the runtime... does not erase the transcript or
  artifacts on disk" maps exactly onto this project's own `session stop`
  (`Request::SessionStop`): gracefully shuts the worker down, leaves
  `state.json`/`transcript.jsonl` untouched. No separate durable
  tombstone entry is written -- the stopped child's own persisted
  `status: Stopped`, visible via `list_subagents()`/`session list` from
  then on, already serves as that record without a second status-
  tracking mechanism to keep in sync with the first.

  Coverage follows the exact same shape and reasoning as `rlm(...)`
  itself: a new `#[ignore]`d real-kernel test
  (`real_kernel_rlm_list_and_delete_subagent_open_typed_host_requests`)
  proves the kernel-side wiring produces the right `pending_host_request`
  (`kind`/`payload`) for both calls and returns exactly whatever the host
  replies with. `handle_list_subagents`/`handle_delete_subagent`
  themselves are not independently re-tested with a real daemon, for the
  identical, already-documented infrastructure reason `handle_rlm_run`
  isn't: they reuse `Request::SessionList`/`Request::SessionStop`, the
  same request shapes `tests/subagents.rs`'s `session children` coverage
  and `tests/session_lifecycle.rs`'s `session stop` coverage already
  exercise end to end, and combining a real kernel with a real daemon in
  one test still needs `CARGO_BIN_EXE_harness`, still unavailable to a
  lib crate's own `#[cfg(test)]` modules.

  **Later increment: child usage/cost attribution to the parent turn.**
  Parity with `rlm-runtime.md`: "Prime Agent asynchronously folds the
  child's assistant usage and cost into the parent assistant turn that
  launched it," persisting "a `child_usage_attributed` entry containing:
  the target parent assistant message ID; the child usage being
  attributed; and the resulting aggregate usage." Closing this honestly
  required a real foundation first, since it didn't exist anywhere in
  this project: no `TranscriptEntry` had ever recorded a model call's own
  token usage, even for the model's *own* turns, let alone a child's.
  Checked directly against a real `rp-server`-fronted Ollama session
  before assuming anything: `rp-server`'s own `/v1/chat/completions`
  response already carries a top-level, OpenAI-shaped `usage:
  {prompt_tokens, completion_tokens, total_tokens}` object (confirmed via
  `crates/core/src/types.rs`'s own `Usage` struct in the `rusty_provider`
  source, then reconfirmed with a live prompt against a real Ollama
  model) -- this project's own `provider::parse_response` was simply
  discarding it, not missing a subsystem that didn't exist upstream. So
  the honest scope here was: read the data that was already there, not
  invent a token-accounting subsystem from scratch.

  `protocol::Usage { prompt_tokens, completion_tokens, total_tokens }`
  (with a `+` impl for summing) is the new shared type. `provider::
  ModelProvider::respond` now returns a `ProviderResponse { reply, usage:
  Option<Usage> }` instead of a bare `ProviderReply` -- `parse_response`
  extracts `usage` as a sibling of `choices`, `None` when the key is
  absent (`EchoProvider` always reports `None`, having made no real
  call), a malformed sub-field defaulting to `0` rather than failing an
  otherwise-successful reply over a telemetry nicety. `TranscriptEntry`
  gains `usage: Option<Usage>`, set on every `Role::Assistant` entry
  backed by a real model call (`session::AgentSession::prompt`'s tool-
  calling loop now destructures `ProviderResponse` and threads `usage`
  through both its `Text`- and `ToolCalls`-reply branches -- one real
  call's usage covers the whole call either way). The compaction-summary
  call (`compact_now`) deliberately does *not* record its own usage
  anywhere: it produces a `Role::System` entry, not the `Role::Assistant`
  "parent assistant message" this whole mechanism targets, and tracking
  meta-call usage is a separate concern left untouched here.

  "The target parent assistant message ID" is this project's own
  `sequence`, not a separate message-id concept: `SessionState` gains
  `spawned_from_sequence: Option<u64>`, set once, at admission, by
  `handle_rlm_run` capturing `self.state.last_sequence` -- at that exact
  point in `prompt`'s tool loop, that's the sequence of the `Role::
  Assistant` tool-calls entry whose `execute_python` call invoked
  `rlm(...)`. Unlike `rlm_depth`/`rlm_max_depth`, this can't be resolved
  server-side by the daemon (only the spawning worker knows its own
  `last_sequence`), so it travels over the wire as a new `Request::
  SessionNew::spawned_from_sequence` field, and (since `AgentSession::
  create`, not `::recover`, is the only place that reads `NewSessionMeta`
  at all) as a new `--spawned-from-sequence` `__worker-main` flag,
  `WorkerArgs` field, and `NewSessionMeta` field -- the same cross-
  process-boundary plumbing `model`/`goal`/`thinking` already established,
  not the "needed before `bootstrap_kernel` runs" urgency `rlm_depth`/
  `rlm_max_depth` had. Every other admission path (`session new`,
  `session spawn`, a fork) leaves it `None`.

  `session::AgentSession::attribute_child_usage(child_id)` is the
  mechanism itself, called via a new private-transport-only `Request::
  AttributeChildUsage { child_id }`/`Response::AttributeChildUsageAck {
  attributed }` pair. No separate registry data structure holds "which
  children still need attributing" -- idempotency comes entirely from
  scanning *this session's own* transcript for an existing attribution of
  `child_id` first (`Ok(false)`, no new entry, if found), the same "check
  my own durable state" pattern that makes a redundant delivery safe
  rather than a double-count. `Ok(false)` (not an error) also covers
  `child_id` not actually being a direct child of this session, or having
  no `spawned_from_sequence` at all (admitted via `session spawn`, not
  `rlm(...)` -- nothing to attribute). Otherwise: `child_usage` sums every
  `usage` in the child's own `transcript.jsonl` (a plain, already-durable
  read -- "the admission handle does not contain usage or completion
  data," so this always happens after the fact, never incrementally
  tracked in memory); `aggregate_usage` adds every *prior* attribution
  already recorded against the same `parent_message_sequence` (more than
  one child can be admitted from a single Python cell's assistant turn).
  A new `Role::System` entry with `child_usage_attributed: Some(...)` set
  (`TranscriptEntry::child_usage_attributed`, the new "flat struct,
  optional field per new capability" pattern `tool_calls`/`tool_call_id`
  already established) is the persisted record -- there's no separate
  tombstone concept.

  What triggers a delivery: `rlm-runtime.md` itself is vague here ("the
  specific trigger is not explicitly detailed," only that it's
  "asynchronous" and happens "after child completion"). This project's
  own RLM children are ordinary long-running sessions, not a bounded
  one-shot subprocess the way `rlm-runtime.md`'s own runtime treats them
  -- there is no "child task finished" event anywhere to hook. The
  closest real analog this architecture has is "the child's own worker
  stopped" (whether via `rlm.delete_subagent()`, a direct `session stop`,
  or a crash), so `daemon::Supervisor::attribute_pending_child_usage`
  polls for exactly that on the same cadence and in the same loop as
  `fire_due_schedules` (`SCHEDULE_POLL_INTERVAL`, 5s): for every session
  with a `parent_id` whose own worker just isn't alive, if that parent is
  itself `Active` right now, forward `Request::AttributeChildUsage` to
  the *parent's* own private worker socket (never write to the parent's
  `transcript.jsonl`/`state.json` directly -- same "only a session's own
  worker owns its persisted state" invariant `trigger_heartbeat` already
  established a workaround pattern for). An inactive parent is simply
  left for a later poll once it's running again, rather than logged as a
  failure every cycle forever. **A known, accepted inefficiency, recorded
  rather than glossed over:** there's no separate "already attempted"
  bookkeeping, so a long-stopped child of a continuously-`Active` parent
  keeps getting a harmless redundant delivery attempt every cycle for as
  long as that parent stays up, even after the one real attribution has
  already landed -- `attribute_child_usage`'s own idempotency check
  absorbs it every time, at the cost of a cheap, wasted round trip.

  Coverage: two new CI-safe unit tests
  (`attribute_child_usage_folds_the_childs_usage_into_a_new_parent_entry`,
  `attribute_child_usage_is_a_no_op_for_a_non_rlm_admitted_child`) prove
  the mechanism directly -- no daemon or kernel needed, since
  `attribute_child_usage` only ever touches this session's own in-memory
  transcript plus plain reads of another (directly-constructed, real)
  session's already-durable `state.json`/`transcript.jsonl` -- covering
  real aggregation math (two child turns' usage summed correctly),
  idempotency (a second call is a safe no-op, not a duplicate entry), and
  the non-`rlm`-admitted no-op case. A new `parse_response` unit test
  (`parse_response_extracts_usage_when_present`) proves the wire-parsing
  half. The daemon-side poll/relay (`attribute_pending_child_usage`/
  `attribute_one_child_usage`) is not independently re-tested: it reuses
  `catalog::scan` and the same `transport::connect`/`Request`/`Response`
  round trip `fire_one_schedule`/`handle_session_stop` already exercise
  end to end, and (like `handle_rlm_run`/`handle_list_subagents`/
  `handle_delete_subagent` before it) a genuine end-to-end proof would
  need a session actually admitted via `rlm(...)` -- the one thing this
  project's test infrastructure still can't combine with a real daemon in
  one test, the same documented gap as always. Manually verified in this
  sandbox instead: a real `session new --model ollama/qwen2.5:0.5b`
  prompt against a real running `rp-server`/Ollama produced a transcript
  entry with `"usage":{"prompt_tokens":36,"completion_tokens":8,
  "total_tokens":44}`, confirming the wire-parsing half end to end
  against real infrastructure, not just a hand-built fixture.

  **Later increment: kernel-callable `goal`/`agent_message`/`compact`
  skills.** Parity with `rlm.md`/`rlm-runtime.md`: "the `goal`,
  `agent_message`, `rlm_heartbeat`, and `compact` skills call `rlm.
  host_request(...)`" -- `rlm_heartbeat()` was already real (an earlier
  increment); this closes the other three. Real Python API signatures
  from `rlm-runtime.md`: `await goal.get()`, `await goal.create(task,
  token_budget=...)`, `await goal.complete()`; `await agent_message.send(
  message, receiver_role="parent")` / `await agent_message.send(...,
  receiver_role="child", receiver_name=child.session_name)`. `compact`
  itself was named but never given a documented signature there, so
  `await compact.now(instructions=None)` mirrors this project's own
  existing `session compact [instructions]`/`/compact [instructions]`
  naming instead of guessing at an upstream shape that was never
  specified. Implemented as three small namespace objects in
  `worker::bootstrap_kernel` (`goal`/`agent_message`/`compact`, each a
  bare class instance with one or a few `async def` methods), matching
  upstream's own dotted-call syntax exactly -- a deliberate departure
  from the bare-top-level-function simplification `rlm(...)`/
  `rlm_list_subagents()`/`rlm_delete_subagent()` already made, since here
  the upstream shape genuinely is a small namespace object per skill, not
  one big `rlm.*` surface.

  Five new `handle_host_request` kinds: `"goal.get"`/`"goal.create"`/
  `"goal.complete"` and `"compact.now"` all operate on *this same
  session's own state* -- "goal state, persistence... live in
  `AgentSession`," and compaction is likewise entirely local -- so unlike
  every `rlm.*`/`agent_message.*` kind (which all cross to another
  session and therefore need a daemon round trip), these four reuse
  `update_goal`/`compact_now` directly, the exact same in-process methods
  `Request::GoalUpdate`/`Request::SessionCompact` already call from the
  worker's own private-connection handler -- no round trip at all.
  `token_budget` (`goal.create`) is accepted but not enforced: this
  project's own `GoalState` has no token/wall-clock budget concept
  (`session_autonomous`'s own turn/time budget is a separate, unrelated
  mechanism), so there's nothing real to wire it to yet, the same
  "accept an argument that doesn't fully translate" looseness `rlm(...)`'s
  own `model` parameter already has. `"agent_message.send"` resolves
  `receiver_role="parent"` to `self.state.parent_id` directly (an error
  if there is none) and `receiver_role="child"` (with a required
  `receiver_name`) via the exact same `Request::SessionList`-filtered-
  by-`parent_id` lookup `handle_list_subagents` already performs,
  matched by name (an error if zero or more than one direct child has
  that name); delivery reuses this project's own existing `session
  message` mechanism verbatim -- a `"[from <session-id>] <message>"`-
  prefixed `Request::SessionPrompt` sent to the recipient's own worker
  via the daemon. Because `handle_host_request` now mutates `self.state`
  directly (`goal.*`/`compact.now`), its own signature changed from
  `&self` to `&mut self` -- a small, mechanical ripple, not a design
  change; every `rlm.*` handler stays `&self` since none of them ever
  need to.

  Coverage: two new CI-safe unit tests
  (`goal_and_compact_host_requests_operate_on_this_sessions_own_state`,
  `agent_message_send_to_parent_is_an_error_when_there_is_no_parent`)
  prove what's provable without a daemon -- the entire `goal.*`/
  `compact.now` surface (no daemon involved at all) plus
  `agent_message.send`'s `receiver_role="parent"`-with-no-parent error
  path (the one `agent_message.send` case that returns before ever
  touching the network). A new `#[ignore]`d real-kernel test
  (`real_kernel_goal_agent_message_and_compact_open_typed_host_requests`)
  proves the kernel-side wiring for all three skills produces the right
  `host.request` `kind`/`payload` and returns exactly whatever the host
  replies with. `agent_message.send`'s `receiver_role="child"` path
  (needs a real daemon just to look children up) is not independently
  re-tested, the same documented reason `handle_list_subagents` itself
  isn't: it reuses the identical `Request::SessionList` shape `tests/
  subagents.rs` already exercises end to end.

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

  Explicitly still not implemented: interrupt/cancel (the `control`
  channel itself is now wired, but nothing sends `interrupt_request` over
  it yet -- a running cell still can't be cancelled mid-execution),
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
  project originally had neither. **Partially closed since, as a side
  effect of the RLM child-usage-attribution work (see that entry
  below):** `provider::parse_response` now reads `rp-server`'s `usage`
  field and `TranscriptEntry::usage` persists it on every real assistant
  turn -- but `maybe_compact`'s own trigger still isn't wired to consume
  it (a separate change, not attempted here), and no per-model context-
  window catalog exists either, so `maybe_compact` still uses a single
  fixed, deliberately approximate token estimate instead (`text.len() /
  4`, overridable via
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

  Explicitly not implemented: wiring `maybe_compact`'s own trigger to the
  now-real `TranscriptEntry::usage` data instead of the `text.len() / 4`
  estimate (the data exists now; the trigger logic itself is untouched),
  a per-model context-window catalog (still doesn't exist), a growing
  chain of separate summaries (deliberately re-summarized into one
  running summary instead, see above), and any interaction with
  `session_autonomous`'s own turn/time budget (compaction is orthogonal
  to that loop, not a third stop condition).
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
- [x] **Extensions: manifest/registration format + event-hook system.**
  Previously filed under "Needs a new subsystem" (see that section's own
  entry, now narrowed rather than removed) as "genuinely undefined," then
  corrected once by reading `prime-agent`'s real `docs/extensions.md`
  directly -- a concrete spec exists: a default-export factory receiving
  an `ExtensionAPI`, `pi.registerTool()`/`registerCommand()`/
  `registerShortcut()`/`registerProvider()`/`on(event, handler)` across
  roughly 25 named lifecycle events. That correction stood; what changed
  here is scope, not the finding -- the "first increment" that entry
  called out as tractable (one blocking pre-tool-call hook plus one
  custom-command registration point) is what actually shipped, deliberately
  bounded rather than attempting the full ~25-event surface.

  **A real, load-bearing design constraint found before writing any
  code, not discovered via a hanging test**: this project's extensibility
  story is already Python-via-kernel (skills, RLM), not JS/TS like
  `prime-agent`'s own `ExtensionAPI` -- so an extension here is a Python
  package (`<state_root>/extensions/<name>/EXTENSION.md` +
  `__init__.py` defining `def register(pi): ...`), discovered the exact
  same way `skills::discover` already works (mirrored line-for-line in
  the new `src/extensions.rs`). The `pi` object's two methods,
  `pi.on("pre_tool_call", handler)` and `pi.register_command(name,
  handler, description="")`, had to stay *synchronous* Python (no `await
  host_request(...)`, unlike `goal`/`agent_message`/`compact`/`rlm(...)`)
  for a real reason: an extension's own `register(pi)` call happens
  through a plain `import <name>` statement inside `worker::
  bootstrap_kernel`'s bootstrap cell, and ordinary module-level code
  executed via `import` does **not** get IPython's own top-level-`await`
  support the way a genuine `execute_request` cell does -- an `async def
  register` pattern would need `bootstrap_kernel`'s single
  `tool_runtime.execute(&code)` call to pause and resume mid-registration
  (`pending_host_request` machinery `execute_python_tool_call` has and
  `bootstrap_kernel` deliberately doesn't), for no actual benefit:
  registration itself never needs to notify Rust mid-flight. What Rust
  *does* need (which commands got registered, whether any hook exists)
  is read back in one shot instead, via one more marker-prefixed `print`
  (`EXTENSION_REGISTRY_MARKER`) appended to the end of the same bootstrap
  code string -- the same "no host_request round trip needed" technique
  `session::HEARTBEAT_MARKER` already established for `rlm_heartbeat()`.

  `pi.on("pre_tool_call", handler)` is a real blocking hook:
  `AgentSession::execute_tool_call` -- confirmed to be the one real
  chokepoint every tool call passes through regardless of backend
  (`execute_python`, built-in read tools, MCP) before any implementation
  work started, not assumed -- now calls `run_pre_tool_call_hooks` first;
  a handler returning a non-empty string skips the real tool call
  entirely and substitutes that string as the result
  (`"blocked by extension: <reason>"`), matching `prime-agent`'s own
  "hook can veto/replace a tool call" semantics for this one event.
  `pi.register_command(name, handler, description="")` makes `/name
  <args>` invocable from `session_repl` (a new fallback in the slash-
  command dispatch chain, tried *after* every built-in command, calling
  the new `Request::SessionExtensionCommand` -> `daemon::
  handle_session_extension_command` -> worker relay ->
  `AgentSession::invoke_extension_command`); an unrecognized command name
  reads as a friendly `"unknown extension command: /name"`, not a hard
  error -- the same non-fatal shape `/resume <unknown-id>` already has.
  Deliberately **not** added to `REPL_SLASH_COMMANDS` (the flat list Tab-
  completion consults) -- extension commands aren't known statically, so
  there's no completion for them, an honest bounded limitation rather
  than a lookup against a registry Tab-completion would need to query
  live.

  Verified three ways: the full existing CI-safe suite plus a new
  CI-safe `tests/repl.rs` case (`repl_unknown_extension_command_reports_a_friendly_message`)
  proving the friendly-fallback path on a session that never ran
  `--runtime ipython` at all (no extension registry ever installed); a
  new `#[ignore]`d real-kernel test in `ipython_runtime.rs`
  (`real_kernel_pi_registers_a_command_and_a_pre_tool_call_hook`) driving
  the exact `_Pi` class `bootstrap_kernel` generates against a genuine
  `ipykernel`, confirming both `register_command`+`_run_command` and
  `on("pre_tool_call", ...)`+`_run_pre_tool_call_hooks` actually work --
  this surfaced one real bug in the test itself (not the implementation):
  an assertion expected calling a command that returns Python `None` to
  produce `execute_result` text `"None"`, but IPython's interactive
  displayhook suppresses `execute_result` entirely for a `None` value
  (identical to a plain Python REPL), so a "no output" case reads as no
  `execute_result` at all -- fixed once actually run against a real
  kernel, not assumed from reading the wire protocol docs; and the full
  `extensions.rs` unit suite (6 tests: empty/missing directory, a
  directory without `EXTENSION.md` skipped, a real extension discovered
  and parsed, a missing `description` field, sorted-by-name ordering) --
  a line-for-line mirror of `skills.rs`'s own test suite, same discovery
  code shape.

  Still genuinely absent, same "picked a real subset, not a fake-complete
  first slice" stance the original "Needs a new subsystem" entry called
  for: `registerTool`/`registerShortcut`/`registerProvider` (custom
  LLM-callable tools, keybindings, provider registration), the other
  ~23 named lifecycle events beyond `pre_tool_call`, the dialog-based
  user-interaction surface (`select`/`confirm`/`input`/`editor`), custom
  rendering, and extension-scoped persistent state. None of these are
  implied to exist by anything here or in `ARCHITECTURE.md`.
- [x] **`/login`: interactive provider-setup wizard.** Previously filed
  under "Needs a new subsystem" (see that section's own entry, now
  narrowed to just the OAuth half) as entirely out of scope, on the
  grounds that there's no Prime Intellect account system for a local
  single-user harness to authenticate against. Re-investigated rather
  than left as-is: that grounds is still true (this project genuinely has
  no OAuth client and nothing real to send one to), but `prime-agent`'s
  own quickstart names `/login` and `export ANTHROPIC_API_KEY=...` as two
  *alternative* paths to the same destination -- a configured model
  backend -- and this project already has a real destination for that:
  `auth.json` (see `auth.rs`'s own module doc comment). An interactive
  wizard around the actual destination is a bounded, honest slice even
  without the OAuth client bolted on top.

  `/login`, typed in `session_repl`, walks a two-question flow: prints
  every provider `rp_server::known_providers` knows about with its
  current configured/not-configured status, asks for a provider name,
  then asks for an API key, and saves it via the new `auth::write_key`
  (insert-or-overwrite one entry, preserving every other provider already
  in the file). A blank answer at either question cancels cleanly; an
  unrecognized provider name reports `"unknown provider ... -- run
  /login again"` rather than silently writing an entry `known_providers`
  would never activate; `ollama` (needs no key at all) short-circuits
  with its own message instead of prompting for one. Reads its two
  answers off the same `line_rx` channel every other REPL line already
  flows through -- safe because, like every other slash command in this
  loop, `/login` only ever runs while no prompt is in flight (`current`
  is `None`), so there is never a second concurrent reader racing it for
  the next line.

  A real caveat, stated rather than glossed over: `rp_server::
  ensure_running` only spawns its `rp-server` sidecar once and reuses it
  as long as it stays healthy (see that function's own source), so an
  `auth.json` edit made while a sidecar is already running doesn't take
  effect until that sidecar is restarted -- the wizard's own success
  message says so explicitly (`daemon shutdown` then `daemon start`).
  This isn't a bug in `/login` itself; it's the same "an `auth.json` edit
  takes effect on the next sidecar spawn" behavior `auth.rs`'s own module
  doc comment already documents for hand-editing the file directly --
  `/login` doesn't change that contract, just makes reaching it easier.

  Verified with three new CI-safe `tests/repl.rs` cases exercising the
  real daemon/worker/REPL process end to end (`EchoProvider`, no real
  provider key needed): a full run saves the entered key and every other
  field in `auth.json` is left as `write_key`'s own unit tests already
  proved untouched; an unknown provider name reports the friendly error
  and writes nothing; a blank provider answer cancels and writes nothing.
  Plus three new `auth.rs` unit tests for `write_key` itself (creates a
  fresh file, preserves an unrelated existing entry, overwrites a
  same-provider entry).

  Still genuinely absent: any actual OAuth client, anything resembling a
  Prime Intellect account, and (unlike a real terminal's password prompt)
  no input-hiding for the key -- this project's own raw-mode reader
  (`termctl`) has no "don't echo" mode today, so the key is visible on
  screen as it's typed, the same as everything else piped through this
  REPL. Not attempted here; a caller who wants the key hidden can still
  hand-edit `auth.json` directly instead.
- [x] **Self-update: best-effort translation.** `prime-agent update
  [--force]` (see `CLAIMS_AUDIT.md`'s own quickstart/usage entries,
  previously confirmed False/N/A) presumably checks whatever registry
  `prime-agent` itself is published to (npm) for a newer release. That
  premise doesn't transfer here at all: this project's own `Cargo.toml`
  says `publish = false`, there's no crates.io release, no GitHub
  Releases workflow -- no release channel exists to check *against*,
  confirmed by direct inspection rather than assumed from the command's
  name alone.

  What *is* real: this project is only ever built one way (`cargo build
  --release`, run directly in a git checkout of its own source -- see
  the README's own "Build" section, unchanged since day one). `harness
  update [--force]` (new `src/self_update.rs`, no daemon needed, same
  "pure local operation" shape as `model list`/`skill list`) translates
  the *substance* of "update" into the one thing that already covers
  every real user of this binary: `git pull` the checkout this exact
  binary was built from (`CARGO_MANIFEST_DIR`, embedded at compile
  time -- accurate for exactly the binary this project ever produces),
  then `cargo build --release` in the same directory unless `git pull`
  reported nothing new and `--force` wasn't passed. A missing checkout
  (the binary was copied somewhere else, or the source directory has
  since been deleted) fails loudly naming the embedded path, rather than
  silently doing nothing -- the same "loud failure over fake success"
  stance every other config-file/subsystem lookup in this project
  already takes.

  `--force` deliberately does **not** mean "discard uncommitted local
  changes" -- consistent with this whole project's own development
  discipline around destructive git operations, that would need an
  explicit, separate ask, never a side effect of an update flag. `git
  pull` already refuses, loudly, to overwrite uncommitted changes a
  merge would touch; nothing here second-guesses that. `--force` instead
  means "rebuild even if `git pull` alone saw nothing new" -- useful
  after a manual local edit or branch switch.

  Verified for real, not just unit-tested: `harness update` and
  `harness --mode json update` both run successfully against this
  project's own checkout in this sandbox, correctly reporting "Already
  up to date" and taking no rebuild action; a dedicated `#[ignore]`d
  test (`run_against_the_real_checkout_pulls_and_rebuilds`, real network
  access to `origin`, same reasoning as this project's other genuinely-
  external-state tests) was also run manually and passed. Two CI-safe
  unit tests cover what doesn't need real git/cargo: `SOURCE_ROOT`
  really does resolve to a `.git`-containing directory, and a plain temp
  directory (never `git init`-ed) reports "no release channel" rather
  than attempting a `git pull` against it.

  Still genuinely absent: any actual release-channel check (there is
  none to check), any binary-replacement/download mechanism (irrelevant
  without a release channel), and any attempt to hot-swap the
  *currently running* daemon's own in-memory code -- the wizard's own
  success message says to restart the daemon manually, the same honest
  "you still have to act to actually reach the new code" caveat
  `/login`'s own entry above already established for an `auth.json`
  edit.
- [x] **Telemetry: opt-in settings + local-only stub.** `CLAIMS_AUDIT.md`'s
  own `settings.md` audit previously confirmed `telemetry.*` entirely
  absent from `Settings` -- checked directly against the struct's
  complete field list, not inferred. `prime-agent`'s real telemetry
  presumably configures *where* usage events get sent, to some analytics
  collector this project has no equivalent of -- the same "nothing on
  the other end" shape `/login`'s missing OAuth backend and
  `self_update`'s missing release channel both already have, two entries
  above. Rather than inventing a fake destination to send anything to,
  this builds the one honest thing that's actually implementable without
  one: an explicit opt-in toggle plus a genuine, structurally
  local-only sink.

  `Settings` gains one new field, `telemetry_enabled: Option<bool>`
  (`None`/`Some(false)` both mean off -- opt-in, not opt-out, matching
  this entry's own title). New `src/telemetry.rs`: `telemetry::
  record(state_root, event, session_id, data)` checks that setting
  fresh on every call (no caching, same as every other `settings::load`
  consumer) and, only when it's `true`, appends one JSON line to
  `<state_root>/telemetry.jsonl`. "Local-only" is a structural property
  of the module, not a configuration choice worth re-checking later --
  there is no HTTP client, no collector URL, no network call anywhere in
  `telemetry.rs` at all, confirmed by the module's own small size rather
  than merely asserted; enabling telemetry can only ever grow a local
  file, and nothing in this project ever reads it back out.

  Two event kinds wired from real call sites, not fabricated for this
  feature: `session_created` (one event per new session, from
  `AgentSession::create` -- not `recover`, the same "new root session
  only" precedent `NewSessionMeta::goal`'s own doc comment already
  established for goal seeding) and `prompt` (one event per completed
  `prompt_with_images` call, recording `ok` and how many tool-call
  rounds it took, on both the success and error path -- the public
  method is now a thin wrapper around a renamed `..._inner` that does
  the actual work, so the telemetry call runs exactly once per turn
  regardless of which of the loop's several return points was taken).
  Failures writing the file are silently swallowed -- a telemetry write
  must never turn an otherwise-successful session operation into a
  failure.

  Verified two ways: `src/telemetry.rs`'s own unit tests exercise
  `record` directly (writes nothing when unset, writes nothing when
  explicitly `false`, appends one well-formed JSON line per call when
  `true`, omits `session_id` when `None`), and a new `tests/telemetry.rs`
  drives the real daemon/worker/`EchoProvider` path end to end: telemetry
  off by default writes nothing, explicitly `false` writes nothing, and
  `true` produces both a `session_created` and a `prompt` event in
  `telemetry.jsonl` with the real session id and a `tool_rounds` count
  that actually reflects what happened.

  Still genuinely absent: any event for `session recover`/`session
  stop`/tool-call-level granularity, an anonymous installation ID (or
  any ID at all -- events carry a real session id, not an anonymized
  one), any aggregation/summary view over the raw JSONL, and any actual
  transmission anywhere -- there is nothing to disable for privacy
  beyond simply never setting `telemetry_enabled` in the first place.
- [x] **Bounded candidates batch 1: `doctor`, `session heartbeat`,
  cancel primitive.** Three small, independent, previously-unattempted
  gaps closed together in one increment.

  **`harness doctor [--fix]`** (`CLAIMS_AUDIT.md` previously confirmed
  entirely absent) -- new `src/doctor.rs`, no daemon required (checking
  reachability is one of its own checks). Deliberately doesn't duplicate
  `session list`'s own `catalog::scan` (stale/crashed worker detection
  via `worker_pid` liveness); instead checks what nothing else in this
  project ever surfaces proactively: daemon reachability, whether
  `rp-server` can be found on `PATH` (new `rp_server::
  rp_server_available`, checks existence only -- deliberately never
  *runs* `rp-server` with a guessed flag to probe it, since this project
  doesn't control that binary's own CLI surface), and whether
  `settings.json`/`auth.json`/`providers.json` actually parse as JSON --
  a real, previously-invisible gap, since every one of `settings::load`/
  `auth::load`/`providers::load` is deliberately permissive (a malformed
  file silently reads as "no config," the right default for every other
  caller, but it means a typo was otherwise undiscoverable). `--fix` is
  narrow on purpose: it only ever starts the daemon if it wasn't already
  running (the same idempotent spawn `daemon start` itself uses) --
  never rewrites a config file or otherwise mutates state on the
  caller's behalf, the same "no destructive action without an explicit,
  separate ask" stance `self_update`'s own `--force` already takes.

  **`harness session heartbeat <id> [--every DURATION]`** -- a top-level
  CLI entry point into the exact same "continue toward the goal"
  re-entry mechanism `session_repl`'s own `/heartbeat`/`/heartbeat every
  <duration>` lines already cover (see the "`/heartbeat` and
  `rlm_heartbeat()`" entry above), for a caller who wants it without an
  interactive REPL -- parity with `session compact` already existing
  alongside `/compact`. A deliberately *separate* implementation from
  the REPL's own inline lines, not a shared helper: the REPL tolerates a
  bad `/heartbeat every <duration>` by printing an error and continuing
  the loop, while this command validates `--every` at CLI-parse time
  (`cli::parse_duration_ms`, same as `session schedule add --every`) and
  is a hard usage error on a bad value -- correct, different contracts
  for the two call sites, not accidentally-duplicated logic.

  **Cancel primitive** (`protocol::Request::SessionInterrupt`,
  `harness session interrupt <id>`) -- see the "Needs a new subsystem"
  section's own narrowed "Steering" entry for exactly what this can and
  can't stop, and why REPL-integrated steering (typing `/interrupt` in
  the *same* REPL session that's waiting on a reply) still isn't
  attempted. What's real: `AgentSession` gains a `cancel_requested:
  Arc<AtomicBool>` (`cancel_flag()` returns a clone), cleared at the
  start of every `prompt_with_images_inner` call (so a flag left set
  after one turn can never leak into cancelling an unrelated later one --
  proven by a dedicated test) and checked once per tool-calling round,
  before that round's own work starts. The worker's own `Request::
  SessionInterrupt` handler deliberately never takes the session's own
  lock to set it -- captured as a separate `Arc` clone *before* the
  session is wrapped in its `Arc<Mutex<_>>`, specifically so setting it
  doesn't have to wait behind an in-flight prompt that already holds
  that lock for its whole duration, which would defeat the entire point.
  `Response::SessionInterruptAck` carries no `interrupted: bool` --
  stated honestly rather than a placeholder: the worker never checks
  whether a prompt was actually in flight before acking (that would mean
  taking the lock it's trying to avoid), so there's no truthful fact
  available to report beyond "the flag was set."

  Verified three ways: `tests/doctor.rs` (6 cases, real daemon spawn/
  reachability, `--fix`, malformed/valid/missing config files, JSON
  mode), `tests/heartbeat_cli.rs` (6 cases: no active goal, an active
  goal sent immediately, `--every` registering a real recurring
  schedule, a malformed `--every` rejected at parse time, interrupting
  an idle session still acks cleanly and leaves it usable, interrupting
  an unknown session id is a real error), and two new `session.rs` unit
  tests using a purpose-built `AlwaysToolCallsProvider` test double
  (`EchoProvider` never emits `ToolCalls` at all, so it can't drive a
  real multi-round loop to interrupt) -- one proving a flag set while
  round 1's provider call is "in flight" genuinely stops round 2 before
  it starts (the provider is called exactly once, not twice), one
  proving a flag left set *before* a new turn starts gets cleared rather
  than spuriously cancelling that unrelated turn.

  Still genuinely absent, stated rather than implied complete:
  `doctor`'s checks don't cover stale worker detection (already
  `session list`'s job) or a Python/`ipykernel` availability check;
  `session heartbeat`/`/heartbeat` still have no heartbeat-specific
  management surface (deliberate, see that entry's own "no separate
  list/pause/resume/clear surface needed" reasoning); the cancel
  primitive can't abort a model call already in flight to a real
  provider's HTTP endpoint, and REPL-integrated steering (see the
  narrowed "Steering" entry) is still not attempted.
- [x] **Bounded candidates batch 2: compaction fixes.** Three small,
  independent gaps closed together, all named directly in
  `CLAIMS_AUDIT.md`'s own `compaction.md` audit and recommendations
  checklist rather than newly discovered here.

  **Turn-boundary-aware compaction cut points.** `find_compaction_
  fold_count`'s backward token-budget walk previously treated every
  transcript entry identically regardless of role, so a cut could land
  immediately after a `Role::Assistant` tool-call-request entry (or
  between two of its own `Role::Tool` results), splitting a request from
  a response that only make sense read together -- `CLAIMS_AUDIT.md`
  had already confirmed this exact gap ("a cut can land immediately
  after a tool-call entry, separating it from its result"). New
  `adjust_fold_count_to_turn_boundary`: if the entry right at the naive
  cut index is a `Role::Tool` result, walk forward past every remaining
  `Role::Tool` entry to the next real `Role::User`/`Role::Assistant`
  boundary (or the end of the candidate list, if the tail past the cut
  turns out to be nothing but trailing tool results with no boundary
  left to land on -- folds everything rather than leaving a dangling
  split). Folding a few extra, already-over-budget entries is the honest
  tradeoff for never producing an orphaned tool result with no visible
  request behind it.

  **Persisted compaction `instructions`.** `compact_now` already
  received `instructions` as a parameter (`session compact <id>
  [instructions...]`/`/compact [instructions]`) and folded it into the
  summarization prompt, but never stored it anywhere -- confirmed
  directly against `CompactionState`'s own two-field shape before this
  entry, not assumed. `CompactionState` gains a third field,
  `instructions: Option<String>` (`#[serde(default)]`, so a `state.json`
  persisted before this field existed still deserializes as `None`
  rather than failing to load).

  **A `compaction.enabled` settings toggle.** `Settings` gains
  `compaction_enabled: Option<bool>` (`None`/`Some(true)`, the default,
  leaves automatic compaction on; only an explicit `Some(false)`
  suppresses it), following the exact env-var-then-`settings.json`-
  then-hardcoded-default precedence `compact_trigger_tokens`/
  `compact_keep_recent_tokens` already established (new
  `RUSTY_PRIME_AGENT_COMPACTION_ENABLED` env var). Gates only
  `AgentSession::maybe_compact` (the *automatic* per-round trigger) --
  manual compaction (`session compact`/`/compact`, i.e. `compact_now`
  called directly) never checks it, since a caller explicitly asking for
  compaction right now should still get it regardless of whether the
  automatic trigger is turned off. Before this field existed, the only
  way to suppress automatic compaction at all was to never configure a
  real `--model` in the first place.

  Verified with new `session.rs` unit tests, all in-process (no daemon,
  `AgentSession::create` called directly with a real `--model` string
  but still backed by `EchoProvider` -- the same technique
  `build_turns_prepends_the_context_file_as_a_system_turn` already uses,
  since a genuine model-backed round trip needs `rp-server`/a real
  backend and stays `#[ignore]`d elsewhere): two new
  `find_compaction_fold_count` cases (a naive cut landing between two
  tool results gets pushed forward to the next real boundary; a naive
  cut with no boundary left in the tail folds everything), one proving
  `compact_now` actually persists `instructions` when it folds something
  for real (forced via a `compact_keep_recent_tokens: 0` `settings.json`
  override, not the shared env var this file's own pre-existing
  compaction tests already guard with a mutex -- no shared global state
  to guard here), and two proving `compaction_enabled: false` genuinely
  suppresses automatic compaction even with `compact_trigger_tokens: 0`
  (every prompt would otherwise cross the threshold) while leaving it on
  by default. Plus two `settings.rs` unit tests for the new field itself.

  Not attempted, stated honestly: a CI-safe *integration* test (through
  the real daemon/CLI) for `compaction_enabled` specifically -- automatic
  compaction only ever runs at all once `state.model` is set, and a real
  `--model` needs a real `rp-server`-backed provider at the daemon/worker
  level (`worker::build_provider` never uses `EchoProvider` once a model
  string is given), the same real-model requirement `tests/compaction.rs`'s
  own module doc comment already states for testing compaction's actual
  effect end-to-end. The in-process `session.rs` unit tests above are the
  real, meaningful coverage instead.
- [x] **Bounded candidates batch 3: session/CLI convenience.** Four
  small, independent gaps, all named directly in `CLAIMS_AUDIT.md`'s own
  checklist.

  **Resume-by-partial-ID convenience.** `CLAIMS_AUDIT.md` scoped this
  narrowly ("a small prefix-match helper ahead of `session attach`/
  `session fork`"), but a `grep` for every place the daemon validates a
  `session_id` against disk found exactly seven real chokepoints:
  `resolve_worker` (used by ten handlers) plus six standalone handlers
  doing their own inline `state_file_path(...).exists()` check
  (`handle_schedule_add`/`handle_schedule_list`/`handle_schedule_cancel`/
  `handle_session_stop`/`handle_goal_show`/`handle_harness_show`). New
  `Daemon::resolve_session_id(&self, partial: &str) -> String`: returns
  `partial` unchanged if it already names a real session directly (the
  fast, common path -- checked first even though a full id being a
  literal prefix of some other session's own id is vanishingly unlikely
  given `new_session_id`'s nanosecond-timestamp-plus-pid shape), else
  resolves via `catalog::scan` to the one real session id starting with
  it if exactly one matches. Zero matches or more than one both fall
  through to returning `partial` itself unresolved -- every caller
  already has its own "unknown session" error path for that string, so
  an ambiguous prefix and a genuinely unknown one are reported
  identically, a real but bounded imprecision, not a distinction this
  slice attempts. Wired into all seven chokepoints, covering effectively
  every session-scoped daemon request rather than just the two named in
  the checklist, with zero protocol/CLI changes needed. New `tests/
  partial_session_id.rs`: an unambiguous prefix resolves the same as the
  full id (proven through `session goal show`/`session schedule list`/
  `session stop`), a full exact id still wins even when a second session
  exists that it could also be read as a prefix of, an ambiguous prefix
  (`"sess-"`, matching every session) reports the existing "unknown
  session" error rather than guessing, and a prefix matching nothing
  does the same.

  **`daemon shutdown --force`.** `Request::DaemonShutdown` gains a
  `force: bool` field. `force: false` (the default, unchanged) still
  sends `Request::WorkerShutdown` to every `Active` session's worker and
  waits for each ack before tearing down the daemon's own sockets.
  `force: true` skips that round trip entirely -- useful when a worker
  has wedged and its ack would otherwise hang the whole shutdown. Skipped
  workers are not killed, just not waited on: they keep running headless,
  exactly the same "supervisor gone, worker still alive" state this
  project's own crash recovery (`is_worker_alive`/`resolve_worker`)
  already has to and does handle for an actual crash, so nothing about a
  forced shutdown needed new recovery machinery. New `tests/
  session_lifecycle.rs` test proves both halves at once: after `daemon
  shutdown --force` on a session with a live worker, `state.json` still
  reads `"active"` (a graceful shutdown would have flipped it to
  `"stopped"` via `mark_stopped` before ever acking), and a fresh daemon
  on the same state root can still reach the orphaned worker rather than
  finding it crashed.

  **A `--no-session`/ephemeral-mode flag.** `-p`/`--print` gains
  `--no-session` (`cli::Command::Print::no_session`, a third strict
  leading flag alongside `--model`, composable in either order). Unlike
  ordinary `-p` (`client::print_once`, which creates a real, durably
  persisted session via the daemon), `--no-session` routes through new
  `client::print_ephemeral`: no daemon, no worker process, nothing left
  in `session list` afterward. It reuses the exact in-process path the
  embeddable SDK already established for a non-daemon caller
  (`AgentSession::create`, see `tests/embedded_session.rs`), pointed at a
  throwaway scratch directory under the OS temp dir that's removed again
  (along with any `rp-server` sidecar `--model` needed) once the prompt
  completes. Honest caveat: `AgentSession::create`'s durable `state.json`/
  `transcript.jsonl` writes have no in-memory-only mode to opt out of, so
  "no session persists" is enforced by cleaning up after the fact rather
  than by skipping the writes in the first place -- a real implementation
  difference from prime-agent's own in-memory-only ephemeral sessions,
  but not a user-visible one: nothing durable remains on disk by the time
  the command returns either way. `--model` still works in ephemeral mode
  (reuses `worker::build_provider`, now `pub(crate)`, plus an explicit
  `rp_server::ensure_running` call this path has to make itself since
  there's no daemon-side spawn-ordering to lean on); `--thinking`/
  `--tools`/`--runtime` are not exposed here, the same as ordinary `-p`
  already doesn't expose them. Three new `tests/session_lifecycle.rs`
  tests: a successful ephemeral prompt starts no daemon (`daemon.pid`
  never created) and leaves no `sessions/` entries behind; `--no-session`
  and `--model` compose in either order (proven via the same
  no-rp-server-available failure `session_new_with_model_fails_loudly_
  when_rp_server_is_unavailable` already exercises, still without ever
  starting a daemon).

  **Piped-stdin merging for `-p`.** New `merge_piped_stdin` (`lib.rs`):
  when `-p`'s own stdin is not a terminal (`std::io::IsTerminal`), its
  full contents are read and appended to the prompt text, blank-line
  separated, before either the `print_once` or `print_ephemeral` path
  runs -- no protocol change needed, matching `CLAIMS_AUDIT.md`'s own
  scoping. Fails open: an I/O error reading an already-non-terminal
  stdin is treated as "nothing piped" rather than failing the command,
  since the prompt text already given on the command line is still a
  valid prompt on its own. A real hazard caught before it could land in
  CI: `tests/common/mod.rs`'s `run()` helper spawns the compiled binary
  with stdin inherited from the test process by default, which is
  usually not a terminal either -- every existing `-p` test would have
  started blocking on stdin, waiting for an EOF nothing was ever going to
  send. Fixed by having `run()` explicitly set `Stdio::null()` (read as
  immediate empty EOF -- safe, not a hang) and adding two tests that
  build their own `Command` with a real `Stdio::piped()`: one proves a
  real piped file's contents get appended correctly, the other proves an
  explicitly-empty pipe (`common::run`'s new default) leaves the prompt
  text untouched rather than appending a stray blank line.
- [x] **Bounded candidates batch 4: auth + skills.** Four small,
  independent gaps closed together, all named directly in
  `CLAIMS_AUDIT.md`'s own checklist.

  **`auth.json`-vs-env-var precedence: confirmed, documented, not
  changed.** `prime-agent`'s own `providers.md` states `auth.json` wins
  over an env var; `auth::resolve_key`'s callers do the opposite (env var
  always wins, `auth.json` never even consulted once it's set). This was
  already a deliberate design choice, not a bug -- the fix here is purely
  the one-line note the checklist itself asked for, added to this
  project's own `auth.json` entry above, marking it a permanent,
  intentional divergence from that documented upstream contract rather
  than an open gap.

  **Env-var-name indirection in `auth::resolve_key`.** `prime-agent`'s
  own third `key` form (`{"key": "MY_KEY"}` meaning "read this env var,"
  not "use the literal string `MY_KEY`") was silently treated as a
  literal before this -- exactly the "ships a garbage literal credential"
  risk the checklist named. New `looks_like_env_var_name` (`^[A-Za-z_]
  [A-Za-z0-9_]*$`): a non-`!`-prefixed value with that shape is tried
  against `std::env::var` first, falling back to the literal only if no
  such env var is set. Deliberately conservative -- a real API key
  (`sk-...`, `sk-ant-...`, ...) almost never has that exact shape (the
  hyphen alone fails the check), and even a literal that happens to match
  only changes behavior if an env var of that exact name is *also* set,
  which was already true before this indirection existed (env var always
  wins over `auth.json` regardless, per the entry above).

  **Skill frontmatter validation beyond `description`.** `skills::
  discover` previously only ever read `description`; `frontmatter::parse`
  already returned the full field map, so `license`/`compatibility`
  (both purely informational -- nothing here enforces either) and
  `disable-model-invocation` (see below, a real behavioral flag) are now
  read too. A `name:` field that doesn't match the skill's own directory
  name produces a `Skill::warnings` entry (lenient -- warn, don't fail)
  rather than a discovery failure or a second "display name" this module
  reconciles: the directory name is what actually governs `import
  <name>`. `harness skill list` surfaces all of it -- `license`/
  `compatibility` lines, a tag when model-invocation is disabled, and any
  warnings.

  **`disable-model-invocation` + a `/skill:name [args]` explicit-invoke
  command.** Two pieces, as the checklist scoped them. First:
  `AgentSession::execute_python_tool_def_with_skills` now filters out
  any skill flagged `disable-model-invocation: true` from the
  `execute_python` tool's own description entirely -- the model never
  learns it exists (still on the kernel's `sys.path` either way;
  `worker::bootstrap_kernel` doesn't consult the flag, so `import` still
  works if the model somehow already knew the name). Second: `client::
  session_repl` gains `/skill:<name> [args...]`, the same REPL-command
  shape `/fork`/`/export` already have -- looks the name up via
  `skills::discover`, reports "no such skill" and sends nothing if it
  isn't found, otherwise composes an instruction ("use the `<name>`
  skill... ") and sends it as an ordinary prompt (same `send_prompt`
  shape `/heartbeat` already uses). This is deliberately *not* a direct
  Python-execution path -- the model still has to actually call
  `execute_python` itself for anything to run; `/skill:` only ever makes
  which skill to reach for explicit instead of leaving it to be noticed.
  Works for any skill, disabled or not -- the whole point of the split is
  that a human can always reach one on purpose even when the model can't
  stumble onto it on its own.

  Verified with new unit tests (`auth.rs`: `looks_like_env_var_name`'s
  own shape check, an env var actually resolving, and falling back to
  the literal when it's unset, guarded the same
  set/clear-then-drop-before-`.await` way `rp_server.rs`'s own
  `PROVIDER_ENV_GUARD` tests already are, to avoid an `await_holding_
  lock` clippy failure; `skills.rs`: `license`/`compatibility`/
  `disable-model-invocation` parsing including case-insensitivity, a
  matching `name:` producing no warning and a mismatched one producing
  exactly one; `session.rs`: `execute_python_tool_def_with_skills`
  advertises an ordinary skill, omits a disabled one, and still
  advertises the rest when only one of several is disabled) and new
  integration tests (`tests/skills.rs`: `skill list` renders the new
  fields/tag/warning; `tests/repl.rs`: `/skill:` sends the right
  instruction for a known skill with and without args, rejects an
  unknown one without sending anything, and still reaches a
  `disable-model-invocation` skill).
- [x] **Bounded candidates batch 5: harness notes feedback + idempotency.**
  Two small, independent gaps, both named directly in `CLAIMS_AUDIT.md`'s
  own "Candidate follow-ups" checklist.

  **Feed harness notes back into `build_turns`.** Durable, rollback-able
  supplemental state (`session harness add`/`rollback`, `/refine`) was
  previously invisible to the model on an ordinary turn -- the only place
  it ever surfaced was `/refine`'s own one-off review prompt
  (`client::build_refine_prompt`). New `format_harness_notes` renders
  `state.harness.notes` as one system turn, appended in `build_turns`
  right after the context-file and compaction-summary system turns
  (only when `notes` is non-empty, the same "gated on presence" shape
  those two already have). Design decision the checklist itself flagged
  as open, resolved: every note is included regardless of
  `HarnessNoteKind` (`Prompt`/`Memory`/`SkillDescription` alike) -- a
  caller who explicitly added supplemental state meant it to influence
  the session; the kind is a categorization label for display/filtering
  (`harness_add`'s CLI dispatch, `harness list`'s rendering), not a
  signal that some kinds should stay hidden from the model that's
  supposed to act on them. Verified with new `session.rs` unit tests:
  notes reach `build_turns` as one system turn containing every note's
  text; no notes means no extra turn at all; `format_harness_notes`
  itself lists every note with its kind. Not integration-tested through
  the CLI/daemon: `EchoProvider` only ever echoes the *last user* turn
  (see `provider::EchoProvider::respond`), so a black-box round trip
  can't observe *system*-turn content at all -- the in-process unit
  tests above are the real, meaningful coverage, the same test-boundary
  split `build_turns_prepends_the_context_file_as_a_system_turn`
  already established for the context-file system turn.

  **Idempotent replay protection for in-flight requests.** Bounded first
  slice, exactly as the checklist scoped it: an in-memory (not durable --
  lost on a worker crash/restart, a separately larger step), per-session
  dedup keyed by a caller-supplied request id, rejecting an exact
  duplicate rather than double-enqueuing. `Request::SessionPrompt` gains
  a `request_id: Option<String>` field (`#[serde(default)]`); every
  existing caller except the one new surface passes `None`, unaffected.
  New `AgentSession::prompt_with_images_and_request_id`: `None` is
  exactly `prompt_with_images` unchanged; `Some(id)` already present in
  a new `recent_request_ids: HashMap<String, TranscriptEntry>` field
  (paired with a `VecDeque<String>` for insertion order) returns the
  cached entry without prompting the provider again, otherwise prompts
  normally and remembers the result, evicting the oldest remembered id
  past `REQUEST_ID_CACHE_CAP` (64) so a long-lived session's memory use
  stays bounded. Exposed via `harness session prompt <id> --request-id
  <id> <text...>` -- the one client-facing surface that actually needs
  retry-safety; `/refine`/`/heartbeat`/`session_autonomous`/subagent
  messaging/etc. all still pass `None`, since none of them are a client
  retrying a dropped connection. Verified with new `session.rs` unit
  tests (a repeated id returns the identical cached entry without a
  second transcript turn, even when the retried text itself differs --
  proving the dedup check short-circuits before the provider is asked
  again, not that it happens to produce the same answer; distinct ids
  both enqueue; no id at all behaves exactly as before; the cache evicts
  its oldest entry once the cap is exceeded) and new `tests/
  idempotent_replay.rs` integration tests proving the same three
  properties end-to-end through the real daemon/worker.

## Needs a new subsystem

Architecturally significant `prime-agent` capabilities that would each
require a genuinely new subsystem this project has no analog of (most a
Python control environment, one an account/identity system) -- not
attempted here, and not silently implied by anything in
`ARCHITECTURE.md`'s "Known gaps" section:

- **Extensions, the rest of the surface.** Named in this bullet's
  original heading alongside "skills"/"themes" (both now done, see the
  medium-effort section above). An earlier pass here searched only *this
  project's own* docs, found no manifest/registration/capability spec,
  and concluded the whole surface was "genuinely undefined." Corrected by
  reading `prime-agent`'s own `docs/extensions.md` directly: a concrete
  spec exists (`extensions.md` documents a real manifest/registration
  format -- a default-export factory receiving an `ExtensionAPI`,
  `pi.registerTool()`/`registerCommand()`/`registerShortcut()`/
  `registerProvider()`/`on(event, handler)` across roughly 25 named
  lifecycle events). The bounded first slice that correction made
  scopable -- one blocking `pre_tool_call` hook plus one custom-command
  registration point, as a Python package matching this project's own
  established Python-via-kernel extensibility model rather than
  `prime-agent`'s literal JS/TS `ExtensionAPI` -- has since shipped; see
  the medium-effort section's own "Extensions: manifest/registration
  format + event-hook system" entry for the full design, the real
  sync-vs-async constraint found and worked around before it could cause
  a bug, and what got verified against a genuine kernel. What's named
  here now is only the remainder that entry explicitly left out:
  `registerTool`/`registerShortcut`/`registerProvider`, the other ~23
  lifecycle events, the dialog-based user-interaction surface, custom
  rendering, and extension-scoped persistent state -- each a large
  enough surface on its own (an LLM-callable-tool registry, a keybinding
  system, a provider registry, a modal dialog protocol with no rendering
  substrate to build it on) that picking any one of them up is its own
  future increment, not a continuation of this one.
- **"Steering," the REPL-integrated half.** Interrupting an already-in-
  flight prompt *from the same REPL session that's waiting on it*
  (typing while a reply is generating and having it actually cancel that
  reply) is the other half of `prime-agent`'s "steering vs. follow-up
  queuing" surface; the follow-up-queuing half is done (see the medium-
  effort section's own "Interactive TUI: steering vs. follow-up message
  queue" entry). A real cancellation primitive now exists (`Request::
  SessionInterrupt`, see the medium-effort section's own "Bounded
  candidates batch 1" entry) -- what's still missing is wiring it into
  `session_repl`'s own dispatch loop: today, any line typed while a
  prompt is in flight is queued (`Wake::Reader` unconditionally pushes
  onto `queue`, see that loop's own doc comment), never dispatched
  immediately, so there's no way to type `/interrupt` and have *this*
  REPL act on it right away rather than after the current reply lands
  -- it would need its own bypass of the queue, a REPL-loop change this
  batch didn't attempt. A caller in a *second* terminal/process can
  already interrupt a running turn today via `session interrupt <id>`
  (a genuinely different process, so no queue to bypass) -- see
  `protocol::Request::SessionInterrupt`'s own doc comment for exactly
  what that can and can't stop (a multi-round tool-calling turn stops
  before its next round; a model call already in flight to a real
  provider's HTTP endpoint can't be aborted mid-request without
  cooperative cancellation inside `ModelProvider::respond` itself, out
  of scope here too).
- **Mid-session `/model`/`/effort` switching, `/usage`, `/mcp
  login|logout`.** Surfaced (not previously named as their own bullet)
  while scoping the "full slash-command surface" entry above. Model and
  thinking level are fixed at `session new` time -- no `Request` variant
  or daemon/worker handler exists to mutate either on an already-running
  session, so `/model <name>`/`/effort <level>` each need a real
  protocol change, not just REPL wiring (the bare, read-only `/model`
  that lists configured providers is done -- see the entry above).
  `/usage` needs a token/cost data model that plainly doesn't exist: no
  `usage_tokens`/`token_usage`/`cost_usd` field anywhere in
  `protocol.rs`/`session.rs`, confirmed by direct search rather than
  inferred from a missing command. `/mcp login|logout` needs an
  MCP-server-scoped enable/disable primitive that also doesn't exist --
  MCP tool access is unconditional today, on or off only at the whole-
  session `--tools mcp` level.
- `/clone` (live-state duplication) stays out of scope for
  the reasons given above and in the medium-effort section's `session
  fork` entry -- a running kernel connection or MCP session dies with
  the source worker, same as any other session boundary, and that's not
  something a bounded increment can fix without a genuinely different
  worker-handoff mechanism.

  Investigated the rest of this bullet's original list piece by piece
  rather than leaving it one atomic blob -- `/file`, `/fork`, `/compact`,
  `/export`, and (once its own underlying data model landed) `/tree`
  all turned out to have honest, bounded REPL-only slices and are now
  done, see the medium-effort section's `session_repl`/`/tree`
  entries. `/share` stays out of scope: it needs somewhere to send the
  export *to* (a hosted paste/share destination), the same "nothing on
  the other end" shape `/login` has just below.
- **`/login`, the OAuth half.** `prime-agent`'s real `/login` is an
  in-session OAuth-style flow to Prime Intellect's own hosted account
  system. There's no Prime Intellect account for a local single-user
  harness to log into, and no other identity/account system this project
  has ever needed -- unlike the rest of this section, the missing piece
  isn't a Python control environment, it's an OAuth client plus somewhere
  real to send it, and there's nothing on the other end for this project
  to authenticate against. What *did* ship: `prime-agent`'s own
  [quickstart](https://github.com/PrimeIntellect-ai/prime-agent/blob/main/packages/coding-agent/docs/quickstart.md)
  presents `/login` and setting an API key beforehand
  (`export ANTHROPIC_API_KEY=...`) as two alternative paths to the same
  destination -- a configured model backend -- and an interactive wizard
  around that actual destination is a bounded, honest thing to build even
  without the OAuth client on top; see the medium-effort section's own
  "`/login`: interactive provider-setup wizard" entry for what that wizard
  does and doesn't cover.
- **ACP mode** (Agent Client Protocol). `prime-agent`'s `--mode acp`
  speaks the Agent Client Protocol (`agentclientprotocol.com`) to
  editor integrations. Re-investigated closely rather than taken on
  faith, because -- unlike `/login` just above it, or "Extensions"/
  "Themes" (see the medium-effort section's own entry on those) -- the
  `/login` reasoning this bullet used to give ("no editor plugin on the
  other end") turns out to be factually wrong: ACP is a real, openly
  specified JSON-RPC 2.0 protocol, and Zed is a real, currently-shipping
  ACP *client* already in people's hands. Implementing an ACP *server*
  here needs no backend this project would have to build or own, unlike
  `/login`'s missing Prime Intellect account system -- the "nothing on
  the other end" framing simply doesn't transfer.

  The honest reason to still hold off: this project doesn't yet have
  field-verified knowledge of ACP's exact wire shapes (`session/update`'s
  content union, `session/request_permission`'s parameters, `initialize`'s
  capability payload, confirmed message framing) the way MCP integration
  and the ZMTP client both required a direct probe of the real thing
  before any code was written for either. Writing an implementation
  against recalled-but-unverified JSON-RPC method signatures risks
  producing something that *claims* ACP compliance without actually
  having it -- worse than staying unimplemented. Structurally, this
  looks tractable once that spike is done: `client::session_rpc`
  (`--mode rpc`) is already this project's own precedent for "a headless
  JSON-in/JSON-out protocol reusing existing `Request`/`Response`/
  `SessionEvent` types rather than inventing a second schema," and
  `ProviderReply`'s non-streaming shape (a complete reply per turn, no
  partial deltas -- see the RPC mode entry above) looks like it can
  still emit a single, spec-legal `session/update` chunk per turn rather
  than needing this project's whole model-provider path restructured
  for true incremental streaming. Kept in this section for now (nothing
  here has actually verified a spike against the real protocol yet), but
  unlike `/login`'s genuinely missing backend, this one is a tractable
  near-term increment once that verification step happens, not a
  structurally missing subsystem -- still unimplemented today.
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
