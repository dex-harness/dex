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

Several capabilities in one program is the normal case. Searching, reading,
filtering, editing and testing happen inside a single program, because each model
round trip costs a turn and a pile of context.

## Running it

```bash
cp .env.example .env      # add OPENCODE_GO_API_KEY and a DEX_AUTHORITY grant
cargo run --release --bin dexd
```

`dexd --check` prints the effective configuration, including exactly which
authorities were granted, without starting anything.

In another terminal, from a checkout of `dex-cli`:

```bash
dex --cwd /path/to/a/repo
dex --cwd /path/to/a/repo -p "find the auth implementation"
```

`dex` will not start the runtime for you. If the socket is missing it says so,
because quietly spawning `dexd` would hide the boundary these two repositories
exist to demonstrate.

## How a program is written

A program is a Rune module: declarations with exactly one entry point, `main`.
The value `main` returns is the program's result.

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
  registered in the `dex` module, and the VM has no route to the filesystem, the
  network, or a process except through the layer that authorizes each call.

## Capabilities and authority

There is no `exec`, no `shell`, and no general command runner. `testing.run` is a
table of allowlisted tools with allowlisted arguments, and `git.checkout` refuses
a branch name git would read as an option. Arguments are passed as argv, never
through a shell, so there is nothing to quote and no metacharacter to blacklist.

Authority is **deny by default**. A capability runs only when a grant names it
*and* the resource matches the grant's scope:

```
DEX_AUTHORITY=filesystem.read=/work/**;filesystem.write=/work/**;memory.write=*
```

Enforcement is in Rust below the script runtime, so a generated program can ask
but cannot widen. Path containment is a separate check and both must pass: it
compares canonicalized paths, so a symlink pointing out of the tree fails while
one staying inside succeeds.

## Execution limits

Each program runs under a wall-clock deadline, an **instruction** budget from
Rune's own per-instruction counter, a capability-invocation count, and separate
filesystem read/write and output-size limits.

The instruction budget is what makes a tight compute-only loop stoppable. A wall
clock alone cannot interrupt a program that never yields, so the two cover each
other: the instruction budget catches a loop, and the deadline catches a program
making progress slowly. Cancellation is a third signal, and it reaches the model
request, a running test process, and the program.

`DEX_BUDGET_INSTRUCTIONS` is best-effort rather than a hard bound. Rune notes that
a budget cannot be enforced without cooperation from native functions, so a
capability that spends a long time inside Rust is bounded by the byte and deadline
limits instead. The capability layer is the authoritative boundary; the language
counter is a second line of defence.

## Memory

`dex::remember(key, program)` stores a reusable procedure; `dex::recall(key)`
loads one. Records are file-backed JSON under `~/.dex/memory`, and a program
remembered in one turn can be reloaded in a later one. Loading counts itself, so
the number a program sees answers "how well has this held up" rather than being
permanently one behind.

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
```

`crates/dex-runtime/tests/end_to_end.rs` starts a real runtime on a real socket
and drives it with the real protocol. The only substitution is the model
provider, which is scripted, so the suite is deterministic and needs no network.

Two structural invariants are enforced by tests rather than by convention:

- **No tool calls.** A test asserts the outgoing provider body contains no
  `tools`, `functions` or `tool_choice` field.
- **The scripting language is an implementation detail.** Only `script/rune/*`
  may import `rune`; everything else is written against the `ScriptRuntime`
  trait, so swapping the language touches one construction site.
