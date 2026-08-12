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
daemon/worker operational architecture, plus a bounded, non-Python
subset of its higher-level features (scheduling, persistent goals,
bounded autonomous mode, prompt templates, the Continual Harness,
recursive subagents, a minimal REPL, model catalog listing) -- without
attempting to reimplement `prime-agent` itself. See
[`PARITY.md`](PARITY.md) for exactly what's mirrored, what's a
deliberate simplification, and what's out of scope for this project's
current shape, and [`ARCHITECTURE.md`](ARCHITECTURE.md) for how the
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
harness session new [--name NAME] [--model PROVIDER/MODEL] [--goal TEXT] [--thinking low|medium|high] [--tools read|mcp]
harness session attach <id>          # streams the transcript live
harness session list                 # id, status, name, turns, model, ...
harness session prompt <id> <text...>
harness session stop <id>            # gracefully shuts down one worker
harness session rename <id> <name>
```

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
existing transcript first.

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

## Environment variables

| Variable | Effect |
| --- | --- |
| `RUSTY_PRIME_AGENT_HOME` | Overrides the state-root directory (sockets, session state). Defaults to a per-OS state directory. |
| `RUSTY_PRIME_AGENT_MODEL` | Default `--model` for `session new` when no explicit flag is given. |
| `RUSTY_PRIME_AGENT_RP_SERVER_BIN` | Path/name of the `rp-server` binary (default: `rp-server` on `PATH`). |
| `RUSTY_PRIME_AGENT_OLLAMA_BASE_URL` | Base URL for the Ollama provider (default: `http://127.0.0.1:11434/v1`). |
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
  genuinely out of scope for this project's shape.
