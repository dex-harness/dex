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

## How a program is written

A program is a Rune module, so it holds declarations and has exactly one entry
point, `main`. The value `main` returns is the program's result.

```rune
pub fn main() {
    let found = dex::find("authenticate");
    let names = [];
    for m in found["matches"] {
        if m["text"].contains("token") {
            names.push(m["path"]);
        }
    }
    names
}
```

Three properties of the language shape that API, and each is enforced by a test
rather than assumed:

- **Fixed arity.** Rune has no optional arguments, so capabilities are named for
  the case they serve — `dex::find(q)` beside `dex::find_in(q, path)` — rather
  than taking a bag of defaults.
- **No `try`/`catch`.** A capability failure ends the program with a precise
  reason, which the runtime hands back to the model so a corrected program can
  follow. The adjustment happens where the decision is made.
- **No ambient authority.** The Rune context is built with stdio disabled, so
  `println!` does not resolve. The only reachable operations are the ones
  registered in the `dex` module, and the VM has no way to reach the filesystem,
  the network, or a process except through the capability layer that
  authorizes each call.

## Execution limits

Each program runs under a budget: a wall-clock deadline, an **instruction**
budget enforced by Rune's own per-instruction counter, a capability-invocation
count, and separate filesystem read/write and output-size limits.

The instruction budget is what makes a tight compute-only loop stoppable. A
wall clock alone cannot interrupt a program that never yields, so the two cover
each other: the instruction budget catches a loop, and the deadline catches a
program that is making progress but slowly. Cancellation is a separate signal
that propagates to the model request, a running test process, and the program.

## Known limitation

`DEX_BUDGET_INSTRUCTIONS` is a best-effort bound rather than a hard one. Rune
notes that a budget cannot be enforced without cooperation from native
functions, so a capability that spends a long time inside Rust is bounded by the
byte and deadline limits rather than by instructions. That is deliberate: the
capability layer is the authoritative boundary, and the language-level counter
is a second line of defence.

