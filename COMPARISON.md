# `rusty_prime_agent` vs `prime-agent`: a structural comparison

An independent, evidence-based comparison of this project against the
upstream reference implementation it mirrors, read directly from both
codebases rather than from either project's own descriptive copy.

**Compared revisions**

| | Revision | Date |
|---|---|---|
| `PrimeIntellect-ai/prime-agent` | `97b994c3` (`main`, v0.7.2) | 2026-08-14 |
| `baileyrd/rusty_prime_agent` | `6901258` (`main`) | 2026-08-16 |

**How this document relates to the others.** [`PARITY.md`](PARITY.md)
tracks *feature* parity as a per-item worklist, and
[`CLAIMS_AUDIT.md`](CLAIMS_AUDIT.md) fact-checks upstream's descriptive
copy claim by claim. Neither answers "how do these two systems differ as
systems." That is this document's job: topology, protocol, failure
semantics, and the shape of the engineering, with a bias toward the
places where the two designs genuinely disagree rather than the places
where one is simply smaller. It also records, at the end, three
documentation-drift findings the comparison surfaced in this repo's own
docs.

---

## 1. These are not the same category of artifact

The most important framing point, and the one that makes a
feature-by-feature score misleading:

- **`prime-agent` is a shipping product.** Five distribution targets of
  precompiled binaries, a full-screen TUI, thirteen model providers with
  OAuth and generated cost catalogs, an npm package ecosystem, an
  extension SDK, a public agent-client-protocol surface, near-daily
  releases.
- **`rusty_prime_agent` is a conformance-driven reimplementation of one
  architectural slice**, with feature mirroring layered on opportunistically
  where it composes with that slice. Its center of gravity is the
  daemon/worker/session core and its failure semantics; everything else
  exists to keep that core honest by giving it real work to do.

The size ratio is roughly **8:1** in non-test source, and roughly
**24:1** in test volume — but the two test corpora are not measuring the
same thing (see §11).

| Metric | `prime-agent` | `rusty_prime_agent` |
|---|---:|---:|
| Non-test source LOC | 170,788 TS/TSX + 3,008 Py | 21,822 Rust |
| Non-test source files | 359 (+23 `.py`) | 36 |
| Test LOC | 157,952 | 6,565 (integration) + inline unit |
| Test files / functions | 424 files | 28 files / 394 test fns |
| Resolved dependency graph | 463 lockfile entries | 39 lockfile crates |
| Direct runtime deps | 23 (coding-agent) + 11 (ai) | 4 (+2 platform-gated) |
| CI workflows | 4 | 2 (both 3-OS matrix) |
| Packages / crates | 4 npm workspaces + 1 PyPI shim | 1 crate (lib + bin) |

Per-package upstream breakdown: `coding-agent` 118,859 · `ai` 34,432 ·
`tui` 15,102 · `agent` 2,395.

---

## 2. Dependency posture: the sharpest philosophical divergence

This is where the two projects differ most, and it is a deliberate
choice on both sides rather than an accident of maturity.

**Upstream composes.** `packages/ai` alone pulls in `@anthropic-ai/sdk`,
`@aws-sdk/client-bedrock-runtime`, `@google/genai`,
`@mistralai/mistralai`, `openai`, `proxy-agent`, `undici`; `coding-agent`
adds `zeromq`, `@agentclientprotocol/sdk`, `typebox`, `proper-lockfile`,
`photon-node`, `jiti`, `marked`, and sixteen more. 463 resolved packages.
Each one is a vendor-maintained implementation of a protocol upstream
therefore doesn't have to own.

**This project owns its stack down to the syscall.** The entire resolved
graph is 39 crates, and of those, the only ones not authored by this
repo's owner are `serde`, `serde_json`, `thiserror`, `windows-sys`, and
their proc-macro/`libc` transitives. Everything else — the async runtime
(`rusty_tokio`), the std layer (`rusty_std`), libc bindings
(`rusty_libc`), Win32 bindings (`rusty_win32`), the platform abstraction
(`platform-*`) — is first-party.

Three consequences worth naming, because they are the actual trade:

1. **Wire protocols are hand-rolled where a crate would have done.**
   `src/zmtp.rs` (263 LOC) + `src/sha256.rs` (235 LOC) implement ZMTP 3.0
   and its HMAC-SHA256 signing scheme by hand rather than taking the
   `zeromq` crate — because that crate depends unconditionally on real
   `tokio`, and this process already runs `rusty_tokio`. Upstream simply
   `npm install zeromq`. `src/http_client.rs` (262 LOC) is the same story
   against `undici`.
2. **The model provider layer is out-of-process, not in-process.** This
   is the single largest structural difference in the whole comparison
   and gets its own section (§8).
3. **`ARCHITECTURE.md`'s "Dependency stack" section reads as a design
   record, not a manifest.** It documents the `cargo tree` check that
   killed the `zeromq` dependency and the `rusty_tokio` epoll busy-spin
   fix that a long-lived worker socket surfaced. Upstream has no
   equivalent because it has no equivalent decision to record.

Neither posture is wrong. Upstream's buys thirteen providers for the cost
of eleven dependencies. This project's buys a 39-crate audit surface and
a build that has no npm supply chain in it at all, at the cost of writing
SHA-256 by hand.

---

## 3. Process topology: three genuine divergences

Both systems run the same three-tier shape:

```
clients ──public socket──▶ supervisor ──private socket──▶ worker(s) ──▶ kernel
```

and both hold the same load-bearing invariant: **the supervisor routes,
the worker executes.** This project's `src/daemon/mod.rs` module doc says
so explicitly ("never executes providers, tools, or transcript writes
itself"), matching upstream's `R-ARCH-02`/`R-SUP-01`.

Below that, three real disagreements.

### 3.1 Worker granularity: per-session vs per-root-tree

| | `prime-agent` | `rusty_prime_agent` |
|---|---|---|
| Unit a worker owns | one **root session tree** — root + every RLM descendant beneath it | one **session** |
| RLM child lives | inside the parent's worker process | in **its own worker process** |
| Child admission path | in-process, via `AgentSession` | round trip through the daemon (`SessionNew` + `ScheduleAdd`) |

Upstream's `R-ARCH-03`/`R-WRK-01` are explicit that a worker owns "every
descendant beneath that root." This project instead makes every RLM child
a first-class daemon session with its own worker — `rlm(...)` in the
kernel calls the same `SessionNew` path the `session spawn` CLI does
(`session::handle_rlm_run`).

**The trade.** This project gets: uniform lifecycle (a child is not a
special case of anything), independent crash domains per child, and
`session list`/`attach`/`stop` working on children for free. It pays:
one OS process per child instead of one per tree, and daemon round trips
on the spawn hot path where upstream has a function call. Upstream gets
the reverse.

**Notable convergence.** Upstream's newest commit at the compared
revision (`#1387`, "supervisor-owned rlm spawn ledger as family
authority") moves family topology — parent/child edges, depths, names —
out of writer-owned session state and into a supervisor-owned append-only
JSONL ledger (`modes/daemon/rlm-ledger.ts`). That is a step *toward* this
project's "the daemon is the authority on family structure" model, from
the opposite direction. Worth tracking: if upstream keeps moving family
authority supervisor-ward, this project's topology stops looking like a
simplification and starts looking like an early arrival.

### 3.2 Scheduler placement: supervisor vs worker

This is a direct contradiction of an upstream invariant, and it is worth
being blunt about.

- Upstream: `R-SUP-01` — the supervisor **must not** execute schedules.
  `R-SCHED-01` — each worker runs exactly one scheduler for its root and
  descendants; jobs persist per-session, never in a shared global file.
  Verified: `AgentCronScheduler` is constructed in
  `modes/daemon/daemon-mode.ts`, which is the **worker** process entry.
- This project: the supervisor runs a single background loop
  (`SCHEDULE_POLL_INTERVAL`, 5s) that polls **every** session's
  `schedules.json` and fires due entries as internal `SessionPrompt`s
  (`daemon::Supervisor::fire_due_schedules`). The same loop also drives
  `attribute_pending_child_usage`.

Job storage is per-session in both (this project does *not* have a global
cron file), so `R-SCHED-01`'s second half holds. But the *execution* of
schedules sits on the wrong side of the supervisor/worker line relative
to the reference design.

**Why it matters concretely.** Upstream's `R-SCHED-02`/`R-SCHED-03`
(claim-and-advance before delivery; coalesce missed ticks rather than
build backlog) are cheap when the scheduler and the session runtime share
a process — the claim and the delivery are the same transaction. Across
a socket they are not, and a supervisor restart mid-fire has no
worker-local claim record to reconcile against. This project's scheduler
also cannot be as precise as upstream's: a 5s global poll is a
coarser timing guarantee than a per-worker scheduler with its own timer.

This is the one place where the divergence looks less like a deliberate
simplification and more like a design decision worth revisiting. See §12.

### 3.3 Session ownership: lease vs process-wide lock

- Upstream: a process-safe **lease keyed by canonical JSONL path**
  (`core/session-lease.ts`), acquired via atomic directory rename, with
  PID-reuse-safe liveness (existence probe + process-start-time
  fingerprint), sidecar-rename-before-delete reclaim, and a separate
  short-lived lock around the read-check-reclaim sequence. Concurrent
  opens return `session_already_active`.
- This project: a single coarse `Mutex<()>` (`Supervisor::spawn_lock`)
  serializing "check liveness, spawn/recover if needed" for the *whole
  supervisor*. Its own doc comment says it "stands in for the reference
  architecture's per-canonical-path session lease."

The lock is correct for the property it actually defends — the
double-spawn race between two concurrent `SessionAttach`/`SessionPrompt`
calls for the same crashed session. It is strictly weaker in two ways:
it serializes unrelated sessions against each other, and it protects
nothing against a *second supervisor process* or an out-of-band writer,
because it is in-process memory rather than an on-disk artifact. This
project currently gets away with that because `daemon.sock`'s bind
exclusivity makes the supervisor a singleton per state root — which is a
real argument, but a narrower one than a filesystem lease.

---

## 4. Wire protocol

| Dimension | `prime-agent` | `rusty_prime_agent` |
|---|---|---|
| Public framing | JSONL, versioned envelopes, **v4** | JSONL, `PROTOCOL_VERSION = 1` |
| Private framing | **binary**: 4-byte header len + 4-byte payload len + JSON routing header + opaque payload | same JSONL as public |
| Event cursor | `{generation, sequence}` | `{generation, sequence}` ✅ |
| Generation identity | random **UUID per supervisor instance**; comparison is identity-equality only, never ordering (`R-PROTO-18`) | `(monotonic `u64` counter, 128 random bits)` pair — see §4.1 |
| Capability negotiation | yes, per-command compat metadata | none |
| Protocol vs schema versioning | tracked **independently** (`R-PROTO-07`) | single number |
| Large snapshots | `begin`/`chunk`/`end` at 512 KiB target; supervisor never materializes a history-sized object | single `SessionEvent::Snapshot` |
| Fanout cost | worker serializes each event **once**; supervisor forwards the same buffer | supervisor re-relays per connection |
| Backpressure | explicit: no unbounded per-client queue; a blocked client stops receiving increments only | no stated policy |
| Private-connection auth | per-worker token **fenced to supervisor generation** (`R-PROTO-11`) | ✅ per-worker token fenced to supervisor generation (`src/fence.rs`) |

**What the JSONL-everywhere choice actually costs.** Upstream's binary
private frame exists so the routing header can be parsed without touching
the payload, enabling serialize-once/forward-many. That is a throughput
optimization at high client fanout; at this project's realistic fanout
(one or two attached CLIs) it buys approximately nothing, and the
uniform-JSONL choice is defensible. The genuinely missing pieces are the
ones that are about *correctness under stress*, not throughput:

1. ~~**Generation as counter, not fence.**~~ **Closed** — implemented in
   `src/fence.rs`, `tests/worker_fence.rs`.

   As originally written, this was the highest-leverage single idea in
   the reference design this project had not adopted. Upstream's UUID
   generation is not a version number, it is an authentication fence: a
   worker adopted by supervisor generation *G* rejects any command
   carrying a token fenced to a different generation, which resolves "is
   this supervisor stale?" architecturally rather than empirically — the
   same classification problem `transport::Listener::bind_with_retry`
   answers with a real `Ping`/`Pong` round trip and a 20-second budget,
   after a Windows `AF_UNIX` reclaim race that took several rounds of
   upstream `rustils` fixes to characterize.

   This project now has the mechanism, with one deliberate difference
   forced by §3.3. Upstream compares generations by identity-equality
   only, *never* ordering (`R-PROTO-18`), because its atomic launch lease
   already guarantees a single legitimate supervisor. Absent that lease,
   identity-equality alone would be unsound here: a stale supervisor can
   read the worker token off disk and simply re-adopt the worker back.
   So `SupervisorIdentity` is a `(counter, instance)` pair — the
   monotonic `daemon.pid` counter, which adoption requires to be
   **strictly greater**, plus 128 random bits that ordinary traffic
   requires to match exactly. The residual weakness is a concurrent-
   startup tie (two supervisors both writing `counter = N + 1`), which
   fails closed in both directions rather than letting either command the
   other's workers; closing it properly is item 4's launch lease, not a
   change to the fence. `ARCHITECTURE.md`'s "Generation-fenced per-worker
   tokens" section carries the full design.

   `bind_with_retry` is unchanged and still needed. What the fence buys
   is that being *wrong* about its verdict is now survivable.

2. **No chunked snapshot.** A large transcript is one `Snapshot` event.
   Upstream chunks at 512 KiB specifically so the supervisor never builds
   a full history-sized object in memory. This project's supervisor does.

3. **No backpressure policy.** Upstream states one explicitly. This
   project has not needed to, but "has not needed to" is not the same as
   "has one."

---

## 5. Crash recovery and idempotency

Both projects treat "recover from disk, never from a still-running
process's memory" as the load-bearing rule, and both implement it.
This project's `tests/supervisor_restart_recovery.rs` and
`tests/worker_crash_recovery.rs` exercise it against real force-killed
processes on three OSes.

Where they differ:

| | `prime-agent` | `rusty_prime_agent` |
|---|---|---|
| Mutating-command idempotency | `clientId + commandId`, **append-only journal written before dispatch**, fsync per append, dir-fsync after compaction rename, compaction at 4096 records | ✅ append-only journal written before dispatch, fsync per append, dir-fsync after compaction rename, compaction at 4096 records — `SessionPrompt` only, opt-in via `--request-id` |
| "Did it happen?" for a lost result | explicit **uncertain** state, never blindly replayed (`R-PROTO-03`) | ✅ `Response::SessionPromptUncertain`, never replayed |
| Survives worker crash | yes (journal is on disk) | ✅ yes (journal is on disk) |
| Recovery retry schedule | 250ms / 1s / 5s, third failure marks root failed | not staged this way |
| Recovery blast radius | one root tree | one session |
| Recovery marker in transcript | yes | yes (`SessionEvent::RecoveryMarker`) |
| Orphan subprocess tracking | append-only journal of pid + owner pid + start-time fingerprint | worker spawns no children of its own to track (documented) |
| PID-reuse safety | start-time fingerprint (PowerShell ticks / `/proc/pid/stat` f22 / `ps -o lstart=`) | ✅ start-time fingerprint (`GetProcessTimes` / `/proc/pid/stat` f22 / `ps -o lstart=`) |

Two of these are worth flagging as real, not cosmetic:

~~**Idempotency is not durable here.**~~ **Closed** —
`src/request_journal.rs`, `tests/idempotent_replay.rs`.

The gap mattered exactly when it was needed most: a client retrying after
a dropped connection is *most* likely retrying because the worker died,
which is precisely the case where the in-memory cache was gone and the
retry double-sent. There is now an append-only journal beside the
transcript, `sync_all`ed before dispatch, replayed on every worker start.

Two design points worth recording. It sits **per session** rather than at
the supervisor, where upstream puts its equivalent: the side effect being
deduplicated is a transcript append, and the process that must not repeat
it is the worker. And a `Completed` record whose sequence is absent from
the transcript degrades to *uncertain* rather than being trusted — the
journal is `sync_all`ed while the transcript is only flushed, so after a
machine-level crash the journal can be strictly ahead of the data it
describes.

~~**PID reuse is unhandled.**~~ **Closed** — `procutil::is_same_process`,
`SessionState::worker_start_fingerprint`, `tests/pid_reuse.rs`.

As written, this project's `is_alive` answered "does a process with this
pid exist", not "is this the same process that wrote `state.json`", so a
recovering supervisor reading a stale `worker_pid` now held by an
unrelated process would conclude the worker was alive and decline to
respawn. It now records a per-platform start-time fingerprint beside the
pid and compares both, as upstream does (`R-WRK-14`).

Closing it surfaced a **second** gap in the same check that upstream also
covers and this project did not: **zombies** (`R-PROC-03`). A process
that has exited but not been reaped answers `kill(pid, 0)` successfully,
and no fingerprint can catch it — a zombie has the same pid *and* the
same start time. That one was not hypothetical: it wedged a session
during this work, `daemon shutdown` leaving a worker unreaped just long
enough for the supervisor to skip the respawn and then hit `Connection
refused`. `is_alive` now excludes zombies explicitly.

---

## 6. Session persistence

Structurally close, which is a good sign for the mirroring effort:

- Both: JSONL transcript, one type-tagged object per line, tree-linked
  via parent pointers, in-place branching without a new file.
- Upstream addresses entries by `id`/`parentId` with a **closed 15-member
  entry-type union** and versioned auto-migration (v1 linear → v2 tree →
  v3 `hookMessage`→`custom`).
- This project addresses by **`sequence`** with `parent_sequence` +
  `active_leaf_sequence`, and uses a **single flat `TranscriptEntry`**
  with optional fields (`usage`, `child_usage_attributed`,
  `branch_summary`, ...) rather than a typed union. `#[serde(default)]`
  on every added field is the migration story — older `state.json` files
  parse rather than failing recovery.

The flat-struct-with-optionals choice trades exhaustive matching for
forward compatibility, and given that `serde` will happily ignore unknown
fields, it is the cheaper of the two for a project that adds fields
often. It does mean there is no equivalent of upstream's `SG-5`
select-exactly-one guarantee: nothing structurally prevents an entry from
carrying both a `branch_summary` and a `child_usage_attributed`.

Both persist feature state as sidecar files under a per-session directory
(`schedules.json` here, `scheduled-jobs.json` upstream).

---

## 7. RLM runtime and the kernel

This is the area where the gap is smallest in mechanism and largest in
concept.

**What this project genuinely has.** A real persistent IPython kernel:
`src/ipython_runtime.rs` (1,840 LOC) drives a real
`python3 -m ipykernel_launcher` over a **hand-rolled ZMTP 3.0 client**
(`zmtp.rs` + `sha256.rs`), verified byte-exact against a real
`ipykernel`. Variables and imports persist across calls. The kernel gets
`rlm(task, name, model)` as a real callable coroutine, `rlm_heartbeat()`,
`rlm_list_subagents()`, `rlm_delete_subagent()`. Depth limiting
(`RLM_DEPTH`/`RLM_MAX_DEPTH`, default 1) matches `R-RLM-01`. Child usage
attribution matches `R-RLM-08`'s shape: a `child_usage_attributed`
transcript entry carrying target sequence, child usage, and the running
aggregate.

Reimplementing ZMTP by hand and getting it byte-exact against a real
kernel is, on its own terms, the most impressive single piece of
engineering in this repo.

**What is missing is the abstraction, not the plumbing.** `CLAIMS_AUDIT.md`
already says this and it is correct: the defining RLM idea — *the prompt
and context themselves exposed as a Python variable the model can slice,
summarize, and recurse into* — was never implemented. This project has
the execution half of an RLM and the recursion half of an RLM; it does
not have prompt-as-a-variable. A model in this kernel can run code and
spawn children; it cannot manipulate its own context as data.

**Undocumented-upstream machinery this project has no analog of.** Worth
listing because these are the parts a doc-derived parity effort
structurally cannot find (they exist only in upstream source):

- **Kernel-boot admission control** (`R-RLM-12`/`13`): a semaphore
  independent of `RLM_MAX_DEPTH`, `min(16, max(4, cores×2))` on direct
  spawn, empirically justified — unthrottled fan-out measured ~28% boot
  success at 200 concurrent spawns; capping at cores×4 held 100%. This
  project has no boot gate. It has not yet run fan-out at a scale that
  would surface the problem, which is different from being immune to it —
  and because this project puts each RLM child in its own worker process
  (§3.1), a wide fan-out spawns *more* processes than upstream's would,
  not fewer.
- **Control-channel host replies** (`R-RLM-05`): upstream routes
  host-request admission replies over Jupyter's control channel rather
  than shell, because IPython processes shell messages serially and a
  cell awaiting admission would deadlock. Any future in-kernel
  synchronous host request here inherits that constraint.
- **Kernel Python resolution / managed venv** (`R-RLM-11`): upstream
  bootstraps a Python 3.11 + `ipykernel` + `prime-agent-runtime` venv via
  `uv`. This project requires the user to have `ipykernel` installed.

---

## 8. Model provider layer

The largest single-subsystem asymmetry, and a genuine architectural fork.

**Upstream:** `packages/ai`, 34,432 LOC in-process. Thirteen provider
implementations (Anthropic, OpenAI Responses/Completions/Codex-Responses,
Google direct + Vertex, Bedrock, Azure OpenAI Responses, Mistral,
Cloudflare, Copilot header handling, a `faux` test provider), OAuth,
automatic model discovery, cache pricing, a generated model catalog, SSE
streaming utilities. `R-PROV-02`: only tool-calling-capable models are
listed, because the agent loop requires it.

**This project:** provider work is **delegated to a separate process.**
`src/provider.rs` (562 LOC) is an HTTP client for `rusty_provider`'s
`rp-server`, an OpenAI-compatible router the supervisor spawns and
manages (`src/rp_server.rs`). Default is `EchoProvider` — no model, no
network. `--model provider/id` is an opaque string forwarded to the
router; the router is the only thing that ever rejects an unknown
provider name.

**This is a defensible boundary, not a shortfall.** It is the same
decision as the `zeromq` one: `rp-server` is built on real `tokio`, so
linking it would put two async runtimes in one process. Spawning it makes
it "a service to call," the same category as Ollama. The cost is that
`harness` cannot answer questions about models on its own — `model list
--detailed` needs the sidecar on `PATH` — and there is no cost model at
all (see §12, finding 2).

Where it does fall short of parity in kind rather than degree: no OAuth,
no cost/pricing data, no streaming to the client (the reply arrives whole),
and provider capability is whatever `rp-server` happens to support rather
than something this project can reason about.

---

## 9. Client surface

| | `prime-agent` | `rusty_prime_agent` |
|---|---|---|
| Interactive | full-screen TUI (`packages/tui`, 15,102 LOC): diff rendering, image display, components | line-oriented REPL (`session repl`) on a hand-rolled raw-mode layer (`termctl.rs`, 261 LOC) with multi-line editing, `@` fuzzy search, Tab completion, image paste, themes (`theme.rs`, 528 LOC) |
| One-shot | `-p`/`--print` | `-p`/`--print` ✅ (incl. daemon auto-start, piped stdin merge, `--no-session`) |
| JSON mode | event-per-line | ✅ `--json` output mode |
| RPC mode | JSONL over stdin/stdout until EOF | ✅ `session rpc` |
| ACP mode | `@agentclientprotocol/sdk` | ✅ `src/acp.rs` (526 LOC), bounded schema-verified subset |
| SDK embedding | `sdk.ts`, non-serializable extension factories | ✅ real `lib.rs` public API (355 LOC), `tests/embedded_session.rs` |
| Connection abstraction | `AgentConnection` with `Daemon`/`InProcess` implementations and a **test-enforced boundary invariant** | no equivalent named seam; `ToolRuntime` is the one deliberate ports-and-adapters trait |

The connection-boundary invariant is worth calling out as an upstream
idea with no local analog. Upstream enforces, with dedicated tests, that
`InteractiveMode` never holds a reference to `AgentSessionRuntime`,
`AgentSession`, `SessionManager`, or socket internals — the UI is
transport-agnostic by construction, and executable extension callbacks
are *architecturally* barred from crossing the boundary (`R-CONN-01`/`02`,
`R-CAP-04`). This project's REPL talks to the daemon over the same public
socket the CLI does, so it happens to satisfy the spirit of the rule;
nothing enforces it.

---

## 10. Capability loading: skills, extensions, MCP

| | `prime-agent` | `rusty_prime_agent` |
|---|---|---|
| Skills | Agent Skills standard; global/project/package/CLI/built-in precedence; markdown + Python-backed | Python packages under `<state-dir>/skills/`, importable in the kernel; `skills.rs` (293 LOC), frontmatter parsing |
| Extensions | TypeScript `ExtensionAPI`: `registerTool`/`registerCommand`/`registerShortcut`/`registerProvider`, ~25 lifecycle events, custom TUI components, hot reload | `extensions.rs` (197 LOC): one blocking `pre_tool_call` hook + custom-command registration, as Python packages |
| MCP | **never** exposed as agent-callable tools; each integration is a Python-backed skill imported in the kernel; TS host handles OAuth only (`R-CAP-02`/`03`) | `--tools mcp` exposes `rp-server`'s MCP gateway **as agent-callable tools**, namespaced `{upstream}/{tool}` |

The MCP row is a direct inversion of an upstream invariant. Upstream's
"single built-in tool" philosophy deliberately refuses to grow the tool
schema; this project's `--tools mcp` does exactly what upstream forbids.
That is a coherent choice for a harness whose tool loop is the primary
execution path rather than a fallback — but it means MCP integration here
is *not* the same mechanism upstream describes, and shouldn't be scored
as parity with it.

---

## 11. Testing and CI

Counts are not comparable, but the *strategies* are, and they are
different in an interesting way.

**Upstream** runs 424 vitest files (157,952 LOC), a contribution gate, a
binaries build, and — notably — a **`nightly-process-stress.yml`**
workflow: a dedicated recurring stress test for process/daemon behavior.
That workflow is the strongest signal in the upstream repo that the
crash-recovery correctness described in the docs is treated as a
maintained property rather than a design claim.

**This project** runs one workflow, but across a **three-OS matrix**
(ubuntu/windows/macos) with `fmt` + `clippy -D warnings` + `cargo test`,
and its CI comment states the reason precisely: the recovery tests
force-kill real child processes and rebind real `AF_UNIX` sockets, which
a cross-compile check cannot exercise. That is the right instinct — and
it has already paid, since the Windows `AF_UNIX` stale-reclaim race that
`bind_with_retry` exists for was found on real `windows-latest` CI, not
in theory.

~~**The gap:** no stress/soak dimension.~~ **Closed** —
`.github/workflows/nightly-stress.yml`.

Every recovery test here was a single deterministic scenario, while
process-lifecycle bugs are *frequency*-dependent: the `WSAENOBUFS`
behavior documented in `transport.rs` is exactly the class a
deterministic test finds once and then hides again. There are now two
nightly jobs, and the split matters:

- **`process-stress`** repeats the five suites that force-kill real
  processes and rebind real sockets (`supervisor_restart_recovery`,
  `worker_crash_recovery`, `worker_fence`, `pid_reuse`,
  `session_lifecycle`), 20× each, **on all three OSes** — where upstream's
  equivalent is ubuntu-only. These are expected to be deterministic, so
  any failure gates.
- **`flake-watch`** runs the full suite 5× and reports a per-test failure
  *rate* without gating, so "that test is flaky" becomes a number rather
  than a recollection. Deliberately separate: folding the known
  wall-clock-budget offenders into the gating job would drown its signal
  in noise it cannot act on.

That second job is a direct response to how much time the ambient flakes
cost during this work — the failure rate had to be measured by hand,
twice, to tell a real regression from background noise.

---

## 12. Documentation-drift findings in this repo

The comparison surfaced three places where this repo's own docs no longer
match its code. Recording them here rather than silently fixing them,
since each implies a small follow-up.

1. **`PARITY.md` "Needs a new subsystem" is stale on `/usage`.** It
   states: *"`/usage` needs a token/cost data model that plainly doesn't
   exist: no `usage_tokens`/`token_usage`/`cost_usd` field anywhere in
   `protocol.rs`/`session.rs`, confirmed by direct search."* That is no
   longer true. `protocol::Usage` (prompt/completion/total tokens, with
   an `Add` impl) exists, `TranscriptEntry::usage` persists it,
   `session::AgentSession::attribute_child_usage` aggregates it, and
   `ChildUsageAttribution` carries per-child and aggregate figures. The
   *token* half of the data model is done; only **cost in USD** and the
   `/usage` REPL command itself are actually missing. The bullet should
   be narrowed accordingly.

2. **Cost, specifically, has no model at all** — and unlike tokens, it
   cannot be built locally, because pricing lives in the provider layer
   this project deliberately pushed out of process (§8). Upstream
   generates a cost catalog in `packages/ai`. Closing `/usage` fully here
   means either teaching `rp-server` to report cost or maintaining a
   pricing table in this repo. Worth deciding explicitly rather than
   leaving implied.

3. **`ARCHITECTURE.md`'s scheduler description doesn't flag the
   divergence.** The supervisor runs schedules (§3.2), contradicting the
   reference design's `R-SUP-01`. `PARITY.md` and `ARCHITECTURE.md` both
   describe the scheduler without noting that it sits on the opposite
   side of the supervisor/worker line from upstream's. Given how
   carefully every other divergence in this repo is documented, this one
   reads as an oversight rather than a decision — and if it *is* a
   decision, it deserves the same paragraph of rationale everything else
   here gets.

---

## 13. Where this project is genuinely ahead

Not "smaller but adequate" — actually better on its own terms:

- **Auditability.** 39 crates, four of them third-party. A full read of
  everything this binary links is a tractable afternoon. Upstream's 463
  packages are not.
- **Cross-platform recovery testing is first-class.** *Every* upstream
  job in all four workflows runs on `ubuntu-latest` — its CI matrix is
  per-package, not per-OS, and even `build-binaries.yml` cross-builds the
  darwin/windows targets from Ubuntu. Upstream's substantial
  Windows-specific process machinery (`taskkill /F /T`, PowerShell
  start-time ticks, the win32-only zombie carve-out) is therefore covered
  by code review, not by a Windows CI leg. This project runs its real
  force-kill/rebind recovery tests on `windows-latest` and `macos-latest`
  on every PR, and found a real Windows `AF_UNIX` bug that way.
- **Rationale density.** Doc comments here routinely record what was
  tried, what the measurement showed, and why the alternative was
  rejected — the `zeromq` `cargo tree` check, the `WSAENOBUFS`
  classification failure, the `setsid`/`setpgid` incompatibility, the
  epoll busy-spin fix. Upstream's own spec-tree extraction found that
  **21 of ~120 load-bearing rules exist only in source with zero trace in
  its docs**. This project's ratio is inverted: the rationale is in the
  source, deliberately.
- **Single-binary distribution with no runtime.** `cargo build --release`
  → one `harness`. Upstream ships five per-platform bundled Node/Bun
  binaries plus a `uv`-bootstrapped Python venv for the kernel.
- **Honest self-assessment as a maintained artifact.** `PARITY.md`,
  `CLAIMS_AUDIT.md`, and their freshness passes are unusual and
  genuinely load-bearing. The three findings in §12 are drift within a
  process that is otherwise working.

---

## 14. Recommendations, ranked by leverage

1. ~~**Adopt generation-fenced per-worker tokens**~~ (§4.1) — **done**.
   `src/fence.rs` mints a per-worker token and a per-supervisor identity;
   `Supervisor::connect_worker` is now the single chokepoint every
   private connection goes through, and `Supervisor::adopt_worker`
   handles takeover on supervisor restart. `tests/worker_fence.rs` covers
   both directions, including the refusal path a passing happy path can
   never demonstrate.
2. ~~**Make idempotency durable**~~ (§5) — **done**. `tests/idempotent_replay.rs`
   now covers "survives a crash" rather than only "same process", and the
   uncertain case has its own wire response instead of being unrepresentable.
3. ~~**Add start-time fingerprinting to `procutil::is_alive`**~~ (§5) —
   **done**, and it turned up a zombie-handling gap in the same check
   that was actively wedging sessions. See §5.
4. **Decide the scheduler's home explicitly** (§3.2, §12.3). Either move
   it worker-side to match `R-SUP-01`/`R-SCHED-01` and gain per-session
   claim-and-advance semantics, or document why supervisor-side is right
   for a process-per-session topology. Both are defensible; silence is
   not.
5. ~~**Add a stress/soak CI job**~~ (§11) — **done**, on all three OSes
   rather than upstream's ubuntu-only, plus a non-gating `flake-watch`
   job that measures the ambient failure rate instead of leaving it to
   memory.
6. **Bound kernel-boot concurrency before fan-out grows** (§7). Upstream
   measured ~28% boot success at 200 concurrent spawns. This project
   spawns a *worker process per RLM child*, so a wide `rlm()` fan-out is
   heavier here than upstream's, not lighter. A semaphore is cheap
   insurance to add before the failure mode is discovered empirically.
7. **Narrow the `/usage` bullet in `PARITY.md`** (§12.1) and decide the
   cost-data question (§12.2).

---

## Appendix: methodology

Both trees were read directly at the revisions in the header —
`prime-agent` via a fresh shallow clone, this project from the working
tree. Line counts are `wc -l` over `find`-selected file sets, with
upstream tests excluded from source counts (upstream colocates tests in
`packages/*/test`, not alongside sources, so the two counts are
genuinely disjoint). Upstream invariant IDs (`R-ARCH-*`, `R-SUP-*`,
`R-PROTO-*`, ...) reference the spec-tree extraction of upstream's
`packages/coding-agent/docs/` at v0.7.2; each was re-checked against
upstream source where the comparison depended on it — in particular
`AgentCronScheduler`'s construction site (§3.2), `rlm-ledger.ts` (§3.1),
and `session-lease.ts` (§3.3).
