# SPI in `sqlite-wasm-host`

## TL;DR

The host's `LoadedState` stubs out every `sqlite:extension/spi`,
`prepared`, `transaction`, and `schema` method. A dynamically-loaded
extension that calls `spi.execute("SELECT count(*) FROM t")` gets back
a structured `"not implemented in dispatch host"` error. This document
explains *why* — the limitation is architectural, not a missing
afternoon of work.

## The picture

Three components are alive when a dynamically-loaded extension's
`scalar-function.call` runs:

```
+----------------------------+      +-------------------+      +------------------------+
| sqlite-cli-demo.wasm       |      | sqlite-wasm-run   |      | test_extension.wasm    |
| (SQLite-in-WASM + CLI;     |      | (Rust binary;     |      | (loaded ext;           |
|  command-mode, has main()) |      |  embeds wasmtime) |      |  reactor)              |
+----------------------------+      +-------------------+      +------------------------+
            |                              ^   |                        ^
            |    .load /ext.wasm           |   |  dispatch.scalar-call  |
            |   (extension-loader)         |   |    (cross-component)   |
            +------------------------------+   +------------------------+
```

`sqlite-cli-demo.wasm` *contains* a real SQLite (compiled into the
binary). When SQL inside it invokes `wasm_dyn_xfunc`, the
trampoline calls `dispatch.scalar-call` and the host (the middle box)
instantiates `test_extension.wasm` for the duration of the call.

Now suppose the loaded extension does this in its `call` body:

```rust
fn call(_id: u64, _args: Vec<SqlValue>) -> Result<SqlValue, String> {
    let r = sqlite::extension::spi::execute("SELECT 42", &[])?;
    Ok(r.rows[0][0].clone())
}
```

`spi.execute` is an *import* from `test_extension.wasm`'s perspective.
Wasmtime asks our `LoadedState` SPI Host impl to satisfy it. And the
question becomes: *which* SQLite does the host run that SQL against?

There is no native rusqlite here. The only SQLite is inside the
command-mode `sqlite-cli-demo.wasm` — and that component is
*currently busy*. Its execution stack is:

```
_start → main → repl → fgets → ... → wasm_dyn_xfunc → dispatch.scalar-call (BLOCKED waiting on host)
```

The host can't call into `sqlite-cli-demo.wasm` to run SQL because
`sqlite-cli-demo.wasm` is on the stack, waiting for the host to
return. The wasmtime engine is busy with one wasm computation —
suspending and re-entering it cooperatively is what async wasmtime
would allow, but that's a separate runtime model.

## What it would take to make this work

Two architectural directions, both substantial:

### Option A: Convert `sqlite-cli-demo.wasm` to a reactor

Reactor components don't have a `main` that drives a REPL. They
export named functions the host calls. The host owns the control
loop, calls `cli.eval_sql("...")`, drains output, repeats.

Then "the host's SPI" can call `cli.eval_sql(...)` from within
`dispatch.scalar-call` because the host *is* the driver — the CLI
isn't holding the stack, the host is.

**Cost.** Rewriting the CLI: REPL becomes a host-side driver, dot
commands become host-side handlers, SQL becomes a host-side fetch
loop. The in-WASM side keeps only the SQLite library + the
extension wiring, no main, no fgets, no stdio dance. That's
weeks. It also removes one of the appealing properties of mode 3 —
that `wasmtime run sqlite-cli-demo.wasm` Just Works for users
without involving the reference host.

### Option B: Async wasmtime + cooperative scheduling

Use `wasmtime::component::Linker`'s async APIs and `WasiCtxBuilder::
build_async`. Inside the SPI Host impl, the host can:

1. Hold the loaded extension's call suspended
2. Re-enter `sqlite-cli-demo.wasm` to run the SQL
3. Return to the loaded extension with the result

**Cost.** All of `bindings.rs` switches from sync to async Host
traits. `wasi-cli/run` becomes an async future. The host binary
becomes Tokio-flavored. The mental model gets harder. And there's
a subtlety: re-entering a wasm component while *another* call on it
is on the stack requires that component to be re-entrant — which
`wasi:cli/run`-style components generally aren't.

## Native loader (`sqlite-wasm-loader`)

For the native loader case the equivalent problem doesn't exist —
there's no command-mode-component-with-its-own-SQLite-on-the-stack.
The host owns a real `rusqlite::Connection`, and the SPI Host impl
can route directly to it. See `runtimes/wasmtime/src/bindings.rs`
on the loader side — `spi.execute` / `transaction.begin` /
`schema.list_tables` are all real impls.

## The decision

Today: leave `LoadedState`'s SPI methods stubbed. Document the gap
(this file). The architecture for mode 3 (in-WASM dynamic `.load`)
is still useful for *pure-compute* extensions — anything that
doesn't need to read or write its embedding database — and the
unified WIT contract continues to ensure binary portability with
mode 1.

If/when the in-WASM SPI access becomes a real requirement, Option B
(async + re-entrancy) is the lower-cost path: the CLI's interactive
shape survives, the host gains a real responsibility (cooperative
scheduling) but doesn't have to re-implement REPL semantics. Worth
prototyping with one tiny extension before committing to the full
switch.

## What works today

Mode 3 extensions can use:

- `sqlite:extension/types` — record types, no host call needed
- `sqlite:extension/logging` — `LoadedState` routes to stderr
- `sqlite:extension/config` — version reporting, no SQL needed
- `sqlite:extension/random` (when implemented, same shape — no
  cross-component issue)
- `sqlite:extension/text` / `hashing` / `encoding` (same — pure
  compute, no SQL needed)

What's blocked: `spi`, `prepared`, `transaction`, `schema`, `state`
(if it persists to the embedding DB), `http` (depends on whether
the host wants to provide a real http impl independent of the
in-WASM SQLite).
