# sqlite-wasm

Componentized SQLite for WebAssembly. The whole SQLite C
library compiled to wasm32-wasip2, plus a small set of crates
exposing it as WebAssembly Components.

This is the building-block layer. Higher-level applications
(notably [sqlink](https://github.com/tegmentum/sqlink), a cli +
extension catalog + dot-command surface) consume it as a git
submodule.

## Crates

| Crate | Purpose |
|---|---|
| `core` (`sqlite-component-core`) | Rust wrapper around libsqlite3-sys: `Connection`, `Statement`, `Value`, prepared-stmt API. |
| `sqlite-lib` | wasm component that exports `sqlite:extension/spi` and friends — drop into any wasm host that wants SQL via composition. |
| `sqlite-embed` | Static-embed helpers for native Rust extensions (the C-side `sqlite3_create_function_v2` glue). |
| `sqlite-pcache-tvm` / `sqlite-mem-tvm` / `sqlite-vfs-tvm` | Multi-memory cold-tier providers: SQLite's pcache + VFS bytes routed through pool 1 / pool 2 of the `tvm-guest-mm` shell (see [tvm-wasm](https://github.com/tegmentum/tvm-wasm)). |

## Submodule

| Path | Source |
|---|---|
| `sqlite-loader-wit/` | The WIT contract every extension speaks. Pinned via submodule; lives at https://github.com/tegmentum/sqlite-loader-wit. |

## Build

First-time setup (writes per-crate `.cargo/config.toml` from
`.cargo/config.toml.template`, with `__WASI_SDK_PATH__`
substituted):

```sh
WASI_SDK_PATH=$HOME/wasi-sdk bash scripts/setup-cargo-config.sh
```

Then:

```sh
cargo build --release              # native (just `core`)
cargo build -p sqlite-lib \
            --target wasm32-wasip2 \
            --release              # the SPI-providing component
```

## History

Extracted from [sqlink](https://github.com/tegmentum/sqlink)
(formerly the `sqlite-wasm` monorepo) so projects that just
need componentized SQLite don't have to pull in sqlink's cli +
extension catalog. The pre-split history lives in sqlink under
the `pre-split` tag.

## License

SQLite itself is public domain. The wrapping code in this repo
(crates listed above) is MIT.
