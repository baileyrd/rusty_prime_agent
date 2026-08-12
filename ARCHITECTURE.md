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
tractable near-term increment, and what's out of scope for this project's
current shape.

## Module map

| Module | Owns |
| --- | --- |
| `main` | Entrypoint, argv dispatch, `harden_inherited_stdio` |
| `cli` | Argument parsing for the public subcommands |
| `client` | The CLI-side half of every request: connects to `daemon.sock`, sends a `Request`, prints the `Response`/event stream. Also owns `session_autonomous`'s bounded continuation loop (`session autonomous`) and `session_refine`'s Continual Harness review loop (`session refine`, see `PARITY.md` for both) -- pure client-side orchestration over existing `SessionPrompt`/`SessionAttach`/`GoalShow`/`GoalUpdate`/`HarnessShow`/`HarnessUpdate` requests, no daemon/worker changes needed |
| `daemon` | The supervisor process: binds the public socket, routes requests, recovers sessions on its own startup |
| `worker` | The per-session process: binds a private socket, owns one `AgentSession`, serves `SessionAttach`/`SessionPrompt`/`WorkerShutdown` |
| `session` | `AgentSession` -- transcript, `state.json` (including the persistent `goal: Option<GoalState>` and Continual Harness `harness: HarnessState`, see `PARITY.md`), the (fake) model provider, the per-worker event broadcast |
| `catalog` | `session list`'s directory scan, cross-checked against process liveness |
| `transport` | JSONL framing over `rusty_tokio::io::{UnixListener, UnixStream}`, plus `bind_with_retry`/`probe`/`wait_ready` |
| `procutil` | The narrow non-`rusty_tokio` OS surface -- see "Dependency Stack" below |
| `protocol` | The wire types (`Request`/`Response`/`SessionEvent`/`SessionState`) shared by every process this project spawns |
| `paths` | State-root layout (`daemon.sock`, `daemon.pid`, `sessions/<id>/{state.json,transcript.jsonl,worker.sock,schedules.json}`, `provider.{json,log}`) |
| `provider` | `ModelProvider` trait + `EchoProvider` (the default); `RustyProviderModel`, a real backend opt-in per session via `session new --model provider/model` -- see `PARITY.md` |
| `rp_server` | Sidecar lifecycle for `rusty_provider`'s `rp-server` (spawn, health-check, teardown) -- owned by the supervisor, read by workers |
| `http_client` | Minimal hand-rolled HTTP/1.1 client `RustyProviderModel`/`rp_server` use to talk to `rp-server` |
| `schedule` | Per-session `schedules.json` read/write/take-due -- fired by `daemon`'s own background poll loop, see `PARITY.md` |
| `prompt_template` | Discovery (`paths::global_prompts_dir`/`project_prompts_dir`), frontmatter parsing, and `$1`/`$@`/`${@:N}`-style positional-argument expansion for `prompt-template list/render` and `session prompt-template` -- see `PARITY.md` |
| `tool_runtime` | `ToolRuntime` trait boundary -- see below |
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

**No bridging layer.** An earlier revision of `transport.rs` bridged
`rustils`' blocking `Net` trait through `spawn_blocking` by hand, because
`rusty_tokio` had no native Windows `AF_UNIX` support yet. That bridge was
removed once `rusty_tokio`'s own Windows `AF_UNIX` support landed (the
"rusty_tokio-Windows-gap addendum") -- `transport.rs` now builds directly
on `rusty_tokio::io::{UnixListener, UnixStream}`, genuinely non-blocking
on every platform this project targets (Linux/macOS/BSD via epoll/kqueue,
Windows via IOCP+AFD-poll).

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
no-op/mock runtime") -- still the one genuinely open seam, unrevised by
anything in this session's own work.

`tool_runtime::ToolRuntime` (object-safe, `Box<dyn ToolRuntime>`,
boxed-future async methods rather than a real `async fn` in the trait --
see that module's own doc comment for why) is the host-side handle to a
model-facing code execution environment: `start`/`execute`/`shutdown`.
Phase 1 backs it only with `NoopToolRuntime`, which never spawns a
process and never executes anything -- it exists so `AgentSession` and
the worker's lifecycle plumbing are built and tested end to end against
the real trait shape before a Phase 2 kernel backend exists.

Every *other* subsystem in this crate calls `rusty_tokio` (and, through
it, `rustils`) directly rather than wrapping them behind a speculative
trait -- `transport.rs` calls `rusty_tokio::io` directly, `worker::spawn`
calls `rusty_tokio::process::Command` directly. `ToolRuntime` is the one
boundary that genuinely has a second, concrete backend coming (Phase 2's
real IPython kernel subprocess, itself spawned via
`rusty_tokio::process::Command`/`rusty_tokio` pipes -- the trait's boxed
futures exist specifically so that `.await`ing subprocess I/O won't need
a redesign when that lands), so it alone earns the indirection now.

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
