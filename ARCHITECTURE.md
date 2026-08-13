# Architecture

Phase 1 skeleton for a cross-platform, daemon-backed agent harness:
`daemon start` launches a supervisor that owns a public Unix-domain (or
Windows `AF_UNIX`) socket, `session new`/`session prompt`/`session attach`
spawn and talk to per-session worker processes over private sockets of
their own, `session stop` gracefully tears one of them back down,
`-p`/`--print` is one-shot sugar over `session new` + `session prompt`
that starts a daemon transparently if none is running, and both
crash-recovery paths (supervisor restart, worker crash) rebuild
in-memory state from disk rather than trusting anything a still-running
process remembers.

This project deliberately mirrors one slice of
[`PrimeIntellect-ai/prime-agent`](https://github.com/PrimeIntellect-ai/prime-agent)'s
daemon/worker operational architecture without attempting to reimplement
that project itself -- see `PARITY.md` for what's mirrored, what's a
tractable near-term increment, and what's not yet implemented in this
project's current shape.

## Module map

| Module | Owns |
| --- | --- |
| `main` | Entrypoint, argv dispatch, `harden_inherited_stdio` |
| `cli` | Argument parsing for the public subcommands |
| `client` | The CLI-side half of every request: connects to `daemon.sock`, sends a `Request`, prints the `Response`/event stream. Also owns `session_autonomous`'s bounded continuation loop (`session autonomous`), `session_refine`'s Continual Harness review loop (`session refine`), `session_spawn`/`session_children`/`session_message`'s recursive-subagent composition, and `session_repl`'s minimal interactive loop (`session repl`, see `PARITY.md` for all four) -- pure client-side orchestration over existing `SessionNew`/`SessionList`/`SessionPrompt`/`SessionAttach`/`ScheduleAdd`/`GoalShow`/`GoalUpdate`/`HarnessShow`/`HarnessUpdate` requests, no daemon/worker changes needed beyond threading `parent_id` through `SessionNew`/`SessionState` |
| `daemon` | The supervisor process: binds the public socket, routes requests, recovers sessions on its own startup |
| `worker` | The per-session process: binds a private socket, owns one `AgentSession`, serves `SessionAttach`/`SessionPrompt`/`WorkerShutdown` |
| `session` | `AgentSession` -- transcript, `state.json` (including the persistent `goal: Option<GoalState>`, Continual Harness `harness: HarnessState`, recursive-subagent `parent_id: Option<String>`, `thinking: Option<String>` (`--thinking low/medium/high`), `tools: Option<String>` (`--tools read|mcp`), and `runtime: Option<String>` (`--runtime ipython`), see `PARITY.md`), the (fake) model provider, a lazily-connected `mcp_client::McpClient`, a real or no-op `tool_runtime::ToolRuntime`, the per-worker event broadcast. `prompt`'s own tool-calling loop (build the turn history, call the provider, execute any requested tools -- `tools::execute` for `--tools read`, `McpClient::call_tool` for `--tools mcp`, `tool_runtime::ToolRuntime::execute` for the `execute_python` tool `--runtime ipython` offers, independent of and combinable with `--tools` -- loop) lives here too, capped at 8 rounds. Also owns `NewSessionMeta`, the bundled creation-time metadata (`name`/`model`/`goal`/`parent_id`/`thinking`/`tools`/`runtime`) `AgentSession::create`/`worker::spawn` both take, keeping their own argument lists from growing every time a new `session new`-seedable field is added |
| `catalog` | `session list`'s directory scan, cross-checked against process liveness |
| `transport` | JSONL framing over `rusty_tokio::io::{UnixListener, UnixStream}`, plus `bind_with_retry`/`probe`/`wait_ready` |
| `procutil` | The narrow non-`rusty_tokio` OS surface -- see "Dependency Stack" below |
| `protocol` | The wire types (`Request`/`Response`/`SessionEvent`/`SessionState`) shared by every process this project spawns |
| `paths` | State-root layout (`daemon.sock`, `daemon.pid`, `sessions/<id>/{state.json,transcript.jsonl,worker.sock,schedules.json}`, `provider.{json,log}`) |
| `provider` | `ModelProvider` trait (`respond(turns, tools) -> ProviderReply`) + `EchoProvider` (the default, ignores `tools`); `RustyProviderModel`, a real backend opt-in per session via `session new --model provider/model` -- see `PARITY.md`. Also owns the turn/tool wire types (`ChatTurn`/`TurnRole`/`ToolDef`/`ProviderReply`) the real tool-calling loop uses, hand-rolled to `rp-server`'s own shape |
| `tools` | Built-in tools offered to a model via `session new --tools read` (`read_file`/`list_dir`, no path sandboxing), plus `execute_python_tool_def`'s `ToolDef` for `session new --runtime ipython` (its call is routed to `tool_runtime::ToolRuntime` by `AgentSession`, not to this module's own `execute`) -- see `PARITY.md`. A separate capability from `tool_runtime::ToolRuntime` below, not a second backend for it |
| `mcp_client` | Minimal MCP (Model Context Protocol) client against `rp-server`'s own built-in `/mcp` gateway (`session new --tools mcp`) -- `initialize`/`notifications/initialized` + `tools/list` + `tools/call` only, hand-rolled to the exact wire behavior a direct probe of a real sidecar confirmed (SSE-framed responses, chunked transfer encoding, `Mcp-Session-Id` session affinity) -- see `PARITY.md` |
| `sha256` | Hand-rolled SHA-256/HMAC-SHA256 (FIPS 180-4/RFC 2104), pinned against official test vectors -- exists solely for `ipython_runtime`'s Jupyter message signing, see below |
| `zmtp` | Hand-rolled ZMTP 3.0 client (NULL mechanism, DEALER/SUB socket semantics) on top of `rusty_tokio::io::TcpStream` -- exists solely for `ipython_runtime`, see below and "Dependency Stack" |
| `ipython_runtime` | `IpythonKernelRuntime`, the real `tool_runtime::ToolRuntime` backend (`session new --runtime ipython`) -- see below |
| `rp_server` | Sidecar lifecycle for `rusty_provider`'s `rp-server` (spawn, health-check, teardown) -- owned by the supervisor, read by workers. Also owns `known_providers`, the env-var-driven provider catalog `harness model list` reads (see `PARITY.md`) -- the same check `write_config` itself uses, so the two can't drift -- `fetch_model_catalog`, a direct `GET /v1/models` query against a sidecar `harness model list --detailed` starts itself (no daemon involved) -- and emits `[mcp] enabled = true` unconditionally in every generated `provider-config.toml`, the same "harmless with nothing configured" reasoning `[providers.ollama]` already gets |
| `http_client` | Minimal hand-rolled HTTP/1.1 client `RustyProviderModel`/`rp_server`/`mcp_client` use to talk to `rp-server`. Decodes `Transfer-Encoding: chunked` responses (`decode_chunked`) -- needed only for `mcp_client`'s SSE-framed calls, a no-op for every other caller, which had only ever seen `Content-Length` framing |
| `schedule` | Per-session `schedules.json` read/write/take-due -- fired by `daemon`'s own background poll loop, see `PARITY.md` |
| `prompt_template` | Discovery (`paths::global_prompts_dir`/`project_prompts_dir`) and `$1`/`$@`/`${@:N}`-style positional-argument expansion for `prompt-template list/render` and `session prompt-template` -- see `PARITY.md`. Frontmatter parsing itself lives in `frontmatter`, shared with `skills` |
| `frontmatter` | Hand-rolled `---\nkey: value\n---\n<body>` parsing shared by `prompt_template` and `skills` -- both only ever read a couple of flat string keys |
| `skills` | Discovery of real, importable Python packages for `session new --runtime ipython` (`paths::global_skills_dir`, `SKILL.md` frontmatter) -- see below and `PARITY.md` |
| `tool_runtime` | `ToolRuntime` trait boundary -- see below |
| `settings` | `<state_dir>/settings.json` read/parse (`load`) -- see below |
| `auth` | `<state_dir>/auth.json` read/parse plus `!command` key resolution (`load`/`resolve_key`) -- see below |
| `providers` | `<state_dir>/providers.json` read/parse for custom/arbitrary OpenAI-compatible provider registration (`load`) -- see below |
| `error` | `HarnessError`/`Context`, the one error type every module maps into |

## Dependency stack

```
rusty_prime_agent (this project)
        |
        v
   rusty_tokio  --  async runtime AND process/signal/local-IPC layer,
        |            genuinely cross-platform (Linux/macOS/BSD/Windows)
        v
    rustils     --  only where rusty_tokio doesn't cover something
```

**`rusty_provider`'s `rp-server` is a spawned process, not a `Cargo.toml`
dependency.** `session new --model provider/model`'s `RustyProviderModel`
(`PARITY.md`) talks to it purely over HTTP (`http_client.rs`) after the
supervisor spawns it (`rp_server.rs`) -- it is never linked into this
binary, since it's built on real `tokio` rather than `rusty_tokio`, and
embedding it as a library would mean two async runtimes in one process.
This keeps the dependency stack below accurate for everything this
project actually links against; `rp-server` is an external service this
project happens to manage the lifecycle of, the same category as Ollama
itself, not a fourth entry in the stack.

This is otherwise a **two-hop** dependency shape, not the direct, broader
harness-to-`rustils` dependency the original Phase 1 brief assumed.
`rusty_tokio::io::{UnixListener, UnixStream}` and
`rusty_tokio::process::Command`/`Child` are now genuinely cross-platform
(Linux/macOS/BSD/Windows alike, per `rusty_tokio`'s own
Windows-process-signal-IPC work), so every socket and every process this
project spawns goes through `rusty_tokio` directly. This project depends
on `rustils` (via `platform::error`, transitively) not at all in its own
`Cargo.toml` -- `rusty_tokio` already sits on top of it internally for
exactly the primitives this project would otherwise have wrapped by hand.

**No `zeromq` crate, despite `ipython_runtime` needing a real ZMTP
client.** The plan for that increment (`PARITY.md`'s RLM programming
model entry) originally named `zeromq` (pure-Rust ZMTP) as an explicit,
justified dependency exception. Its actual `Cargo.toml` (checked via
`cargo tree` against a throwaway scratch crate before committing to it)
depends directly on real `tokio` unconditionally -- exactly the "two
async runtimes competing in one process" shape this section's own
`rp-server` note explains this project avoids by never linking that
sidecar in as a library either. `zmtp.rs`/`sha256.rs` hand-roll the wire
protocol and its HMAC-SHA256 signing scheme directly on `rusty_tokio::
io::TcpStream` instead, so this dependency stack stays exactly the same
two-hop shape below -- see `PARITY.md`'s RLM entry for the full story
(what the plan assumed, what `cargo tree` actually showed, and why
hand-rolling was judged the better fit for this project's existing
"hand-roll wire protocols" posture, not a compromise).

**No bridging layer.** An earlier revision of `transport.rs` bridged
`rustils`' blocking `Net` trait through `spawn_blocking` by hand, because
`rusty_tokio` had no native Windows `AF_UNIX` support yet. That bridge was
removed once `rusty_tokio`'s own Windows `AF_UNIX` support landed (the
"rusty_tokio-Windows-gap addendum") -- `transport.rs` now builds directly
on `rusty_tokio::io::{UnixListener, UnixStream}`, genuinely non-blocking
on every platform this project targets (Linux/macOS/BSD via epoll/kqueue,
Windows via IOCP+AFD-poll).

**Pinned `rusty_tokio` rev bumped past an epoll busy-spin fix
(`baileyrd/rusty_tokio#265`).** Every socket this process holds open --
worker/daemon private sockets very much included -- registers with the
Linux reactor; a level-triggered registration bug there meant the
reactor thread pegged a full CPU core in a tight spin for as long as
any such socket sat open, not just the one that surfaced it
(`rp_server`'s HTTP client). See `PARITY.md`'s own entry on this for
the full story -- it's a `rusty_tokio`-side fix, not this project's own
code, but worth flagging here since every long-lived connection this
project holds (worker sockets in particular) was affected.

**`procutil.rs`'s narrow remaining gap.** Two things `rusty_tokio::process`
doesn't cover, both because they're about a pid or a spawn-time detail
outside what a process-spawning wrapper naturally exposes:

- **Liveness of an arbitrary pid.** `rusty_tokio::process::Child::wait`/
  `try_wait` need an owned `Child` this process itself spawned.
  `catalog::effective_status`'s crash check needs to probe a
  `worker_pid` recorded in `state.json`, possibly by a *previous* worker
  process, long after that spawn call returned (or after this
  supervisor itself restarted). `procutil::is_alive` goes straight to
  `kill(pid, 0)` (Unix) / `OpenProcess` + `GetExitCodeProcess` (Windows)
  for that, matching `rustils`' own `Spawner::is_alive` shape (RFC v2
  decision, `docs/decision-request-detach-liveness.md` in that repo) but
  without pulling in a `rustils` dependency of this project's own for
  one function.
- **Real session-leader detach.** `rusty_tokio::process::Command` exposes
  `process_group` (`setpgid`-before-exec) but no `setsid`/"new session"
  builtin. `procutil::prepare_detached` reaches for
  `std::os::unix::process::CommandExt::pre_exec` (a raw `libc::setsid()`
  call) on Unix and
  `std::os::windows::process::CommandExt::creation_flags`
  (`CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS`) on Windows, both
  through `rusty_tokio::process::Command::as_std_mut`'s escape hatch --
  the same flags `rustils`' own `Command::detach()` sets internally, just
  reached directly rather than through a `rustils` dependency.

Everything else -- spawn, piped stdio, non-blocking `wait`, single-pid
kill -- goes through `rusty_tokio::process::Command`/`Child` directly.

## `ToolRuntime` trait boundary

The one deliberate ports-and-adapters seam this project leaves open, per
the original Phase 1 brief's Architecture Constraints ("design the trait
boundary [to the model-facing IPython kernel] ... but implement only a
no-op/mock runtime") -- now backed by a real implementation, see below.

`tool_runtime::ToolRuntime` (object-safe, `Box<dyn ToolRuntime>`,
boxed-future async methods rather than a real `async fn` in the trait --
see that module's own doc comment for why) is the host-side handle to a
model-facing code execution environment: `start`/`execute`/`shutdown`.
Every session still gets `NoopToolRuntime` by default (never spawns a
process, never executes anything) unless it opts in via `session new
--runtime ipython`, in which case it gets `ipython_runtime::
IpythonKernelRuntime` -- a real, spawned Jupyter/IPython kernel
subprocess (`python3 -m ipykernel_launcher`), driven over a hand-rolled
ZMTP 3.0 client (`zmtp.rs`, NULL mechanism, `shell`/`iopub` sockets only)
with HMAC-SHA256-signed messages (`sha256.rs`) -- see `PARITY.md`'s RLM
programming model entry for the full story (what was originally planned,
what a `zeromq` crate `cargo tree` check ruled out, and what the
hand-rolled alternative actually covers/still defers). `worker::
build_tool_runtime` is the one place this selection happens, based on
`WorkerArgs::runtime` -- picked before `AgentSession::create`/`recover`
even run, the same `--thinking`-shaped "always supplied by the daemon at
spawn time" thread-through `WorkerArgs::thinking` already established,
not `--tools`'/`--goal`'s "New-only" one, since a `ToolRuntime`
implementation has to exist before there's any persisted `state.json` to
read a resumed session's choice back from.

Every *other* subsystem in this crate calls `rusty_tokio` (and, through
it, `rustils`) directly rather than wrapping them behind a speculative
trait -- `transport.rs` calls `rusty_tokio::io` directly, `worker::spawn`
calls `rusty_tokio::process::Command` directly. `ToolRuntime` remains the
one boundary with a genuinely swappable second backend, and its
boxed-future shape is exactly why `IpythonKernelRuntime::start`/`execute`
being real, `.await`-heavy subprocess-and-socket I/O needed no trait
redesign to land.

**Not the same thing as the real tool-calling loop** (`provider`/`tools`
modules, `session new --tools read|mcp`, see `PARITY.md`): that's
`rp-server` answering `ChatRequest.tools`/`tool_calls` over the same HTTP
connection `RustyProviderModel` already uses, independent of any kernel
process. `ToolRuntime` stays the specific boundary to a model-facing
*code execution environment* -- but the two *do* meet at exactly one
point: `session new --runtime ipython` offers an `execute_python` tool
(`tools::execute_python_tool_def`) through that same tool-calling loop,
and `AgentSession::execute_tool_call` routes a call to it straight to
`self.tool_runtime.execute(code)` instead of `tools::execute`/
`McpClient::call_tool`. This is the natural way to make a real kernel
*reachable* from an ordinary prompt/response turn without inventing a
second turn-loop mechanism alongside the one Increment 3 already built --
the tool-calling loop is "how a turn asks for something to happen
outside the model," and the kernel is one more thing that can happen,
not a different kind of turn.

## Skills packaging

Real, `import`-able Python packages for `session new --runtime ipython`
(`prime-agent skills.md`'s Python-package half -- `prompt_template.rs`
already covers the plain-text half) -- see `PARITY.md` for the full
story. A skill is a directory under `paths::global_skills_dir`
(`<state_dir>/skills/<name>/`): a `SKILL.md` (`description` frontmatter,
parsed by the shared `frontmatter.rs` module) alongside a real Python
package (`__init__.py`). `skills::discover` is a pure filesystem scan --
it never inspects the Python itself; a broken package surfaces as an
ordinary `ImportError` the model sees when it actually tries to `import`
it, the same "let the callee reject malformed input" stance `tools::
execute` already takes.

Global tier only -- deliberately no project-local tier the way
`prompt_template::discover` has one, since the one place skill *loading*
needs to run (`worker::bootstrap_kernel`, right after `tool_runtime.
start()` succeeds: one `execute_request` puts the skills directory on
the kernel's own `sys.path`) is the worker process, which has no access
to the CLI caller's own cwd the way `prompt_template::discover`'s
always-client-side callers do. `session::enabled_tool_defs` appends
every discovered skill's name/description to the `execute_python` tool's
own description, so the model knows what it can `import` without being
told in the prompt. `harness skill list` is the human-facing view, same
shape as `prompt-template list`.

## `/heartbeat` and `rlm_heartbeat()`

Two manual entry points into the same re-entry mechanism `schedule.rs`
already covers server-side -- see `PARITY.md` for the full story
(including a real concurrent-write hazard found by reading `schedule.rs`
directly, not assumed: it has exactly one safe writer, the daemon's own
background firing loop). `worker::bootstrap_kernel` (the same function
that installs skills, above) always defines `rlm_heartbeat()` in the
kernel's globals when `--runtime ipython`; calling it prints a fixed
marker (`session::HEARTBEAT_MARKER`) that `session::
execute_python_tool_call` watches every call's stdout for, strips out of
what the model sees, and dispatches to `session::trigger_heartbeat` --
which, with an `Active` goal, opens an ordinary client connection to this
process's own `daemon.sock` and sends `Request::ScheduleAdd` (the same
wire path every `client.rs` function already uses to talk to the
daemon -- the first time a *worker* process is the one dialing in,
instead of only ever being dialed into) rather than writing
`schedules.json` directly or reentrantly calling `self.prompt()` (which
would append a second prompt's turns out of order relative to the
in-flight call's own pending tool-result turn). `session_repl`'s
`/heartbeat` needs none of that indirection -- a fresh top-level REPL
action, not nested inside anything, so it fetches the goal
(`client::fetch_goal`, already shared with `goal_show`/
`session_autonomous`) and sends the continuation prompt immediately.

Both accept an optional duration string for a repeating variant
(`rlm_heartbeat(every="10m")`, `/heartbeat every 10m`) --
`ScheduleKind::Every { interval_ms }` instead of `ScheduleKind::Once` on
the same `Request::ScheduleAdd`, parsed by `cli::parse_duration_ms`
(made `pub(crate)`, reused rather than re-implemented). The kernel side
has no channel back to this process besides stdout, so the duration
string rides along on the marker's own printed line
(`session::extract_heartbeat_marker` splits it back out);
`session_repl`'s `/heartbeat every` registers a real schedule
(`client::schedule_add`) instead of sending a prompt immediately, since
a repeating heartbeat is a standing re-entry, not a one-time action.

## Automatic context compaction

Parity with `prime-agent compaction.md` -- see `PARITY.md` for the full
story, including what's approximate about it (no real token accounting,
no per-model context-window catalog) and the real-model test that
verifies it. `session::AgentSession::maybe_compact` is checked once per
round of `prompt`'s own tool-calling loop; when a session has a `model`
set and its estimated turn-token total crosses
`compact_trigger_tokens()` (a fixed default, overridable via
`RUSTY_PRIME_AGENT_COMPACT_TRIGGER_TOKENS`), `compact_now` asks
`self.provider` itself to produce an updated running summary of
everything past the last compacted boundary except the most recent
`compact_keep_recent_tokens()` worth of turns, and records the result in
`SessionState::compaction` (`CompactionState`). `build_turns` is the only
place this is visible to the provider: it replaces every turn at or
before the compacted boundary with one synthetic system turn carrying
the summary. `transcript.jsonl`/`self.transcript` are never rewritten or
truncated -- `session.rs`'s own "full JSONL replay, single source of
truth" decision stays exactly as true as it was before this feature
existed; a `Role::System` entry documenting the compaction is appended
to the durable transcript too, so `session attach`/`session repl` show
it happened without needing to inspect `state.json` directly.
`EchoProvider` sessions (no `model`) never trigger this automatically
and treat a manual `session compact`/`session_repl`'s `/compact` as a
plain no-op -- there's no real model to summarize with.

## RPC mode

Parity with `prime-agent --mode rpc` -- see `PARITY.md` for the full
story, including two real concurrency races (one found in manual
testing, one caught by CI on macOS) and how each was closed.
`client::session_rpc` (`session rpc <id>`)
reuses the wire protocol's own `Request`/`Response`/`SessionEvent` types
directly as its command/event vocabulary rather than inventing a second
schema the way `prime-agent`'s own ~30-command RPC surface is. Two
lanes share one stdout, serialized through a `rusty_tokio::
sync::Mutex<()>`: the initial `SessionAttach` round trip runs
synchronously first (so the snapshot line is always first, not raced
against a background task that might not have started yet), then the
same connection moves into a background task that keeps streaming
`SessionEvent`s for as long as the process lives; the foreground loop
reads one stdin line at a time (each read its own `spawn_blocking` call,
so the loop stays `.await`-able between reads), dispatches it as a
`Request` over an ordinary one-shot connection, and prints the
`Response`. Ends at stdin EOF (after a bounded 300ms grace sleep so the
background lane gets one real chance to drain events the last command
already produced -- see `PARITY.md` for why this is closing this
process's own scheduling latency, not waiting on the provider), same
convention `session_repl` uses.

## Context files (`AGENTS.md`/`CLAUDE.md`)

Parity with `prime-agent`'s own auto-loaded context files -- see
`PARITY.md` for the full story, including a scoping correction (a
project-local tier would need the same cwd-visibility machinery
`skills::discover` doesn't have, but the global tier this project
actually built doesn't need it at all). `session::read_context_file`
checks `<state_dir>/AGENTS.md` then `<state_dir>/CLAUDE.md`, read fresh
on every `build_turns` call -- no persisted state, no caching, same "an
edit takes effect on the next prompt" property `skills`/
`prompt_template` discovery already have. Its content becomes an
even-earlier system turn than the compaction summary's own; like that
injection, `transcript.jsonl` is never touched, so this is
provider-facing only.

## `settings.json`

Parity with `prime-agent`'s own persistent config layer -- see
`PARITY.md` for the full story. `settings::load(state_root)` reads and
parses `<state_dir>/settings.json`, returning an all-`None` `Settings`
for a missing file, an unreadable one, or one that isn't valid JSON --
never a hard error, the same permissive stance the compaction
thresholds' own env-var overrides already take for an unparseable
value. Scoped today to exactly the two fields those overrides already
had (`compact_trigger_tokens`/`compact_keep_recent_tokens`);
`session::compact_trigger_tokens`/`compact_keep_recent_tokens` now check,
in order, the env var, then `settings::load(&self.state_root)`, then the
hardcoded default -- one more fallback tier under what was already there,
not a new precedence model. Global only, same cwd-visibility reason
`skills::discover`/`read_context_file` are.

## `auth.json`

Parity with `prime-agent`'s own `auth.json` -- see `PARITY.md` for the
full story. `auth::load(state_root)` reads `<state_dir>/auth.json` into
a `{provider name -> {"key": ...}}` map, same permissive-parse stance
`settings::load` already takes. `auth::resolve_key` turns one entry's
`key` into a real string: a literal value as-is, or (a `!`-prefixed
value) the trimmed stdout of running the rest as a shell command
(`sh -c`/`cmd /C`, bounded by a 10s timeout) -- the same trust model
`session_autonomous --quality-gate` already accepts, no sandboxing,
because there is exactly one local caller.

`rp_server::resolve_auth_env(state_root)` is the only caller: for every
provider `rp_server::all_providers` returns (see the next section) whose
env var isn't already set in the daemon's own environment, it resolves
an `auth.json` entry (if any) into an `(api_key_env, key)` pair.
`ensure_running` hands each pair straight to the spawned `rp-server`
child via `Command::env`, never `std::env::set_var`-ing the daemon's own
process -- an `auth.json` edit takes effect on the next sidecar spawn
without a daemon restart. `write_config` activates a `[providers.*]`
block when either the env var or a resolved `auth.json` entry configures
it; `known_providers` (`harness model list`) only checks *presence* of
an `auth.json` entry, never resolving a `!command`, so a plain listing
can't run an arbitrary command as a side effect.

## `providers.json` (custom provider registration)

Parity with letting a session point at any self-hosted OpenAI-compatible
endpoint (a vLLM server, LM Studio, a company-internal proxy) -- see
`PARITY.md` for the full story, including the confirmation against
`rusty_provider`'s own real router source that an arbitrary provider
*name* is exactly the mechanism it already supports. `providers::
load(state_root)` reads `<state_dir>/providers.json` into a `{provider
name -> {base_url, kind}}` map (`kind` optional, defaults to
`"openai"`), same permissive-parse stance every other config file in
this project takes.

`rp_server::all_providers(state_root)` merges this with the hardcoded
`OPTIONAL_PROVIDERS` const into the one list `write_config`/
`known_providers`/`resolve_auth_env` all iterate now, instead of the
bare const directly -- a custom entry reusing a reserved name (a
built-in provider's own name, or `"ollama"`) is silently dropped, so it
can never collide into a duplicate `[providers.*]` TOML table
`rp-server` would reject. A registered provider's env var is derived as
`<NAME>_API_KEY` (`rp_server::custom_provider_api_key_env`,
non-alphanumerics folded to `_`) -- `provider.rs`/`cli.rs`/`client.rs`
needed no changes at all, since `--model <name>/<model>` was already an
opaque string forwarded straight through to `rp-server`, and `auth.rs`
needed no changes either, since it was already a plain name-keyed map
that a custom name slots into for free.

## Known gaps

Reflecting two addenda from prior work on this project, both worth
stating explicitly rather than leaving implicit -- neither blocks
anything Phase 1 actually needs, and neither is this project's own gap to
close:

- **`rustils`' `detach()` refuses `NewGroup` uniformly on both
  platforms**, not Linux-permissive as the original Phase 1 brief
  assumed. The brief's premise was that `setsid()`+`setpgid(0, 0)` is
  POSIX-specified-harmless self-targeting, so Linux could allow
  `Command::detach()` to compose with `GroupSpec::NewGroup` even though
  Windows can't (a kill-on-close Job Object would defeat `detach`'s own
  "survives a crash" guarantee). A real `posix_spawn` call proved that
  premise wrong: Linux's `setpgid(2)` forbids changing a session leader's
  process group ID at all, even a self-targeting no-op, and `setsid`
  always makes the caller a session leader first -- so the two flags can
  never coexist in one `posix_spawn` call, on either platform, for two
  unrelated reasons. `rustils` refuses the combination
  (`ErrorKind::Unsupported`) uniformly now; see that repo's
  `docs/decision-request-detach-liveness.md` for the full record. Not
  this project's problem in practice: `procutil::prepare_detached` never
  asked for a new process group at spawn time in the first place (Phase 1
  worker spawns no child processes of its own to need a *tree*-kill for
  -- see `worker::spawn`'s own doc comment), so there was nothing here to
  adjust when this landed upstream.
- **`rusty_tokio` still has real Windows gaps in `UnixDatagram`/socket
  pairs/raw `UnixSocket`.** `UnixDatagram` has no Windows arm at all
  (Windows `AF_UNIX` datagram sockets exist at the OS level, but neither
  `rustils` nor `rusty_tokio` has wired one up yet). `UnixStream::pair`
  and the pre-bind `UnixSocket` builder stay `#[cfg(unix)]`-only -- no
  anonymous `AF_UNIX` pair primitive on Windows at the OS level, and
  `platform_windows` has no owned-socket-adoption path yet for the
  builder to construct one from a raw handle. `UnixListener`/`UnixStream`
  get `AsRawSocket` on Windows but not `AsSocket`/`FromRawSocket`/
  `IntoRawSocket` (no ownership-transfer interop from `rustils` yet
  either). None of this is something this project needs -- every socket
  it opens is a named, bound/connected `AF_UNIX` stream (`daemon.sock`,
  each session's `worker.sock`), never a datagram socket, an anonymous
  pair, or a raw pre-bind builder -- but it's worth naming so a future
  change to this project's own IPC shape doesn't discover the gap by
  surprise.

## IPC model

Two socket tiers, both JSONL-framed (`transport::LineStream`,
newline-delimited `Request`/`Response`/`SessionEvent`) over
`rusty_tokio::io::{UnixListener, UnixStream}`:

- **Public** (`daemon.sock`): CLI -> supervisor. One request per
  connection for everything except `session attach`, which stays open
  and streams `SessionEvent`s until `SessionEnded` or the client
  disconnects.
- **Private** (`sessions/<id>/worker.sock`): supervisor -> worker,
  forwarding an attach/prompt on the client's behalf. The supervisor
  never lets a client connect to a worker's private socket directly --
  every worker interaction is relayed, which is what lets the supervisor
  restart/recover a worker transparently without the client having to
  know a new process now owns the session.

`transport::Listener::bind_with_retry` retries a bind on `AddrInUse` for
a short window -- see that function's own doc comment, and
`docs/decision-request-af-unix-stale-reclaim-race.md` in the `rustils`
repo, for the specific Windows dead-listener-reclaim race this exists
alongside (fixed upstream in `rustils`' own `unix_listen`, not papered
over here; `bind_with_retry`'s retry is about a real, load-bearing
`AddrInUse` transient, not a substitute for that fix).

## Recovery model

Two independent recovery paths, deliberately not unified into one:

- **Supervisor restart** (`daemon::Supervisor::recover_on_startup`): a
  fresh supervisor scans the catalog for `Active` sessions and, for each,
  checks whether the recorded `worker_pid` is still alive. If so, it
  *adopts* the still-running worker -- no respawn, no recovery marker,
  the worker never knew its supervisor died. `tests/
  supervisor_restart_recovery.rs` is the test for this path, and its own
  repro is exactly what motivated the `rustils` stale-reclaim fix above:
  the new supervisor rebinding `daemon.sock` right after the old one was
  force-killed.
- **Worker crash** (`daemon::Supervisor::ensure_worker_running`, invoked
  on demand by `SessionAttach`/`SessionPrompt`): when the recorded
  `worker_pid` is dead, a fresh worker is spawned in `Recover` mode,
  which full-replays `transcript.jsonl` into memory, bumps the
  generation, and stashes a one-time `RecoveryMarker` event
  (`AgentSession::pending_recovery_marker`) delivered to whichever
  attach happens first -- see that field's own doc comment for why it's
  stashed rather than broadcast directly (a crash-recovered worker emits
  it before its private socket is even bound, before any client could
  possibly have subscribed yet). `tests/worker_crash_recovery.rs` covers
  both trigger paths (a mutating `session prompt` and a read-only
  `session attach`), and both a `spawn`+immediately-`drop`ped `Child`
  handle (which zombies a dead worker under a still-running supervisor on
  Unix, since `setsid` alone doesn't reparent it away from this process)
  and the recovery-marker delivery race above were real bugs this
  session found and fixed while rerunning this suite for the first time
  since the `rusty_tokio` migration -- see `worker::spawn`'s and
  `AgentSession::emit_recovery_marker`'s doc comments for the specifics.
