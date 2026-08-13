# rusty_prime_agent

A small, cross-platform, daemon-backed agent harness written in Rust.
`daemon start` launches a supervisor that owns a public Unix-domain (or
Windows `AF_UNIX`) socket; `session new`/`session prompt`/`session
attach` spawn and talk to per-session worker processes over private
sockets of their own; crash-recovery paths (supervisor restart, worker
crash) rebuild in-memory state from disk rather than trusting anything a
still-running process remembers.

This project deliberately mirrors one slice of
[`PrimeIntellect-ai/prime-agent`](https://github.com/PrimeIntellect-ai/prime-agent)'s
daemon/worker operational architecture, plus a bounded, mostly-non-Python
subset of its higher-level features (scheduling, persistent goals,
bounded autonomous mode, prompt templates, the Continual Harness,
recursive subagents, a minimal REPL, model catalog listing, real
tool-calling/MCP integration, and a real persistent IPython kernel for
its RLM programming model) -- without attempting to reimplement
`prime-agent` itself. See
[`PARITY.md`](PARITY.md) for exactly what's mirrored, what's a
deliberate simplification, and what's not yet implemented in this
project's current shape, and [`ARCHITECTURE.md`](ARCHITECTURE.md) for how the
pieces fit together internally.

## Build

```sh
cargo build --release
```

The binary is named `harness` (`target/release/harness`).

## Quick start

```sh
harness daemon start
harness session new --name demo
harness session prompt <session-id> "hello there"
harness session list
harness daemon shutdown
```

Or skip the ceremony for a single one-shot reply:

```sh
harness -p "hello there"
```

`-p`/`--print` transparently starts a daemon if none is running, creates
an unnamed session, prompts it once, and prints just the reply.

By default every session uses a built-in echo provider (no model, no
network). Point a session at a real model with `--model
<provider>/<model>` (see [Model providers](#model-providers) below).
Add `--thinking low|medium|high` to request a reasoning/thinking budget
from that model (parity with `prime-agent --thinking <level>`; no effect
without `--model`, since `EchoProvider` has no concept of it).

Add `--tools read` to let the model call built-in, read-only tools
(`read_file`, `list_dir` -- plain filesystem access, no path sandboxing)
during a prompt: the model can request one, get the result back, and
continue, looped up to 8 rounds per prompt. Off by default; `EchoProvider`
sessions can set it too, but never actually invoke a tool.

Add `--tools mcp` instead to offer whatever `rp-server`'s own MCP
(Model Context Protocol) gateway exposes -- its native `chat_completion`/
`list_models`/`embeddings` tools, plus any upstream MCP server configured
via `[[mcp.upstreams]]` in `rp-server`'s own config, all namespaced
`"{upstream}/{tool}"`. Needs `rp-server` reachable (same as `--model`) even
without one set. `read` and `mcp` are mutually exclusive for now -- pick one.

```sh
harness session new --model ollama/qwen2.5:0.5b --tools read
harness session new --model ollama/qwen2.5:0.5b --tools mcp
```

Add `--runtime ipython` to give the session a real, persistent IPython
kernel it can run code in -- parity with `prime-agent`'s RLM programming
model. Spawns a real `python3 -m ipykernel_launcher` subprocess (needs
`python3` with `ipykernel` installed: `pip install ipykernel`) and offers
an `execute_python` tool through the same tool-calling loop `--tools`
uses (independent of, and combinable with, `--tools read`/`mcp`): the
model sends code, gets stdout/the last expression's value back, and
variables/imports persist across calls within the session. Off by
default; `EchoProvider` sessions can set it too, but never actually
invoke it. Drop real Python packages into `<state-dir>/skills/` to make
them `import`-able in the kernel -- see "Skills" below. The kernel also
gets a built-in `rlm_heartbeat()` function: calling it (with an `Active`
goal set) schedules an immediate continuation prompt, the kernel-callable
sibling of `session repl`'s own `/heartbeat`. `rlm_heartbeat(every="10m")`
(or `/heartbeat every 10m` in the REPL) schedules a repeating one
instead -- a real `session schedule` entry, listed/canceled the same way
any other one is.

The kernel also gets `rlm(task, name=None, model=None)`, parity with
`prime-agent`'s kernel-callable recursive subagents: `await rlm("review
the API")` admits a real child session (the same underlying mechanism as
`session spawn`, just called from inside the kernel instead of the CLI)
and returns immediately after admission -- `{"rlm_child_id", "name",
"session_dir", "model"}` -- without waiting for the child's answer.
Recursion is bounded: a root session may create children up to
`RUSTY_PRIME_AGENT_RLM_MAX_DEPTH` deep (default `1`, i.e. children may
not create grandchildren unless raised); a child inherits its parent's
own max depth unchanged, and a call past the limit returns
`{"error": "recursion depth limit reached ..."}` instead of admitting a
child. Admitted children are tracked for you: `await
rlm_list_subagents()` lists this session's own direct children --
`[{"child_id", "name", "status", "session_dir"}, ...]` -- and `await
rlm_delete_subagent(id)` (matching by child id or name) gracefully stops
one, leaving its transcript and session directory on disk. Only a
session's own direct children are visible/deletable this way, not
grandchildren or unrelated sessions.

```sh
harness session new --model ollama/qwen2.5:0.5b --runtime ipython
```

## Global flags

- `--mode json|text` (must come first, before the subcommand) -- switches
  every subcommand's output from human-readable text to raw JSON lines.
  Text is the default.

## Command reference

### Daemon lifecycle

```sh
harness daemon start                 # idempotent; spawns a detached supervisor
harness daemon status                # pid, generation, active session count
harness daemon shutdown              # gracefully stops every worker, then exits
```

### Sessions

```sh
harness session new [--name NAME] [--model PROVIDER/MODEL] [--goal TEXT] [--thinking low|medium|high] [--tools read|mcp] [--runtime ipython]
harness session attach <id>          # streams the transcript live
harness session list                 # id, status, name, turns, model, ...
harness session prompt <id> <text...>
harness session stop <id>            # gracefully shuts down one worker
harness session rename <id> <name>
harness session compact <id> [instructions...]   # force compaction now
harness session fork <id> [--at N] [--name NAME]  # copy into a brand-new session
```

Sessions with a real `--model` automatically compact their own context
once it grows past a size threshold -- older turns get folded into a
running summary the model itself writes, without touching the durable
transcript (`session attach` still shows everything). `session compact`
forces it immediately instead of waiting for the automatic trigger,
optionally focused with free-text `instructions` (parity with
`prime-agent /compact [instructions]`). A no-op, not an error, on a
session with no `--model` set (nothing to summarize with) or nothing old
enough to fold away yet.

`session fork` creates a brand-new, fully independent session whose
starting transcript is a copy of `<id>`'s own transcript up through
`--at N` (or the whole thing, if `--at` is omitted) -- bounded parity
with a slice of `prime-agent`'s `/fork` (session-level forking, not
intra-session branching; no `/tree` visualization or active-leaf
switching -- see `ARCHITECTURE.md` for exactly what that distinction
means and why). The new session carries forward the source's `--model`/
`--thinking`/`--tools`/`--runtime` configuration but starts with no goal
and no Continual Harness history, since both would only be accurate
against the source's full history, not necessarily a truncated copy of
it. `--at N` past the source's own last turn is a conflict, not a
silent clamp. Prompting the fork never affects the source session, or
vice versa -- they're two ordinary, unrelated sessions from that point
on.

### Scheduling

Register a prompt the daemon injects into a session later, with no
client attached.

```sh
harness session schedule add <id> (--at TIME|--every DURATION) <text...>
harness session schedule list <id>
harness session schedule cancel <id> <schedule-id>
```

`TIME`/`DURATION` are short strings like `30s`/`5m`/`2h`/`1d`, or (for
`--at`) a raw Unix-epoch-milliseconds integer.

### Persistent goals

```sh
harness session goal set <id> <text...>
harness session goal show <id>
harness session goal pause <id>
harness session goal resume <id>
harness session goal complete <id>
harness session goal clear <id>
```

A goal can also be seeded at creation time with `session new --goal`.

### Bounded autonomous mode

Drives repeated continuation prompts toward a session's active goal
until a turn budget, a time budget, or a quality gate stops it.

```sh
harness session autonomous <id> --max-turns N [--max-time DURATION] [--quality-gate CMD]
```

Requires an existing `Active` goal (`session goal set` first).
`--quality-gate` is an arbitrary shell command; exiting `0` marks the
goal `Complete` and stops the run.

### Prompt templates

Markdown-plus-frontmatter snippets that expand into a full prompt,
discovered from `<state-dir>/prompts/*.md` (global) and
`.rusty-prime-agent/prompts/*.md` (project-local, wins on a name
collision).

```sh
harness prompt-template list
harness prompt-template render <name> [args...]
harness session prompt-template <id> <name> [args...]
```

Template bodies support `$1`/`$2`/... (positional args), `$@`/
`$ARGUMENTS` (all args joined), and `${@:N}`/`${@:N:L}` (a 1-indexed
slice).

### Skills

Real, importable Python packages for `session new --runtime ipython`
sessions. Drop a directory into `<state-dir>/skills/<name>/`: a
`SKILL.md` (`description` frontmatter) alongside a real Python package
(`__init__.py`). The kernel gets the skills directory added to its
`sys.path` on startup, so the model can `import <name>` directly --
`session new --runtime ipython`'s `execute_python` tool description
lists what's installed.

```sh
harness skill list
```

### Context files

Drop an `AGENTS.md` (or `CLAUDE.md`) into the state directory and every
session automatically gets its content as context, on every prompt --
parity with `prime-agent`'s own auto-loaded context files. Checks
`AGENTS.md` first, then `CLAUDE.md` (whichever is found first wins, not
merged); read fresh each time, so an edit takes effect on the very next
prompt with no restart needed. Global only -- no project-local tier, same
cwd-visibility reason `skills` don't have one either.

### Config file (`settings.json`)

Drop a `settings.json` into the state directory to persist a default for
a tunable that's otherwise only a CLI flag or an env var, checked fresh
on every read (no restart needed):

```json
{
  "compact_trigger_tokens": 4000,
  "compact_keep_recent_tokens": 1500
}
```

Currently covers just the two automatic-compaction thresholds above
(parity with `prime-agent`'s own `settings.json`, narrower today). An env
var still wins when both are set; a missing or malformed file is treated
as "no settings" rather than an error. Global only, same cwd-visibility
reason `--runtime ipython` skills and context files don't have a
project-local tier either.

### Provider keys (`auth.json`)

Drop an `auth.json` into the state directory to configure a provider's
API key without exporting a real env var, parity with `prime-agent`'s
own `auth.json`:

```json
{
  "groq": { "key": "sk-a-literal-key" },
  "anthropic": { "key": "!security find-generic-password -w -s my-service" }
}
```

A `key` is either a literal string, used as-is, or a string prefixed
with `!`, whose remainder runs as a shell command (`sh -c`/`cmd /C`);
its trimmed stdout becomes the key. An already-set env var
(`OPENAI_API_KEY`/`ANTHROPIC_API_KEY`/`GEMINI_API_KEY`/`GROQ_API_KEY`)
always wins over an `auth.json` entry for that same provider -- the
command is never even run in that case. `harness model list` (no
`--detailed`) only checks whether an `auth.json` entry *exists*, never
running a `!command` as a side effect of listing; the command only ever
runs when a real `rp-server` sidecar is actually starting up. Global
only, same cwd-visibility reason `settings.json` is.

### Custom providers (`providers.json`)

Drop a `providers.json` into the state directory to point `--model` at
any self-hosted OpenAI-compatible endpoint -- a vLLM server, LM Studio,
a company-internal proxy -- that isn't one of the built-in
`openai`/`anthropic`/`gemini`/`groq`/`ollama` names:

```json
{
  "my-vllm": { "base_url": "http://127.0.0.1:8000/v1" }
}
```

`kind` is optional and defaults to `"openai"`, the right value for any
wire-compatible self-hosted endpoint. The registered name works
everywhere a built-in provider name does -- `session new --model
my-vllm/<model>`, `harness model list`/`--detailed` -- and its API key
is supplied the same way a built-in provider's is: an env var (derived
as `<NAME>_API_KEY`, e.g. `MY_VLLM_API_KEY`), or an `auth.json` entry
keyed by the same name (see above). A custom entry reusing a reserved
name (a built-in provider's own name, or `ollama`) is silently ignored.
Global only, same cwd-visibility reason `settings.json`/`auth.json` are.

```sh
harness session new --model my-vllm/some-model-id
```

### The Continual Harness

Durable supplemental notes (prompts, memories, skill descriptions) a
session can accumulate and roll back.

```sh
harness session harness add <id> prompt|memory|skill <text...>
harness session harness list <id>
harness session harness rollback <id> <history-index>
harness session refine <id>          # reviews the trajectory, proposes one small update
```

Every `add`/`rollback` is recorded in history, so a rollback is itself
auditable rather than destructive.

### Recursive subagents

```sh
harness session spawn <parent-id> [--model PROVIDER/MODEL] [--name NAME] <task text...>
harness session children <id>
harness session message <from-id> <to-id> <text...>
```

`session spawn` creates a child session (inheriting the parent's model
unless overridden) and enqueues the task text as a near-immediate
schedule -- it returns right away, without waiting for the child's
answer. `session message` only allows a session to message its own
parent or one of its own children.

### Interactive REPL

```sh
harness session repl <id>
```

Reads lines from stdin, sends each as a prompt, prints the reply, until
EOF or a line that's exactly `/exit`/`/quit`. Replays the session's
existing transcript first. A line that's exactly `/heartbeat` manually
re-enters the session's `Active` goal right away (`"Continue working
toward the goal: ..."`) -- with no active goal, it prints an explanation
and sends nothing. `/heartbeat every <duration>` (e.g. `/heartbeat every
10m`) registers a real recurring `session schedule` entry instead of
sending anything immediately -- list or cancel it with `session schedule
list`/`cancel`, same as any other schedule. A line that's `/compact` or
`/compact <instructions>` forces context compaction immediately, same as
`session compact`.

`/file <path>` reads a local file and includes its content in your
*next* prompt (queued across an intervening `/heartbeat`/`/compact`/
`/fork` line, not dropped) -- a bounded slice of a TUI's file-reference
feature, no client-side attachment UI, just folding the file's content
into the next ordinary prompt text. `/fork [--at N] [--name TEXT]` is
`session fork` wired into the REPL loop, same as `session compact` is
via `/compact`. `/export <path>` writes the session's current
transcript to a local file as pretty-printed JSON.

### RPC mode

```sh
harness session rpc <id>
```

Headless, embeddable operation over stdin/stdout -- parity with
`prime-agent --mode rpc`. Reads one JSON `Request` per stdin line (the
same wire type `--mode json` already exposes -- `session_prompt`,
`session_compact`, `goal_update`, `schedule_add`, ...), dispatches it,
and prints the `Response` as its own JSON line. Concurrently streams
every live `SessionEvent` for that session, the same event stream
`--mode json session attach` produces, without needing a second CLI
invocation. Ends at stdin EOF. `session_attach` is rejected as a
command -- this mode already streams events automatically.

```sh
echo '{"type":"session_prompt","session_id":"<id>","text":"hello"}' | harness session rpc <id>
```

### Model providers

```sh
harness model list
harness model list --detailed
```

Lists which providers (`openai`/`anthropic`/`gemini`/`groq`/`ollama`)
this process's environment currently has configured (see
[Environment variables](#environment-variables)). `ollama` is always
listed as configured -- it needs no API key.

`--detailed` starts (or reuses) an `rp-server` sidecar and lists its real
per-model catalog instead (id, owning provider, context length) -- needs
`rp-server` on `PATH`, unlike the plain listing above.

## Embedding as a library

`harness` is one binary built on top of the `rusty_prime_agent` library
crate (`Cargo.toml` has both a `[lib]` and a `[[bin]]` target); an
external Rust program can depend on the same crate directly, with two
embedding layers to pick from:

```rust
// No daemon at all -- a real, driveable session in-process.
use rusty_prime_agent::provider::EchoProvider;
use rusty_prime_agent::session::{AgentSession, NewSessionMeta};
use rusty_prime_agent::tool_runtime::NoopToolRuntime;

let mut session = AgentSession::create(
    state_root,
    "my-session".to_string(),
    NewSessionMeta::default(),
    Box::new(EchoProvider),
    Box::new(NoopToolRuntime),
)
.await?;
let reply = session.prompt("hello".to_string()).await?;
```

```rust
// Drive an already-running daemon instead.
use rusty_prime_agent::protocol::{Request, Response};

let response = rusty_prime_agent::dispatch_one_shot(
    state_root,
    Request::SessionList,
)
.await?;
```

Implement `provider::ModelProvider`/`tool_runtime::ToolRuntime` yourself
to plug in a custom model backend or tool/code-execution environment --
both are plain `pub trait`s already used exactly that way internally, no
separate registration step needed. See `ARCHITECTURE.md`'s "Embeddable
SDK" section for the full design, including what's deliberately kept
crate-internal (`daemon`/`worker`/`ipython_runtime`/`zmtp`).

## Environment variables

| Variable | Effect |
| --- | --- |
| `RUSTY_PRIME_AGENT_HOME` | Overrides the state-root directory (sockets, session state). Defaults to a per-OS state directory. |
| `RUSTY_PRIME_AGENT_MODEL` | Default `--model` for `session new` when no explicit flag is given. |
| `RUSTY_PRIME_AGENT_RP_SERVER_BIN` | Path/name of the `rp-server` binary (default: `rp-server` on `PATH`). |
| `RUSTY_PRIME_AGENT_OLLAMA_BASE_URL` | Base URL for the Ollama provider (default: `http://127.0.0.1:11434/v1`). |
| `RUSTY_PRIME_AGENT_IPYTHON_BIN` | Path/name of the Python interpreter `--runtime ipython` spawns (default: `python3`, or `python` on Windows). Must have `ipykernel` installed. |
| `RUSTY_PRIME_AGENT_RLM_MAX_DEPTH` | Max `rlm(...)` recursion depth for a root session (default: `1`). Children inherit their parent's own resolved value unchanged. |
| `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `GROQ_API_KEY` | Activate the matching provider when set. |

Setting a `--model` on `session new` (directly, or via
`RUSTY_PRIME_AGENT_MODEL`) starts a local `rp-server` sidecar the first
time it's needed, routing that session's prompts to the real provider
instead of the built-in echo provider.

## More detail

- [`ARCHITECTURE.md`](ARCHITECTURE.md) -- module map, IPC model, on-disk
  layout, dependency stack.
- [`PARITY.md`](PARITY.md) -- what's mirrored from `prime-agent`, what's
  a bounded/simplified version of a larger feature, and what's
  genuinely not yet implemented for this project's shape.
