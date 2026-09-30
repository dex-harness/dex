# dex-runtime

The DEX runtime: a standalone process that owns model access, program execution,
capabilities, authorization, memory, and the IPC server.

A frontend (see [`dex-cli`](https://github.com/dex-harness/dex-cli)) never
executes a repository operation. It connects over a Unix socket, sends a user
message, and renders the structured events that come back.

## The core idea

The model does not call tools. **The model programs the runtime.**

```text
User
 ↓
Model
 ↓
Generated program  (Rune)
 ↓
Script runtime
 ↓
DEX capability API   (repo / git / filesystem / testing / memory / ui)
 ↓
Rust runtime         (authorization + budgets enforced here)
 ↓
Result
 ↓
Model
```

A single program can do `discover → inspect → filter → modify → test → summarize`
with no model round-trip between steps. There is no model-facing tool-call
schema, no `tools` array is ever sent to a provider, and no shell is exposed to
the model.

## Layout

```text
crates/
├── dex-protocol/     Wire types shared with frontends. serde only, no tokio.
└── dex-runtime/      Everything else. Produces the `dexd` binary.
```

`dex-protocol` is the single source of truth for anything that crosses a process
boundary. It is deliberately free of tokio and of runtime internals so a
frontend can depend on it without pulling in the runtime.

## Configuration

Copy `.env.example` to `.env` and fill in an API key. `.env` is gitignored.

The one setting that decides whether anything works is `DEX_AUTHORITY`: authority
is **deny by default**, and a capability runs only when a matching grant exists
and the requested resource matches the grant's scope. The example file documents
a development grant for a repository at `/work`.

## Running

```bash
cargo build --release
./target/release/dexd              # binds ~/.dex/dex.sock
```

In another terminal, from a checkout of `dex-cli`:

```bash
dex
```

`dexd` does not spawn or supervise frontends, and `dex` never spawns the
runtime. If the socket is missing, the CLI says so rather than quietly starting
one, because a hidden spawn would hide the boundary this project exists to
demonstrate.

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
```

Two structural invariants are enforced by tests rather than by convention:

- **No tool calls.** A test asserts the outgoing provider request body contains
  no `tools` field.
- **The scripting language is an implementation detail.** Only
  `script/rune/*` may import `rune`; a test asserts no other module does. The
  `ScriptRuntime` trait is the seam a future language replaces.

## Known limitation

Rune exposes no instruction or operation budget, so a program that loops
forever without ever awaiting cannot be preempted mid-instruction. Execution
limits are enforced instead by a wall-clock deadline around the driving future,
a capability-invocation budget, and filesystem read/write budgets, all checked in
the native runtime. This is deliberate and documented rather than hidden.
