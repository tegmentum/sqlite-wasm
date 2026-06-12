# sqlite-wasm-host

Reference wasmtime-based host for `sqlite-cli-unified`-world components.

## What it does

- **Library (`sqlite_wasm_host`)** — Rust API a host application embeds to run
  SQLite-in-WebAssembly components. Provides:
    - A pre-configured `Engine` (component-model, fuel, epoch interruption,
      cranelift speed) and a background epoch-bumper thread
    - A `Host` registry that holds loaded extension components, indexed by
      name, retained for dispatch
    - A `Policy` type mirroring `sqlite-wasm-loader`'s — same `Capability`
      variant, same wildcard-suffix `HttpPolicy`, same fuel/memory/epoch
      knobs — so values port directly between the native loader and this
      in-WASM host
- **Binary (`sqlite-wasm-run`)** — drop-in replacement for `wasmtime run` for
  SQLite-in-WebAssembly components. Wires WASI Preview 2 (stdio inherited,
  env inherited, argv passed through), instantiates the component as a
  `wasi:cli/command`, calls `run`.

## Verified

```sh
echo "SELECT wasm_reverse('hello'), wasm_double(21);" \
  | host/target/aarch64-apple-darwin/release/sqlite-wasm-run \
      build/sqlite-cli-demo.wasm
→ SQLite version 3.53.1
  sqlite> olleh|42
```

Same output as `wasmtime run build/sqlite-cli-demo.wasm` — the reference
host is a drop-in.

## Architecture cross-link

`sqlite-wasm-host` is the host half of the **dynamic-load deployment** for
SQLite-in-WebAssembly. The architecture has three deployment modes
sharing the same `sqlite:extension` WIT contract:

| Mode | Loader | Where SQLite lives | Where extension lives |
|---|---|---|---|
| Native + WASM extension | [`sqlite-wasm-loader`] | host process | `.wasm` loaded at runtime |
| SQLite-in-WASM + composed extension | wac plug at build time | `.wasm` component | `.wasm` component, statically linked |
| SQLite-in-WASM + dynamic `.load` | this crate | `.wasm` component | `.wasm` component, loaded at runtime via the host's `extension-loader` impl |

[`sqlite-wasm-loader`]: https://github.com/tegmentum/sqlite-wasm-loader

The same `Policy` value type works across all three. The same
`metadata.describe()` produces the manifest in all three. The same
`scalar-function.call(func-id, args)` is how scalar functions are
dispatched in all three.

## What's plumbed today vs. what's next

**Today:**
- `Host::new()` — engine + epoch-bumper
- `Host::load_extension(path, policy)` — reads the file, compiles
  the component, stashes it in the registry alongside its Policy
- `Host::unload(name)`, `Host::list()`, `Host::is_loaded(name)`
- `sqlite-wasm-run` — runs any `wasi:cli/command`-style component
  with WASI fully provided

**Next iteration** — surfacing `Host::load_extension` to the in-WASM CLI
via the `sqlite:wasm/extension-loader` WIT interface:

  1. `wasmtime::component::Linker::root().instance("sqlite:wasm/extension-loader@0.1.0").func_wrap(...)`
     entries for `load-extension` / `unload-extension` / `list-extensions`
     / `is-extension-loaded`, each routing to the methods on `Host`.
  2. The in-WASM CLI's `.load /path/to/ext.wasm` handler updated to call
     this WIT import (currently the legacy CLI has placeholder code that
     doesn't route through any WIT interface).
  3. Host-side dispatch: when loaded extensions' scalar functions get
     invoked via SQL inside the wasm, the host needs to route the call
     across components. This is the substantive design piece — equivalent
     to `sqlite-wasm-loader`'s `wasm_ext_xfunc` trampoline but living
     outside the wasm component instead of inside it.

## Building

```sh
cargo build --manifest-path host/Cargo.toml --release --target aarch64-apple-darwin
```

The repo's `.cargo/config.toml` defaults to `wasm32-wasip2` for the
amalgamation builds, so the `--target` flag is required when building
the host (which targets the platform that *runs* the wasm).
