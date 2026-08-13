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
| `lib` | Crate root (`rusty_prime_agent`): `pub async fn run(args)` (the full subcommand-dispatch match, moved out of `main.rs`), the `pub mod`/`pub use` embedding surface -- see "Embeddable SDK" below |
| `main` | Thin bin entrypoint: `harden_inherited_stdio`, argv collection, calling `rusty_prime_agent::run`, and the exit-code mapping -- the process-level concerns that are genuinely bin-only |
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

## Embeddable SDK

Parity with `prime-agent`'s own `createAgentSession()`/`defineTool()`
programmatic API -- see `PARITY.md` for the full design story, including
why this is two deliberately honest embedding layers rather than an
assumed in-process agent loop. `Cargo.toml` has both a `[lib] name =
"rusty_prime_agent"` and the existing `[[bin]] name = "harness"`; the
bin depends on the lib as an ordinary same-package target, not a
duplicated copy of any source.

`lib.rs`'s public surface is deliberately narrow: `pub mod session`
(`AgentSession` and friends -- see "In-process, no daemon at all" below),
`pub mod provider`/`pub mod tool_runtime` (the two traits an embedder
implements to plug in a custom model/tool backend), `pub mod protocol`
(the wire types), `pub mod error`/`pub mod paths`, and `pub use
client::dispatch_one_shot` (see "Drive a running daemon" below).
Everything else (`daemon`/`worker`/`client`'s own CLI-output functions/
`ipython_runtime`/`zmtp`/...) stays a plain `mod`, still fully usable
*within* the crate by `run()` exactly as before -- module privacy in
Rust only gates *external* crates, so none of that internal wiring
needed to change at all, only which few `mod` declarations became `pub
mod`.

**In-process, no daemon at all.** `session::AgentSession::create(state_root,
session_id, NewSessionMeta, Box<dyn ModelProvider>, Box<dyn
ToolRuntime>)` is exactly what `session.rs`'s own unit tests already
constructed directly -- a real, driveable session with no daemon/
worker/socket machinery in the loop. Not a pure in-memory session,
though: `create` still does real filesystem I/O (`state.json`/
`transcript.jsonl` persistence) under the caller-supplied `state_root`,
the same durability a daemon-backed session gets. `ModelProvider`/
`ToolRuntime` were already plain `pub trait`s, already object-safe and
`Send + Sync` (boxed-future methods specifically so an external async
impl doesn't need `async-trait`), already how `AgentSession` stores them
internally -- implementing either yourself is this project's answer to
`defineTool()`, no separate tool-registration API needed.

**Drive a running daemon.** `dispatch_one_shot(state_root, Request) ->
Result<Response>` sends one typed request over an already-running
daemon's socket and returns a typed response -- the same connect-send-
receive primitive every `client::session_*`/`client::daemon_*` function
already built on internally, but returning data instead of `println!`/
`print_json`-ing straight to this process's own stdout the way those
CLI-rendering functions do. That's exactly why only this one function
is promoted to `pub`, not the whole `client` module.

Explicitly out of scope for this increment: any semver/stability
guarantee on the public surface (`publish = false`/`0.1.0` stay),
docs.rs-quality rustdoc coverage beyond what's here, and exposing
`daemon`/`worker`/`ipython_runtime`/`zmtp` -- nothing in either
embedding layer needs them directly. Verified by `tests/
embedded_session.rs` (layer one) and `tests/dispatch_one_shot.rs` (layer
two) -- the first tests in this project not shaped as `std::process::
Command::new(common::bin())`, since every `tests/*.rs` file compiles as
its own crate linking the lib the same way a real external embedder
would.

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
ZMTP 3.0 client (`zmtp.rs`, NULL mechanism, `shell`/`iopub`/`control`
sockets) with HMAC-SHA256-signed messages (`sha256.rs`) -- see `PARITY.md`'s RLM
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

Each of the three ZMTP sockets connects (and completes the ZMTP
handshake) via a shared `connect_with_retry` helper -- not a bare
`ZmtpSocket::connect` call -- because the handshake's own reads have no
timeout, and a kernel that's merely slow to answer one socket (not
refusing the connection) can hang a single attempt indefinitely; direct
testing surfaced this as an intermittent multi-minute real-kernel test
hang before the fix. `ToolRuntime::execute` can now also pause mid-cell
with `ExecutionOutcome.pending_host_request` when kernel code opens a
Jupyter comm targeting `"host.request"` over `control` -- `resume_execute`
delivers the caller's reply and continues draining. `AgentSession::
handle_host_request` (`&mut self`, not `&self` -- `goal.*`/`compact.now`
mutate `self.state` directly) dispatches eight request kinds so far:
`"rlm.run"` (`handle_rlm_run`, admits a child session through the same
`SessionNew`/`ScheduleAdd` daemon round trip `session spawn` already
uses, just issued from inside the worker process), `"rlm.list_subagents"`/
`"rlm.delete_subagent"` (`handle_list_subagents`/`handle_delete_subagent`,
a `Request::SessionList`-filtered-by-`parent_id` read and a
`Request::SessionStop` write, respectively -- no separate registry data
structure exists; a child's own persisted `parent_id` already is the
durable record `list_subagents()` reads back and `delete_subagent()`
validates against before stopping a session), `"goal.get"`/`"goal.create"`/
`"goal.complete"`/`"compact.now"` (all operate on *this same session's
own state*, so unlike every `rlm.*`/`agent_message.*` kind they need no
daemon round trip at all -- they call `update_goal`/`compact_now`
directly, the exact same in-process methods `Request::GoalUpdate`/
`Request::SessionCompact` already call from the worker's own private-
connection handler), and `"agent_message.send"` (resolves
`receiver_role="parent"`/`"child"` to a target session id -- `self.state.
parent_id` directly, or the same `Request::SessionList`-filtered-by-
`parent_id` lookup matched by name for a child -- then reuses this
project's own existing `session message` mechanism verbatim, a
`"[from <id>] <message>"`-prefixed `Request::SessionPrompt`). Admission
is gated by a recursion-depth check (`RLM_DEPTH >= RLM_MAX_DEPTH`) held
in `SessionState`/checked entirely client-side before the daemon round
trip even starts; the daemon itself is the one place that *computes*
those two values (`daemon::handle_session_new`, inheriting `rlm_max_depth`
from the parent unchanged and incrementing `rlm_depth` by one, or
resolving both from scratch for a root session), never a client.

A child admitted via `rlm(...)` records which of the parent's own
transcript entries launched it (`SessionState::spawned_from_sequence`,
set once at admission -- unlike `rlm_depth`/`rlm_max_depth` this travels
over the wire on `Request::SessionNew`, since only the spawning worker
knows its own `last_sequence`, not something the daemon can derive). A
new background poll in `daemon::Supervisor::run` (same loop, same
`SCHEDULE_POLL_INTERVAL` cadence as `fire_due_schedules`) watches for a
child whose own worker has stopped and, if the parent is `Active`,
forwards a private-transport-only `Request::AttributeChildUsage` to the
*parent's* own worker socket -- the daemon never writes a session's
`transcript.jsonl`/`state.json` itself, so this is a relay, the same
shape `fire_one_schedule` already uses for `SessionPrompt`. The parent's
own `AgentSession::attribute_child_usage` does the actual work: sums the
child's own `TranscriptEntry::usage` (now real, parsed straight off
`rp-server`'s OpenAI-shaped `usage` field, previously discarded
entirely) and appends a `Role::System` entry carrying a
`ChildUsageAttribution` -- idempotent via a scan of the parent's own
transcript for an existing attribution of that child, not a separate
registry. See `PARITY.md`'s RLM programming model entry for the full
mechanism (why `control`, why the `ipykernel` monkeypatch is necessary,
what still isn't wired up).

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

## Session forking (`session fork`)

Bounded parity with a slice of `prime-agent`'s `/tree`/`/fork`/`/clone`
-- see `PARITY.md` for the full story. `session fork` is
**session-level** forking: a brand-new, independent session whose
starting transcript is a copy of an existing session's own transcript
up through `--at N` (or the whole thing). Distinct from the
**intra-session** branching described in the next section, which
diverges *within* one session's own transcript instead of creating a
new session.

New `protocol::ForkedFrom { session_id, at_sequence }` on
`SessionState`/`SessionSummary`, distinct from `SessionState::
parent_id` (recursive subagents relate whole sessions by *ownership*;
this relates them by *shared transcript history*). `session::
snapshot_for_fork(session_dir, at_sequence)` reads a source session's
`state.json`/`transcript.jsonl` straight off disk (`catalog::scan`'s own
"files are the source of truth" reasoning, so this works even if the
source's worker isn't currently running) and truncates, erroring loudly
if `at_sequence` is past the transcript's real end. `session::
seed_forked_session` writes a fresh `state.json`/`transcript.jsonl` pair
for the new session id -- carrying forward `model`/`thinking`/`tools`/
`runtime` from the source, but deliberately not `goal`/`harness` (both
are narrative fields only accurate against the source's *full* history,
which a truncated copy may not match).

`daemon::handle_session_fork` is the only caller: unlike `SessionRename`/
`SessionCompact` (forwarded to an existing session's own worker), a fork
creates a brand-new session, so the daemon handles it directly, the same
shape `handle_session_new` already has. It spawns the new worker with
`WorkerMode::Resume` (not `New`, which always starts an empty transcript
via `AgentSession::create`; not `Recover`, which would misleadingly
append a crash-recovery marker) -- `AgentSession::recover`'s ordinary
full-replay picks up exactly what `seed_forked_session` wrote to disk,
the same path any other resumed session goes through.

## Intra-session tree branching (active-leaf transcript model)

Parity with `session-format.md`'s "sessions... form a tree structure via
`id`/`parentId` fields, enabling in-place branching" -- see `PARITY.md`
for the full increment writeup, including why the original "one atomic,
invasive change" write-off turned out to be wrong once framed as an
additive second field rather than a rework of `sequence`'s own meaning.

New `protocol::TranscriptEntry::parent_sequence: Option<u64>` -- this
project's `parentId` analog, addressed via the existing `sequence`
identity rather than a new id field. Set automatically by `session::
AgentSession::append_entry` (the shared low-level append behind both
`append` and `append_child_usage_attribution`) to whatever
`SessionState::active_leaf_sequence` was at that moment; `append_entry`
then advances `active_leaf_sequence` to the entry it just wrote. Ordinary
linear conversation flow therefore always has `parent_sequence` equal to
"the previous entry," at zero extra cost to any caller.

Backward compatibility rule: `parent_sequence: None` with `sequence > 1`
means a legacy entry (written before this field existed, `#[serde(
default)]`) -- `AgentSession::active_chain`'s walk treats that as an
*implicit* link to `sequence - 1`, the flat order every pre-existing
transcript already has. Only `sequence == 1` with `parent_sequence: None`
ends the walk as a genuine root. This is what keeps the change additive:
no session's `transcript.jsonl` is ever rewritten to backfill real values
into old entries, matching this project's existing "the transcript file
is append-only, never rewritten or truncated" invariant (already relied
on by compaction). `AgentSession::recover` backfills
`active_leaf_sequence` from the transcript's last entry if it's still
`None` after loading `state.json`, the same reconciliation `last_sequence`
already gets.

`AgentSession::active_chain(&self) -> Vec<&TranscriptEntry>` walks from
`active_leaf_sequence` back to the root via `parent_sequence` (falling
back to the legacy rule) and reverses to root-to-leaf order -- "the
actual conversation," as opposed to `self.transcript` (every entry ever
appended, across every branch, in write order -- still shown unfiltered
by `session attach`/`snapshot_event`). `build_turns` and `compact_now`'s
candidate collection both read `active_chain()` instead of `self.
transcript` directly, so a reply or a compaction pass only ever sees the
currently active branch.

`AgentSession::set_active_leaf(&mut self, sequence) -> Result<u64>`
(`pub(crate)`) validates `sequence` names a real entry anywhere in
`self.transcript` (any branch) and persists it as the new
`active_leaf_sequence` -- a pure pointer mutation, the same "mutate +
persist, no transcript entry" shape `rename` has. A real fork only
appears once the *next* `append_entry` call lands: if the redirected leaf
already has a child down the previously-active path, the new append
becomes a second child of that same parent. New wire protocol
`Request::SessionSetActiveLeaf { session_id, sequence }` / `Response::
SessionSetActiveLeafAck { active_leaf_sequence }`, forwarded from the
public daemon socket to the owning worker's private socket unchanged --
the same relay `SessionRename`/`SessionCompact` already use.

This first increment landed with no CLI/REPL command calling
`set_active_leaf` yet -- deliberately data model + protocol/backend
only, matching the pattern `rlm_depth`/`rlm_max_depth` set (landing
before any CLI exposed them). The next section covers the surface that
now does.

## `/tree` navigation + active-leaf switching

The CLI/REPL surface for the mechanism above: `harness session tree
<id>` (display) and `harness session set-active-leaf <id> <sequence>`
(navigation) as top-level commands (`client::session_tree`/`client::
session_set_active_leaf`), plus `/tree` and `/tree <sequence>` wired
into `session_repl`'s own stdin loop -- the same "one command name,
display with no argument, act with one" shape a bounded REPL slice can
afford without `prime-agent`'s own real interactive picker (this
project has no raw-mode UI yet to build one in).

`session_tree` reconstructs the tree entirely client-side from fields
the wire protocol already carries end to end -- `TranscriptEntry::
parent_sequence` and `SessionState::active_leaf_sequence`, both already
serialized onto `SessionEvent::Snapshot` -- rather than adding a new
pre-rendered request/response shape, the same "the client renders, the
wire carries data" split every other `--mode text` renderer already
follows. Its own `effective_parent` helper mirrors `AgentSession::
active_chain`'s legacy-fallback rule exactly (duplicated, not shared,
since the client and worker are different processes communicating only
over the wire protocol), so a pre-branching session still renders as
the flat chain it always was. `fetch_session_snapshot` (a small
generalization of the existing `fetch_transcript_snapshot`, which now
just discards the `SessionState` half) is what fetches both halves in
one `SessionAttach`-then-read-first-`Snapshot`-then-disconnect round
trip.

One real gap this increment's own tests surfaced and fixed:
`set_active_leaf`'s `Err` for an unknown sequence, previously propagated
via a bare `?` out of `worker::handle_private_connection`, closed the
private connection with no response at all instead of reaching the
client as the conflict it actually is -- every request relayed across
that private-transport boundary before this one (`SessionRename`,
`SessionCompact`) happens to never fail, so nothing had exercised an
error path there before. Fixed by matching the `Result` explicitly in
`handle_private_connection` and writing a `Response::Error { conflict:
true, .. }` back over the private connection instead of using `?` --
the same explicit-match-not-`?` shape `daemon::Supervisor::
handle_session_fork` already uses for its own genuinely-failable step,
now the established pattern for private-transport requests too.

## Branch summaries (`BranchSummaryEntry`) and `/clone`, revisited

Parity with `session-format.md`'s `BranchSummaryEntry` -- previously
tracked as absent because it "depends on the tree structure," which now
exists (the two sections above). Same shape decision as
`ChildUsageAttribution`: a new `protocol::BranchSummary {
branch_leaf_sequence, entry_count, summary }`, boxed behind
`TranscriptEntry::branch_summary: Option<Box<BranchSummary>>` (its own
`String` was enough to trip clippy's `large_enum_variant` on
`SessionEvent`, the same reason `SessionEvent::Snapshot` already boxes
its `SessionState`) -- a flat optional field, not a separate typed
message-union class.

`AgentSession::branch_summarize(&mut self, branch_leaf_sequence) ->
Result<(bool, Option<String>)>` is read-only with respect to the branch
it describes, unlike `compact_now` (which shrinks the *active* chain in
place): it walks `branch_leaf_sequence`'s own branch back to wherever it
diverges from the chain active *right now*, asks the session's own model
to summarize those turns the same way `compact_now` asks it to fold old
ones into a running summary, and appends the result as an ordinary
`Role::System` entry on the *current* active chain -- a durable record
of "here's what happened over on that other branch," not a mutation of
the branch it summarizes. A new `effective_parent_sequence` free
function factors the legacy-fallback rule out of `active_chain`'s own
walk so both this and branch-summarization share one implementation of
it. `(false, None)` (not an error) for the same two honest no-op shapes
`compact_now` already established: no model configured, or
`branch_leaf_sequence` already part of the active chain. An unknown
sequence is a real conflict, matched explicitly in
`handle_private_connection` (the same fix `SessionSetActiveLeaf` needed,
now the established pattern for every private-transport request that can
genuinely fail).

New `Request::SessionBranchSummarize { session_id, branch_leaf_sequence
}` / `Response::SessionBranchSummarizeAck { summarized, summary }`, same
relay every other session-mutating request uses. `harness session
branch-summary <id> <sequence>` plus `/branch-summary <sequence>` in
`session_repl` -- the same "protocol + top-level command + REPL wrapper"
shape `/compact`/`/fork`/`/tree` already established. `session_tree`'s
own display renders a `BranchSummary` entry's otherwise-empty `text` as
`(branch summary of sequence N, K turns)` instead of a blank line.

**`/clone` was re-investigated fresh alongside this, not left on its old
reasoning.** The old blocker ("depends on the tree structure") is gone,
but the *real* one never was that -- `prime-agent`'s `/clone` duplicates
*live* interpreter/kernel state (in-flight Python variables, imported
modules, open connections), not just durable transcript/config data.
Nothing in this project's architecture can do that at any layer: a
worker crash or `session stop` already loses an `IpythonKernelRuntime`'s
live kernel state today (only `transcript.jsonl`/`state.json` survive),
and `session fork`'s own design was built around that exact limit (see
the "Session forking" section above). A `session clone` built from what
this project actually has would just be `session fork` with truncation
and narrative-reset turned off -- not a distinct feature. Real live-state
duplication needs OS-level process forking of a running kernel (or full
interpreter-state serialization), a genuinely different and much larger
mechanism than anything branch summaries touched, so it stays
unimplemented.

## Raw-mode terminal control (`termctl`) -- interactive TUI foundation

First of several increments toward `prime-agent`'s real interactive TUI
(panes, cursor control, live re-rendering) -- see `PARITY.md`'s own
"Interactive TUI: raw-mode rendering foundation" entry for the full
writeup, including the real-pseudo-terminal verification proving `Ctrl-C`
is genuinely handled by this project's own code rather than delivered as
`SIGINT`. Scoped narrowly: raw mode plus the minimal live re-rendering it
enables (manual byte-level echo/backspace/cancel), not the rich editor
(multiline input, `@` fuzzy file search, tab completion) a later,
separate increment builds on top of it.

`src/termctl.rs`: hand-rolled direct `libc`/`windows-sys` FFI
(`procutil.rs`'s own precedent for "a handful of small, direct syscalls,
not a dependency"), not a `crossterm`-style terminal-UI crate.
`termctl::is_tty()` requires both stdin *and* stdout to be a real
interactive terminal (`libc::isatty` on unix, `GetConsoleMode` succeeding
on Windows) -- every one of this project's own tests pipes both
(`Stdio::piped()`), so this reports `false` under test and nothing below
engages; raw mode is additive for a real interactive caller only.

`termctl::RawModeGuard::enable()` applies the standard raw-mode recipe
directly via `termios`/`tcgetattr`/`tcsetattr` on unix (clears
`ICANON`/`ECHO`/`ISIG`/`IEXTEN` on `c_lflag`, `IXON`/`ICRNL`/`BRKINT`/
`INPCK`/`ISTRIP` on `c_iflag`, `OPOST` on `c_oflag`, sets `VMIN=1`/
`VTIME=0`) and the equivalent `GetConsoleMode`/`SetConsoleMode` input-mode
flags on Windows (`ENABLE_LINE_INPUT`/`ENABLE_ECHO_INPUT`/
`ENABLE_PROCESSED_INPUT` cleared), restoring the original mode on `Drop`
-- including an early return or panic unwind, so a crashed REPL never
leaves the caller's shell stuck in raw mode. The flag-computation itself
(`termctl::make_raw`) is factored out as a pure function specifically so
it's unit-testable without a real terminal, since `enable`'s own
`tcgetattr`/`tcsetattr` calls need one to succeed against. Deliberately
excludes terminal-size querying (`TIOCGWINSZ`/
`GetConsoleScreenBufferInfo`) -- nothing here needs it yet; left for
whichever later increment (the rich editor) actually needs the
terminal's width.

`client::session_repl`'s stdin loop reads through `next_repl_line`/
`read_raw_line` when `termctl::is_tty()`, falling straight back to the
pre-existing blocking-read behavior otherwise (unchanged, still what
every test exercises): byte-by-byte reading with minimal manual editing
(printable bytes echoed and appended, Backspace/Delete erases the last
byte, `Ctrl-C` cancels the current line and prints `^C` rather than
exiting -- `ISIG` being cleared means the terminal never delivers
`SIGINT` for it, this project's own read loop handles the raw `0x03`
byte instead -- `Ctrl-D` on an empty line signals EOF, Enter submits).
Only the *how one line gets read* changed; `session_repl`'s own command
dispatch (`/heartbeat`, `/compact`, `/file`, `/fork`, `/tree`,
`/branch-summary`, `/export`, ...) is untouched, layered underneath the
existing loop the same way `/tree` layered onto it earlier rather than a
rewrite of it.

## Interactive TUI: rich editor (multi-line, `@` fuzzy search, Tab completion)

Builds directly on the raw-mode foundation above -- see `PARITY.md`'s
own "Interactive TUI: rich editor" entry for the full writeup, including
the real pseudo-terminal verification proving multi-line composition
and Tab completion actually work end to end, not just in isolated unit
tests.

**Multi-line input**: raw mode leaves `\r` (Enter) and `\n` (`Ctrl-J`)
genuinely distinct bytes -- `read_raw_line` treats `\r` as submit, `\n`
as "insert a literal newline and keep composing." Backspacing across a
`Ctrl-J`-inserted line boundary rejoins the buffer but doesn't attempt
to move the terminal's own cursor back up a line already scrolled past
-- that needs real cursor-positioning primitives `termctl` deliberately
doesn't have yet.

**`complete_repl_line`** is the one completion mechanism behind both Tab
completion and `@` fuzzy search: the buffer's first word starting with
`/` completes against `REPL_SLASH_COMMANDS` (a fixed list kept in sync
with `session_repl`'s own dispatch by construction); the current word
(wherever it is in the line) starting with `@` fuzzy-completes the path
fragment after it against real filesystem entries via
`complete_at_path`. `fuzzy_matches` does in-order subsequence matching
(every character of the fragment appears somewhere in the candidate, in
order, case-insensitively) rather than a plain prefix match -- real
"fuzzy," not "starts with." `common_prefix` gives bash-style ambiguous
completion (complete to the longest shared prefix across all matching
candidates); zero candidates or no further shared prefix rings the
terminal bell instead of opening any kind of listing UI -- there isn't
one, by design (a live interactive dropdown needs terminal cursor
positioning `termctl` doesn't expose yet, tracked as the genuinely
out-of-scope piece in `PARITY.md`'s "Needs a new subsystem" section).

**`expand_at_references`** is the submission-time half of the `@` slice:
every `@<path>` token anywhere in the final line (Tab-completed or typed
out by hand) that resolves to a real, readable file gets expanded inline
into that file's own content, formatted the same way `/file`'s own
`pending_file_content` prefix already is -- placed precisely where
referenced rather than only prepended, a more precise placement `/file`
structurally can't offer. Applied to the line regardless of whether it
came from the raw-mode read loop or the piped/cooked-mode fallback,
so it's testable without a real terminal (`tests/repl.rs` covers it end
to end against a real, piped `session repl` process).

## REPL commands: `/file`, `/fork`, `/export`, `/tree`, `/branch-summary`

Bounded parity with a slice of `prime-agent`'s TUI-side rich-editor/
message-queue features -- see `PARITY.md` for the full story, including
which pieces of that surface (steering vs. follow-up queuing, `/clone`,
`/share`) don't have a bounded slice and why (image paste does now --
see "Image paste" below). All four live in `client::session_repl`'s own
stdin loop, the same shape
`/heartbeat`/`/compact` already established: a REPL-only line command
calling directly into an existing client-side function, no daemon/
worker/protocol change.

`/file <path>` reads a local file and stashes its content in a
`pending_file_content: Option<String>` local to the loop, consumed (and
cleared) by the next line that actually calls `send_prompt` -- an
intervening `/heartbeat`/`/compact`/`/fork` line doesn't drop it.
`/fork [--at N] [--name TEXT]` calls the already-existing `client::
session_fork` (see "Session forking" above) directly; its own small
argument parser (`parse_repl_fork_args`) exists because `cli::
scan_named_flag` is private to `cli.rs` and shaped around a full argv
slice, not one already-stripped REPL line. `/export <path>` re-fetches
the session's current transcript and writes it as pretty-printed JSON
via `serde_json::to_string_pretty` -- the same already-`Serialize`
`TranscriptEntry` `--mode json` already renders, no new format
invented.

## Image paste

`prime-agent`'s multimodal input, bounded to "reference a local image
file" -- see `PARITY.md`'s "Interactive TUI: image paste support" entry
for the full story of what was assumed missing (a whole new content-type
subsystem) versus what actually was missing (only this project's own
text-only wire shapes; `rp-server`, the sibling `rusty_provider` repo
this project's `RustyProviderModel` already shells out to, already had
real `ContentPart::ImageUrl` multimodal support end to end).

Additive, not a restructuring: `images: Option<Vec<String>>` sits
alongside `text` on `protocol::TranscriptEntry`, `provider::ChatTurn`,
and `protocol::Request::SessionPrompt`, each entry a
`data:<mime>;base64,<...>` URI -- the exact shape `rp_server`'s own
`ContentPart::ImageUrl` already accepts inline. `session::
AgentSession::prompt_with_images` is the one entry point every image-
carrying prompt goes through, persisting the images on the user's
`TranscriptEntry` via a narrowly-scoped `append_user_turn_with_images`
helper (kept separate from the general-purpose `append` specifically to
avoid pushing that function past clippy's `too_many_arguments` limit --
`append` itself, and every existing caller, is untouched).
`provider::RustyProviderModel::build_request_body` switches a turn's
JSON `content` from a plain string to a `[{"type":"image_url",...},
{"type":"text",...}]` content-block array only when that turn actually
carries images.

Three surfaces reach an image into a prompt: `client.rs`'s `/file
<path>` and `@<path>` (`session_repl`'s two existing local-file-
reference mechanisms) check the path's extension against a small
recognized set (png/jpg/jpeg/gif/webp/bmp) before falling back to their
existing text-inlining behavior, routing a real image to an out-of-band
`images` list instead -- for `@`, the literal `@path` token stays in the
prompt text unchanged, unlike a text-file `@`-expansion, which replaces
the token with file content. A third surface, `harness session prompt
<id> --image <path>... <text...>`, is a repeatable CLI flag (hand-parsed
in `cli.rs` since `scan_named_flag` only handles single-occurrence
flags) that fails loudly on an unreadable path or unrecognized
extension, rather than `/file`'s silent fall-through to "not an image,
try as text." Base64 encoding (`client::base64_encode`) is a small,
hand-rolled RFC 4648 encoder, the same "hand-roll narrow protocol/
encoding concerns instead of adding a dependency" precedent as the
SHA-256/HMAC and ZMTP modules. `EchoProvider` appends `" [+N
image(s)]"` to its reply when the last user turn carried images, a
CI-safe seam proving images reach the provider without needing a real
vision model for most coverage; `tests/ollama_provider.rs`'s
`#[ignore]`d `ollama_provider_describes_a_real_image` is the real-model
proof, run manually against `moondream` in this project's own sandbox.

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
