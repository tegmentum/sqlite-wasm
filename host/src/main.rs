//! `sqlite-wasm-run` — reference command-mode runner for
//! `sqlite-cli-unified`-world components.
//!
//! Equivalent to `wasmtime run <component>` for SQLite-in-WebAssembly
//! binaries, but additionally provides the `sqlite:wasm/extension-
//! loader` interface so the in-WASM `.load` command can route file
//! reads + policy enforcement out to the host instead of asking the
//! sandboxed SQLite to do it.
//!
//! Usage:
//!     sqlite-wasm-run <path-to-component.wasm> [-- args...]
//!
//! For composed binaries that already have every slot wired (e.g.
//! `sqlite-cli-demo.wasm` from `make cli-demo-test`), the host
//! provides only WASI + extension-loader. For binaries with
//! unsatisfied slot imports, an additional `--plug ext.wasm` flag (a
//! follow-up) would let this binary do the composition at start
//! time instead of requiring a separate `wac plug` step.

use std::env;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use wasmtime::component::{Component, Linker};
use wasmtime::Store;
use wasmtime_wasi::{WasiCtxBuilder, ResourceTable};

use sqlite_wasm_host::{bindings, HostWrap, LoaderData, Host};

/// HostState passed to wasmtime; carries WASI resources + a reference
/// to the loader so the (future) extension-loader host functions can
/// hand off loads to the shared registry.
struct State {
    wasi: wasmtime_wasi::WasiCtx,
    resources: ResourceTable,
    host: Host,
}

impl wasmtime_wasi::WasiView for State {
    fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
        wasmtime_wasi::WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.resources,
        }
    }
}

fn usage() -> ! {
    eprintln!("usage: sqlite-wasm-run <component.wasm> [-- guest-args...]");
    std::process::exit(2);
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 || args[1] == "-h" || args[1] == "--help" {
        usage();
    }

    let component_path = PathBuf::from(&args[1]);
    let guest_args: Vec<String> = args.iter().skip_while(|a| *a != "--").skip(1).cloned().collect();

    let host = Host::new()?;
    let engine = host.engine().clone();

    let component_bytes = std::fs::read(&component_path)
        .map_err(|e| anyhow!("read {}: {e}", component_path.display()))?;
    let component = Component::from_binary(&engine, &component_bytes)
        .map_err(|e| anyhow!("compile component: {e}"))?;

    let mut linker: Linker<State> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
        .map_err(|e| anyhow!("wire WASI: {e}"))?;

    // Wire the sqlite:wasm/extension-loader interface so the in-WASM
    // CLI's `.load /path/ext.wasm` command can route through here
    // when wired on the wasm side (currently the in-WASM CLI calls
    // sqlite3_load_extension directly; routing it through the WIT
    // interface is the matching change on the sqlite-wasm side).
    bindings::sqlite::wasm::extension_loader::add_to_linker::<_, LoaderData>(
        &mut linker,
        |state: &mut State| HostWrap { host: &mut state.host },
    )
    .map_err(|e| anyhow!("wire extension-loader: {e}"))?;

    // Build the WASI context: pass guest args through, inherit stdio,
    // inherit env vars so DEBUG flags and the like flow through.
    let mut wasi_builder = WasiCtxBuilder::new();
    wasi_builder.inherit_stdio();
    wasi_builder.inherit_env();
    // wasi:cli expects argv[0] to be the program name. Match
    // `wasmtime run` behavior.
    let argv0 = component_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("component");
    wasi_builder.arg(argv0);
    for a in &guest_args {
        wasi_builder.arg(a);
    }

    let state = State {
        wasi: wasi_builder.build(),
        resources: ResourceTable::new(),
        host,
    };

    let mut store = Store::new(&engine, state);
    // Generous defaults — match sqlite-wasm-loader's behavior so this
    // host runs CLI binaries that don't set Policy explicitly.
    store.set_fuel(u64::MAX / 2)?;
    store.set_epoch_deadline(1_000_000_000_000);

    // Instantiate as a wasi:cli/command-mode component. The wasmtime
    // command crate handles the entry-point lookup + run invocation.
    let command = wasmtime_wasi::p2::bindings::sync::Command::instantiate(
        &mut store, &component, &linker,
    )
    .map_err(|e| anyhow!("instantiate: {e}"))?;

    let result = command
        .wasi_cli_run()
        .call_run(&mut store)
        .map_err(|e| anyhow!("wasi:cli/run.run: {e}"))?;

    match result {
        Ok(()) => Ok(()),
        Err(()) => Err(anyhow!("component exited with error")),
    }
}
